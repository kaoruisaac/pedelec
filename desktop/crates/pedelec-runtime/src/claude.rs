use crate::jsonl::{JsonLineChannel, JsonLineError, JsonLineEvent};
use crate::owner::ProviderRuntimeController;
use crate::persistent_process::{
    PersistentProcess, PersistentProcessError, PersistentProcessSpec, PersistentWriter,
    ProcessExitKind, ProcessGeneration,
};
use crate::protocol::{ProtocolTrafficLogger, ProtocolTrafficRecord};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvError, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const DEFAULT_CLAUDE_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ClaudeReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeRuntimeLaunchConfig {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub model: Option<String>,
    pub effort: Option<ClaudeReasoningEffort>,
    pub resume_session_id: Option<String>,
    pub host_instructions: String,
    pub max_frame_bytes: usize,
}

impl ClaudeRuntimeLaunchConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            env: Vec::new(),
            model: None,
            effort: None,
            resume_session_id: None,
            host_instructions: String::new(),
            max_frame_bytes: DEFAULT_CLAUDE_MAX_FRAME_BYTES,
        }
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_effort(mut self, effort: Option<ClaudeReasoningEffort>) -> Self {
        self.effort = effort;
        self
    }

    pub fn with_resume_session_id(mut self, resume_session_id: Option<String>) -> Self {
        self.resume_session_id = resume_session_id;
        self
    }

    pub fn with_host_instructions(mut self, host_instructions: impl Into<String>) -> Self {
        self.host_instructions = host_instructions.into();
        self
    }

    pub fn process_spec(&self) -> PersistentProcessSpec {
        let mut args = vec![
            OsString::from("-p"),
            OsString::from("--input-format"),
            OsString::from("stream-json"),
            OsString::from("--output-format"),
            OsString::from("stream-json"),
            OsString::from("--include-partial-messages"),
            OsString::from("--verbose"),
            OsString::from("--dangerously-skip-permissions"),
        ];
        if let Some(session_id) = self
            .resume_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args.push(OsString::from("--resume"));
            args.push(OsString::from(session_id));
        }
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
        if let Some(effort) = self.effort {
            args.push(OsString::from("--effort"));
            args.push(OsString::from(effort.as_str()));
        }
        args.push(OsString::from("--append-system-prompt"));
        args.push(OsString::from(&self.host_instructions));

        #[cfg(windows)]
        let mut spec = {
            let extension = self
                .program
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(extension.as_str(), "cmd" | "bat") {
                PersistentProcessSpec::new("cmd.exe")
                    .args(["/d", "/c", "call"])
                    .arg(self.program.as_os_str())
                    .args(args)
                    .cwd(self.cwd.clone())
            } else {
                PersistentProcessSpec::new(self.program.clone())
                    .args(args)
                    .cwd(self.cwd.clone())
            }
        };
        #[cfg(not(windows))]
        let mut spec = PersistentProcessSpec::new(self.program.clone())
            .args(args)
            .cwd(self.cwd.clone());

        spec = spec
            .env_remove("PEDELEC_THREAD_ID")
            .env_remove("PEDELEC_WORKSPACE_PATH");
        for (key, value) in &self.env {
            spec = spec.env(key.clone(), value.clone());
        }
        spec
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeRuntimeEvent {
    SessionReady {
        session_id: String,
    },
    TurnStarted {
        local_turn_id: String,
    },
    AssistantDelta {
        local_turn_id: String,
        text: String,
    },
    AssistantMessage {
        local_turn_id: String,
        text: String,
    },
    UsageUpdated {
        local_turn_id: String,
        usage: Value,
    },
    TurnCompleted {
        local_turn_id: Option<String>,
        status: String,
        success: bool,
        error: Option<Value>,
        session_id: Option<String>,
    },
    Stderr {
        text: String,
    },
    ProtocolError {
        local_turn_id: Option<String>,
        message: String,
    },
    Disconnected {
        generation: ProcessGeneration,
        pid: u32,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeRuntimeError {
    RuntimeStart(String),
    Busy,
    ShuttingDown,
    Write(String),
}

impl fmt::Display for ClaudeRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeStart(error) => write!(f, "Claude runtime start failed: {error}"),
            Self::Busy => write!(f, "Claude runtime already has an active operation"),
            Self::ShuttingDown => write!(f, "Claude runtime is shutting down"),
            Self::Write(error) => write!(f, "Claude stdin write failed: {error}"),
        }
    }
}

impl std::error::Error for ClaudeRuntimeError {}

impl From<PersistentProcessError> for ClaudeRuntimeError {
    fn from(value: PersistentProcessError) -> Self {
        Self::RuntimeStart(value.to_string())
    }
}

impl From<JsonLineError> for ClaudeRuntimeError {
    fn from(value: JsonLineError) -> Self {
        Self::RuntimeStart(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveOperationKind {
    Prepare,
    UserTurn,
}

#[derive(Debug, Clone)]
struct ActiveOperation {
    kind: ActiveOperationKind,
    local_turn_id: Option<String>,
}

#[derive(Debug)]
struct ControllerState {
    session_id: Option<String>,
    active: Option<ActiveOperation>,
    fatal: bool,
    shutdown: bool,
}

#[derive(Debug)]
pub struct ClaudeStreamController {
    thread_id: String,
    process: Arc<PersistentProcess>,
    writer: PersistentWriter,
    state: Arc<Mutex<ControllerState>>,
    events: Arc<Mutex<mpsc::Receiver<ClaudeRuntimeEvent>>>,
    traffic: Arc<Mutex<mpsc::Receiver<ProtocolTrafficRecord>>>,
    event_tx: mpsc::Sender<ClaudeRuntimeEvent>,
    traffic_tx: mpsc::Sender<ProtocolTrafficRecord>,
    protocol_logger: ProtocolTrafficLogger,
    operation_gate: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
}

impl ClaudeStreamController {
    pub fn spawn(
        thread_id: impl Into<String>,
        config: ClaudeRuntimeLaunchConfig,
    ) -> Result<Arc<Self>, ClaudeRuntimeError> {
        let thread_id = thread_id.into();
        let process = Arc::new(PersistentProcess::spawn(config.process_spec())?);
        let stdout = process.take_stdout()?;
        let stderr = process.take_stderr()?;
        let channel = JsonLineChannel::spawn(stdout, stderr, config.max_frame_bytes)?;
        let writer = process.writer();
        let (event_tx, events) = mpsc::channel();
        let (traffic_tx, traffic) = mpsc::channel();
        let protocol_logger = ProtocolTrafficLogger::default();
        protocol_logger.register_protocol_log(&thread_id, "claude", &config.cwd);
        let session_id = config
            .resume_session_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let state = Arc::new(Mutex::new(ControllerState {
            session_id,
            active: None,
            fatal: false,
            shutdown: false,
        }));
        let operation_gate = Arc::new(Mutex::new(()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let controller = Arc::new(Self {
            thread_id: thread_id.clone(),
            process: Arc::clone(&process),
            writer,
            state: Arc::clone(&state),
            events: Arc::new(Mutex::new(events)),
            traffic: Arc::new(Mutex::new(traffic)),
            event_tx: event_tx.clone(),
            traffic_tx: traffic_tx.clone(),
            protocol_logger: protocol_logger.clone(),
            operation_gate: Arc::clone(&operation_gate),
            shutdown: Arc::clone(&shutdown),
        });
        spawn_stream_event_loop(StreamLoopContext {
            thread_id,
            process,
            state,
            event_tx,
            traffic_tx,
            protocol_logger,
            operation_gate,
            shutdown,
            channel,
        });
        Ok(controller)
    }

    pub fn generation(&self) -> ProcessGeneration {
        self.process.generation()
    }

    pub fn process_id(&self) -> u32 {
        self.process.pid()
    }

    pub fn session_id(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.session_id.clone())
    }

    pub fn start_prepare(&self, prompt: &str) -> Result<(), ClaudeRuntimeError> {
        self.start_operation(ActiveOperationKind::Prepare, None, prompt)
    }

    pub fn start_turn(&self, local_turn_id: &str, prompt: &str) -> Result<(), ClaudeRuntimeError> {
        self.start_operation(
            ActiveOperationKind::UserTurn,
            Some(local_turn_id.to_string()),
            prompt,
        )
    }

    fn start_operation(
        &self,
        kind: ActiveOperationKind,
        local_turn_id: Option<String>,
        prompt: &str,
    ) -> Result<(), ClaudeRuntimeError> {
        let _gate = self
            .operation_gate
            .lock()
            .expect("Claude operation gate mutex poisoned");
        {
            let mut state = self
                .state
                .lock()
                .expect("Claude controller state mutex poisoned");
            if state.shutdown || self.shutdown.load(Ordering::Acquire) {
                return Err(ClaudeRuntimeError::ShuttingDown);
            }
            if state.fatal || !self.process.is_running() {
                return Err(ClaudeRuntimeError::RuntimeStart(
                    "runtime generation is not healthy".to_string(),
                ));
            }
            if state.active.is_some() {
                return Err(ClaudeRuntimeError::Busy);
            }
            state.active = Some(ActiveOperation {
                kind,
                local_turn_id: local_turn_id.clone(),
            });
        }

        let message = claude_user_input(prompt);
        let write_result = write_json_line(&self.writer, &message);
        if let Err(error) = write_result {
            if let Ok(mut state) = self.state.lock() {
                state.active = None;
                state.fatal = true;
            }
            let _ = self.process.retire();
            return Err(ClaudeRuntimeError::Write(error));
        }
        let traffic = self.protocol_logger.capture(
            "client_to_provider",
            "event",
            &message,
            Some(&self.thread_id),
            false,
        );
        let _ = self.traffic_tx.send(traffic);
        if kind == ActiveOperationKind::UserTurn {
            if let Some(local_turn_id) = local_turn_id {
                let _ = self
                    .event_tx
                    .send(ClaudeRuntimeEvent::TurnStarted { local_turn_id });
            }
        }
        Ok(())
    }

    pub fn recv_event(&self) -> Result<ClaudeRuntimeEvent, RecvError> {
        self.events
            .lock()
            .expect("Claude events mutex poisoned")
            .recv()
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ClaudeRuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("Claude events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn recv_protocol_traffic_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProtocolTrafficRecord, RecvTimeoutError> {
        self.traffic
            .lock()
            .expect("Claude traffic mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn shutdown_runtime(&self) -> Result<(), ClaudeRuntimeError> {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.active = None;
        }
        self.process
            .shutdown(DEFAULT_SHUTDOWN_GRACE)
            .map(|_| ())
            .map_err(|error| ClaudeRuntimeError::RuntimeStart(error.to_string()))
    }
}

impl ProviderRuntimeController for ClaudeStreamController {
    fn shutdown(&self) -> Result<(), String> {
        self.shutdown_runtime().map_err(|error| error.to_string())
    }

    fn is_healthy(&self) -> bool {
        if self.shutdown.load(Ordering::Acquire) || !self.process.is_running() {
            return false;
        }
        self.state
            .lock()
            .map(|state| !state.shutdown && !state.fatal)
            .unwrap_or(false)
    }
}

fn claude_user_input(prompt: &str) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt,
        }
    })
}

fn write_json_line(writer: &PersistentWriter, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

fn extract_session_id(object: &Map<String, Value>) -> Option<String> {
    object
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn extract_assistant_text(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    }
}

fn extract_stream_text_delta(object: &Map<String, Value>) -> Option<String> {
    let event = object.get("event")?.as_object()?;
    if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return None;
    }
    let delta = event.get("delta")?.as_object()?;
    if delta.get("type").and_then(Value::as_str) != Some("text_delta") {
        return None;
    }
    delta
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn result_is_success(object: &Map<String, Value>) -> bool {
    if object.get("is_error").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let subtype = object
        .get("subtype")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    matches!(subtype, Some("success"))
}

fn map_result_usage(object: &Map<String, Value>) -> Option<Value> {
    let usage = object
        .get("usage")
        .cloned()
        .filter(|value| !value.is_null());
    let model_usage = object
        .get("modelUsage")
        .cloned()
        .filter(|value| !value.is_null());
    match (usage, model_usage) {
        (Some(Value::Object(mut map)), Some(model_usage)) => {
            map.insert("modelUsage".to_string(), model_usage);
            Some(Value::Object(map))
        }
        (Some(usage), Some(model_usage)) => Some(json!({
            "usage": usage,
            "modelUsage": model_usage,
        })),
        (Some(usage), None) => Some(usage),
        (None, Some(model_usage)) => Some(json!({ "modelUsage": model_usage })),
        (None, None) => None,
    }
}

struct StreamLoopContext {
    thread_id: String,
    process: Arc<PersistentProcess>,
    state: Arc<Mutex<ControllerState>>,
    event_tx: mpsc::Sender<ClaudeRuntimeEvent>,
    traffic_tx: mpsc::Sender<ProtocolTrafficRecord>,
    protocol_logger: ProtocolTrafficLogger,
    operation_gate: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
    channel: JsonLineChannel,
}

fn spawn_stream_event_loop(context: StreamLoopContext) {
    thread::Builder::new()
        .name(format!("pedelec-claude-stream-{}", context.thread_id))
        .spawn(move || stream_event_loop(context))
        .expect("could not start Claude stream event loop");
}

fn stream_event_loop(context: StreamLoopContext) {
    let exit_rx = context.process.subscribe_exit();
    loop {
        match context.channel.recv_timeout(Duration::from_millis(25)) {
            Ok(JsonLineEvent::StdoutFrame(frame)) => {
                let _gate = context
                    .operation_gate
                    .lock()
                    .expect("Claude operation gate mutex poisoned");
                let traffic = context.protocol_logger.capture(
                    "provider_to_client",
                    "event",
                    &frame,
                    Some(&context.thread_id),
                    false,
                );
                let _ = context.traffic_tx.send(traffic);
                if let Err(message) = handle_stdout_frame(&context, frame) {
                    mark_protocol_fatal(&context, message);
                    break;
                }
            }
            Ok(JsonLineEvent::StderrChunk(text)) => {
                let _ = context.event_tx.send(ClaudeRuntimeEvent::Stderr { text });
            }
            Ok(JsonLineEvent::StdoutEof { clean }) => {
                if !context.shutdown.load(Ordering::Acquire) && !state_is_fatal(&context.state) {
                    mark_disconnected(
                        &context,
                        if clean {
                            "Claude stdout closed unexpectedly".to_string()
                        } else {
                            "Claude stdout ended with an incomplete JSON frame".to_string()
                        },
                    );
                }
                break;
            }
            Ok(JsonLineEvent::StderrEof) => {}
            Ok(JsonLineEvent::Error(error)) => {
                if !context.shutdown.load(Ordering::Acquire) {
                    mark_protocol_fatal(&context, error.to_string());
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => match exit_rx.try_recv() {
                Ok(exit) => {
                    if exit.kind == ProcessExitKind::UnexpectedExit
                        && !context.shutdown.load(Ordering::Acquire)
                        && !state_is_fatal(&context.state)
                    {
                        mark_disconnected(
                            &context,
                            format!(
                                "Claude process exited unexpectedly (code {:?})",
                                exit.status.and_then(|status| status.code)
                            ),
                        );
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {}
            },
            Err(RecvTimeoutError::Disconnected) => {
                if !context.shutdown.load(Ordering::Acquire) && !state_is_fatal(&context.state) {
                    mark_disconnected(&context, "Claude stream channel disconnected".to_string());
                }
                break;
            }
        }
    }
}

fn handle_stdout_frame(context: &StreamLoopContext, frame: Value) -> Result<(), String> {
    let object = frame
        .as_object()
        .ok_or_else(|| "Claude protocol frame must be a JSON object".to_string())?;
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "system" => handle_system(context, object),
        "stream_event" => handle_stream_event(context, object),
        "assistant" => handle_assistant(context, object),
        "result" => handle_result(context, object),
        _ => Ok(()),
    }
}

fn handle_system(context: &StreamLoopContext, object: &Map<String, Value>) -> Result<(), String> {
    if object.get("subtype").and_then(Value::as_str) != Some("init") {
        return Ok(());
    }
    handle_init(context, object)
}

fn handle_init(context: &StreamLoopContext, object: &Map<String, Value>) -> Result<(), String> {
    let session_id = extract_session_id(object)
        .ok_or_else(|| "Claude system/init is missing session_id".to_string())?;
    let mut state = context
        .state
        .lock()
        .map_err(|_| "Claude controller state mutex poisoned".to_string())?;
    let active_kind = state
        .active
        .as_ref()
        .ok_or_else(|| "Claude system/init arrived without an active operation".to_string())?
        .kind;
    if let Some(existing) = state.session_id.as_deref() {
        if existing != session_id {
            return Err(format!(
                "Claude session id changed from {existing} to {session_id}"
            ));
        }
        return Ok(());
    }
    state.session_id = Some(session_id.clone());
    if active_kind == ActiveOperationKind::UserTurn {
        drop(state);
        let _ = context
            .event_tx
            .send(ClaudeRuntimeEvent::SessionReady { session_id });
    }
    Ok(())
}

fn handle_stream_event(
    context: &StreamLoopContext,
    object: &Map<String, Value>,
) -> Result<(), String> {
    let active = {
        let state = context
            .state
            .lock()
            .map_err(|_| "Claude controller state mutex poisoned".to_string())?;
        state
            .active
            .clone()
            .ok_or_else(|| "Claude stream_event arrived without an active operation".to_string())?
    };
    if active.kind == ActiveOperationKind::Prepare {
        return Ok(());
    }
    let Some(text) = extract_stream_text_delta(object) else {
        return Ok(());
    };
    let local_turn_id = active
        .local_turn_id
        .ok_or_else(|| "Claude user turn is missing local turn id".to_string())?;
    let _ = context.event_tx.send(ClaudeRuntimeEvent::AssistantDelta {
        local_turn_id,
        text,
    });
    Ok(())
}

fn handle_assistant(
    context: &StreamLoopContext,
    object: &Map<String, Value>,
) -> Result<(), String> {
    let active = {
        let state = context
            .state
            .lock()
            .map_err(|_| "Claude controller state mutex poisoned".to_string())?;
        state.active.clone().ok_or_else(|| {
            "Claude assistant event arrived without an active operation".to_string()
        })?
    };
    if active.kind == ActiveOperationKind::Prepare {
        return Ok(());
    }
    let Some(text) = object.get("message").and_then(extract_assistant_text) else {
        return Ok(());
    };
    let local_turn_id = active
        .local_turn_id
        .ok_or_else(|| "Claude user turn is missing local turn id".to_string())?;
    let _ = context.event_tx.send(ClaudeRuntimeEvent::AssistantMessage {
        local_turn_id,
        text,
    });
    Ok(())
}

fn handle_result(context: &StreamLoopContext, object: &Map<String, Value>) -> Result<(), String> {
    let result_session_id = extract_session_id(object);
    let success = result_is_success(object);
    let status = object
        .get("subtype")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            object
                .get("terminal_reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or(if success { "success" } else { "error" })
        .to_string();
    let error = if success {
        None
    } else {
        Some(json!({
            "is_error": object.get("is_error"),
            "subtype": object.get("subtype"),
            "terminal_reason": object.get("terminal_reason"),
            "result": object.get("result"),
        }))
    };
    let usage = map_result_usage(object);

    let (active, session_id) = {
        let mut state = context
            .state
            .lock()
            .map_err(|_| "Claude controller state mutex poisoned".to_string())?;
        let active = state
            .active
            .take()
            .ok_or_else(|| "Claude result arrived without an active operation".to_string())?;
        if let Some(result_id) = result_session_id.as_deref() {
            if let Some(existing) = state.session_id.as_deref() {
                if existing != result_id {
                    return Err(format!(
                        "Claude result session id changed from {existing} to {result_id}"
                    ));
                }
            } else {
                state.session_id = Some(result_id.to_string());
            }
        }
        let session_id = state.session_id.clone().ok_or_else(|| {
            "Claude result arrived before session identity was established".to_string()
        })?;
        (active, session_id)
    };

    match active.kind {
        ActiveOperationKind::Prepare => {
            if success {
                let _ = context
                    .event_tx
                    .send(ClaudeRuntimeEvent::SessionReady { session_id });
            } else {
                let _ = context.event_tx.send(ClaudeRuntimeEvent::TurnCompleted {
                    local_turn_id: None,
                    status,
                    success: false,
                    error,
                    session_id: Some(session_id),
                });
                if let Ok(mut state) = context.state.lock() {
                    state.fatal = true;
                }
                let _ = context.process.retire();
            }
        }
        ActiveOperationKind::UserTurn => {
            let local_turn_id = active
                .local_turn_id
                .ok_or_else(|| "Claude user turn is missing local turn id".to_string())?;
            if let Some(usage) = usage {
                let _ = context.event_tx.send(ClaudeRuntimeEvent::UsageUpdated {
                    local_turn_id: local_turn_id.clone(),
                    usage,
                });
            }
            let _ = context.event_tx.send(ClaudeRuntimeEvent::TurnCompleted {
                local_turn_id: Some(local_turn_id),
                status,
                success,
                error,
                session_id: Some(session_id),
            });
        }
    }
    Ok(())
}

fn state_is_fatal(state: &Mutex<ControllerState>) -> bool {
    state.lock().map(|state| state.fatal).unwrap_or(true)
}

fn mark_protocol_fatal(context: &StreamLoopContext, message: String) {
    let local_turn_id = {
        let mut state = match context.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.fatal || state.shutdown {
            return;
        }
        state.fatal = true;
        state
            .active
            .as_ref()
            .and_then(|active| active.local_turn_id.clone())
    };
    let _ = context.event_tx.send(ClaudeRuntimeEvent::ProtocolError {
        local_turn_id,
        message,
    });
    let _ = context.process.retire();
}

fn mark_disconnected(context: &StreamLoopContext, reason: String) {
    {
        let mut state = match context.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.fatal || state.shutdown {
            return;
        }
        state.fatal = true;
    }
    let _ = context.event_tx.send(ClaudeRuntimeEvent::Disconnected {
        generation: context.process.generation(),
        pid: context.process.pid(),
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;
    use tempfile::tempdir;

    struct FakeClaude {
        directory: tempfile::TempDir,
        program: PathBuf,
        log: PathBuf,
        malformed: bool,
        delayed: bool,
        drift_session: bool,
    }

    impl FakeClaude {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let log = directory.path().join("stdin.jsonl");
            #[cfg(windows)]
            let program = {
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_claude_stream.ps1");
                let wrapper = directory.path().join("fake-claude.cmd");
                fs::write(
                    &wrapper,
                    format!(
                        "@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" %*\r\n",
                        script.display()
                    ),
                )
                .unwrap();
                wrapper
            };
            #[cfg(not(windows))]
            let program = {
                use std::os::unix::fs::PermissionsExt;
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_claude_stream.sh");
                let wrapper = directory.path().join("fake-claude");
                fs::write(
                    &wrapper,
                    format!("#!/bin/sh\nexec sh '{}' \"$@\"\n", script.display()),
                )
                .unwrap();
                let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&wrapper, permissions).unwrap();
                wrapper
            };
            Self {
                directory,
                program,
                log,
                malformed: false,
                delayed: false,
                drift_session: false,
            }
        }

        fn malformed(mut self) -> Self {
            self.malformed = true;
            self
        }

        fn delayed(mut self) -> Self {
            self.delayed = true;
            self
        }

        fn drift_session(mut self) -> Self {
            self.drift_session = true;
            self
        }

        fn launch(&self, resume_session_id: Option<&str>) -> ClaudeRuntimeLaunchConfig {
            let mut config =
                ClaudeRuntimeLaunchConfig::new(self.program.clone(), self.directory.path())
                    .with_host_instructions("Pedelec host instructions")
                    .with_resume_session_id(resume_session_id.map(str::to_string))
                    .with_env("FAKE_CLAUDE_LOG", self.log.to_string_lossy().into_owned());
            if self.malformed {
                config = config.with_env("FAKE_CLAUDE_MALFORMED", "1");
            }
            if self.delayed {
                config = config.with_env("FAKE_CLAUDE_DELAY_MS", "250");
            }
            if self.drift_session {
                config = config.with_env("FAKE_CLAUDE_DRIFT_SESSION", "1");
            }
            config
        }

        fn stdin_frames(&self) -> Vec<Value> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn launch_spec_contains_persistent_stream_flags_and_no_resume_when_fresh() {
        let config = ClaudeRuntimeLaunchConfig::new("claude", ".")
            .with_host_instructions("host-contract")
            .with_model(Some("claude-opus-4-8".to_string()))
            .with_effort(Some(ClaudeReasoningEffort::Medium));
        let spec = config.process_spec();
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"-p".to_string()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--input-format", "stream-json"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"]));
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--disable-slash-commands".to_string()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--append-system-prompt", "host-contract"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-opus-4-8"]));
        assert!(args.windows(2).any(|pair| pair == ["--effort", "medium"]));
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[test]
    fn restore_launch_spec_resumes_and_still_appends_host_instructions() {
        let config = ClaudeRuntimeLaunchConfig::new("claude", ".")
            .with_host_instructions("current-host-contract")
            .with_resume_session_id(Some("persisted-session".to_string()));
        let spec = config.process_spec();
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--resume", "persisted-session"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--append-system-prompt", "current-host-contract"]));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(!args.contains(&"--disable-slash-commands".to_string()));
    }

    #[test]
    fn assistant_parser_extracts_only_typed_text_blocks() {
        assert_eq!(
            extract_assistant_text(&json!({
                "content": [{ "type": "thinking", "thinking": "hidden" }]
            })),
            None
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "content": [{ "type": "text", "text": "hello" }]
            }))
            .as_deref(),
            Some("hello")
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "content": [
                    { "type": "text", "text": "one" },
                    { "type": "thinking", "thinking": "skip" },
                    { "type": "text", "text": "two" }
                ]
            }))
            .as_deref(),
            Some("onetwo")
        );
        assert_eq!(
            extract_assistant_text(&json!({
                "content": [{
                    "type": "thinking",
                    "thinking": "secret",
                    "nested": { "text": "should-not-capture" }
                }]
            })),
            None
        );
    }

    #[test]
    fn stream_event_parser_maps_only_text_delta() {
        assert_eq!(
            extract_stream_text_delta(
                json!({
                    "event": {
                        "type": "content_block_delta",
                        "delta": { "type": "text_delta", "text": "delta" }
                    }
                })
                .as_object()
                .unwrap()
            )
            .as_deref(),
            Some("delta")
        );
        assert_eq!(
            extract_stream_text_delta(
                json!({
                    "event": {
                        "type": "content_block_delta",
                        "delta": { "type": "thinking_delta", "thinking": "hidden" }
                    }
                })
                .as_object()
                .unwrap()
            ),
            None
        );
    }

    #[test]
    fn result_success_uses_subtype_contract_not_result_text() {
        assert!(result_is_success(
            json!({
                "subtype": "success",
                "is_error": false,
                "terminal_reason": "completed",
                "result": ""
            })
            .as_object()
            .unwrap()
        ));
        assert!(!result_is_success(
            json!({
                "subtype": "success",
                "is_error": true,
                "result": "looks like text"
            })
            .as_object()
            .unwrap()
        ));
        for subtype in [
            "error_max_turns",
            "error_during_execution",
            "error_max_budget_usd",
            "error_max_structured_output_retries",
        ] {
            assert!(
                !result_is_success(
                    json!({
                        "subtype": subtype,
                        "is_error": false,
                        "result": "looks like text"
                    })
                    .as_object()
                    .unwrap()
                ),
                "{subtype} must not be treated as success"
            );
        }
        assert!(!result_is_success(
            json!({
                "subtype": "unknown_terminal",
                "is_error": false,
                "terminal_reason": "completed",
                "result": "looks like text"
            })
            .as_object()
            .unwrap()
        ));
        assert!(!result_is_success(
            json!({
                "is_error": false,
                "terminal_reason": "completed",
                "result": "looks like text"
            })
            .as_object()
            .unwrap()
        ));
        assert!(!result_is_success(
            json!({
                "subtype": "   ",
                "is_error": false,
                "result": "looks like text"
            })
            .as_object()
            .unwrap()
        ));
    }

    #[test]
    fn prepare_waits_for_terminal_success_and_suppresses_prepare_semantics() {
        let fixture = FakeClaude::new();
        let controller =
            ClaudeStreamController::spawn("thread-prepare", fixture.launch(None)).unwrap();
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_millis(150)),
            Err(RecvTimeoutError::Timeout)
        ));
        controller
            .start_prepare("[Session Preparation]\nInitialize this provider conversation for subsequent Pedelec user turns. Do not call tools or modify files. A brief acknowledgement is sufficient.")
            .unwrap();

        let event = controller
            .recv_event_timeout(Duration::from_secs(3))
            .unwrap();
        assert_eq!(
            event,
            ClaudeRuntimeEvent::SessionReady {
                session_id: "claude-session-fresh".to_string()
            }
        );
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        let frames = fixture.stdin_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "user");
        assert_eq!(frames[0]["message"]["role"], "user");
        assert!(frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Session Preparation]"));
        assert!(!frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Pedelec Host Bootstrap]"));

        let mut traffic = Vec::new();
        while let Ok(record) = controller.recv_protocol_traffic_timeout(Duration::from_millis(50)) {
            traffic.push(record);
        }
        assert!(traffic.iter().any(|record| {
            record.direction == "client_to_provider" && record.message["type"] == "user"
        }));
        assert!(traffic.iter().any(|record| {
            record.direction == "provider_to_client"
                && record.message["type"] == "system"
                && record.message["subtype"] == "init"
        }));
        assert!(traffic
            .iter()
            .any(|record| record.message["type"] == "assistant"));
        assert!(traffic
            .iter()
            .any(|record| record.message["type"] == "result"));
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn same_process_serves_prepare_and_two_user_turns() {
        let fixture = FakeClaude::new();
        let controller =
            ClaudeStreamController::spawn("thread-multi", fixture.launch(None)).unwrap();
        let pid = controller.process_id();
        let generation = controller.generation();
        controller.start_prepare("[Session Preparation]").unwrap();
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_secs(3)).unwrap(),
            ClaudeRuntimeEvent::SessionReady { session_id } if session_id == "claude-session-fresh"
        ));

        controller.start_turn("local-1", "first").unwrap();
        let mut saw_delta = false;
        let mut saw_message = false;
        let mut saw_usage = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::TurnStarted { local_turn_id } => {
                    assert_eq!(local_turn_id, "local-1");
                }
                ClaudeRuntimeEvent::AssistantDelta { text, .. } => {
                    saw_delta |= text == "哈囉-2";
                }
                ClaudeRuntimeEvent::AssistantMessage { text, .. } => {
                    saw_message |= text == "final-2";
                    assert_ne!(text, "should-not-emit");
                }
                ClaudeRuntimeEvent::UsageUpdated { usage, .. } => {
                    saw_usage = usage["input_tokens"] == 22 && usage.get("modelUsage").is_some();
                }
                ClaudeRuntimeEvent::TurnCompleted { success, .. } => {
                    assert!(success);
                    break;
                }
                ClaudeRuntimeEvent::SessionReady { .. } => {
                    panic!("second init should not emit SessionReady")
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_delta && saw_message && saw_usage);

        controller.start_turn("local-2", "second").unwrap();
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::TurnCompleted {
                    success,
                    session_id,
                    ..
                } => {
                    assert!(success);
                    assert_eq!(session_id.as_deref(), Some("claude-session-fresh"));
                    break;
                }
                ClaudeRuntimeEvent::SessionReady { .. } => {
                    panic!("repeated init must not switch session identity")
                }
                _ => {}
            }
        }
        assert_eq!(controller.process_id(), pid);
        assert_eq!(controller.generation(), generation);
        assert_eq!(
            controller.session_id().as_deref(),
            Some("claude-session-fresh")
        );
        assert_eq!(fixture.stdin_frames().len(), 3);
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn repeated_init_with_a_new_session_id_is_a_protocol_error() {
        let fixture = FakeClaude::new().drift_session();
        let controller =
            ClaudeStreamController::spawn("thread-drift", fixture.launch(None)).unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap(),
            ClaudeRuntimeEvent::SessionReady { .. }
        ));
        controller.start_turn("local-1", "first").unwrap();
        let mut saw_protocol_error = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::ProtocolError { message, .. } => {
                    assert!(message.contains("session id changed"));
                    saw_protocol_error = true;
                    break;
                }
                ClaudeRuntimeEvent::Disconnected { .. } => break,
                _ => {}
            }
        }
        assert!(saw_protocol_error);
        assert!(!controller.is_healthy());
    }

    #[test]
    fn thinking_only_and_nested_text_do_not_emit_assistant_message() {
        let fixture = FakeClaude::new();
        let controller =
            ClaudeStreamController::spawn("thread-parse", fixture.launch(None)).unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap(),
            ClaudeRuntimeEvent::SessionReady { .. }
        ));
        controller
            .start_turn("local-think", "THINKING_ONLY NESTED_TEXT")
            .unwrap();
        let mut saw_message = false;
        let mut saw_delta = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::AssistantMessage { .. } => saw_message = true,
                ClaudeRuntimeEvent::AssistantDelta { .. } => saw_delta = true,
                ClaudeRuntimeEvent::TurnCompleted { success, .. } => {
                    assert!(success);
                    break;
                }
                _ => {}
            }
        }
        assert!(!saw_message);
        assert!(!saw_delta);
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn unsuccessful_prepare_reports_terminal_failure_without_session_ready() {
        let fixture = FakeClaude::new();
        let controller =
            ClaudeStreamController::spawn("thread-fail", fixture.launch(None)).unwrap();
        controller
            .start_prepare("[Session Preparation] FAIL_RESULT")
            .unwrap();
        let event = controller
            .recv_event_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            event,
            ClaudeRuntimeEvent::TurnCompleted {
                local_turn_id: None,
                success: false,
                ..
            }
        ));
        assert!(!controller.is_healthy());
    }

    #[test]
    fn unsuccessful_user_turn_reports_terminal_failure() {
        let fixture = FakeClaude::new();
        let controller =
            ClaudeStreamController::spawn("thread-fail-turn", fixture.launch(None)).unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap(),
            ClaudeRuntimeEvent::SessionReady { .. }
        ));
        controller.start_turn("local-fail", "FAIL_RESULT").unwrap();
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::TurnCompleted {
                    local_turn_id,
                    success,
                    ..
                } => {
                    assert_eq!(local_turn_id.as_deref(), Some("local-fail"));
                    assert!(!success);
                    break;
                }
                ClaudeRuntimeEvent::TurnStarted { .. }
                | ClaudeRuntimeEvent::AssistantDelta { .. }
                | ClaudeRuntimeEvent::AssistantMessage { .. }
                | ClaudeRuntimeEvent::UsageUpdated { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn resume_launch_keeps_persisted_session_identity() {
        let fixture = FakeClaude::new();
        let controller = ClaudeStreamController::spawn(
            "thread-resume",
            fixture.launch(Some("claude-session-fresh")),
        )
        .unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_secs(3)).unwrap(),
            ClaudeRuntimeEvent::SessionReady { session_id } if session_id == "claude-session-fresh"
        ));
        controller.start_turn("local-resume", "resume").unwrap();
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap()
            {
                ClaudeRuntimeEvent::TurnCompleted {
                    success,
                    session_id,
                    ..
                } => {
                    assert!(success);
                    assert_eq!(session_id.as_deref(), Some("claude-session-fresh"));
                    break;
                }
                ClaudeRuntimeEvent::SessionReady { .. } => {}
                _ => {}
            }
        }
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn resume_init_with_a_different_session_id_fails() {
        let fixture = FakeClaude::new();
        let controller = ClaudeStreamController::spawn(
            "thread-resume-mismatch",
            fixture
                .launch(Some("expected-session"))
                .with_env("FAKE_CLAUDE_FORCE_SESSION", "claude-session-fresh"),
        )
        .unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();
        let event = controller
            .recv_event_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            event,
            ClaudeRuntimeEvent::ProtocolError { message, .. }
                if message.contains("session id changed")
        ));
        assert!(!controller.is_healthy());
    }

    #[test]
    fn active_operation_rejects_a_second_user_line_until_result() {
        let fixture = FakeClaude::new().delayed();
        let controller =
            ClaudeStreamController::spawn("thread-busy", fixture.launch(None)).unwrap();
        controller.start_turn("local-1", "first").unwrap();
        assert_eq!(
            controller.start_turn("local-2", "second"),
            Err(ClaudeRuntimeError::Busy)
        );
        loop {
            if matches!(
                controller
                    .recv_event_timeout(Duration::from_secs(4))
                    .unwrap(),
                ClaudeRuntimeEvent::TurnCompleted { .. }
                    | ClaudeRuntimeEvent::ProtocolError { .. }
                    | ClaudeRuntimeEvent::Disconnected { .. }
            ) {
                break;
            }
        }
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn malformed_stdout_is_fatal_and_reports_protocol_error() {
        let fixture = FakeClaude::new().malformed();
        let controller =
            ClaudeStreamController::spawn("thread-malformed", fixture.launch(None)).unwrap();
        controller.start_turn("local-1", "first").unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(3))
                .unwrap(),
            ClaudeRuntimeEvent::TurnStarted { .. }
        ));
        let event = controller
            .recv_event_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            event,
            ClaudeRuntimeEvent::ProtocolError {
                local_turn_id: Some(local_turn_id),
                ..
            } if local_turn_id == "local-1"
        ));
        assert!(!controller.is_healthy());
    }

    #[test]
    #[ignore = "requires authenticated Claude CLI"]
    fn real_claude_cli_prepare_two_turns_and_resume() {
        let program = real_claude_program().expect("claude CLI was not found on PATH");
        let workspace = tempdir().unwrap();
        let host_instructions =
            "You are a Pedelec smoke-test agent. Do not call tools or modify files. Reply briefly.";
        let prepare = "[Session Preparation]\nInitialize this provider conversation for subsequent Pedelec user turns. Do not call tools or modify files. A brief acknowledgement is sufficient.";
        let mut launch = ClaudeRuntimeLaunchConfig::new(&program, workspace.path())
            .with_host_instructions(host_instructions);
        if let Some(path) = std::env::var_os("PATH") {
            launch = launch.with_env("PATH", path);
        }
        let controller = ClaudeStreamController::spawn("thread-real-claude-smoke", launch).unwrap();
        let pid = controller.process_id();
        let generation = controller.generation();
        controller.start_prepare(prepare).unwrap();
        let session_id = wait_real_session_ready(&controller, Duration::from_secs(90));
        assert!(!session_id.is_empty(), "prepare must capture a session id");

        controller
            .start_turn("local-1", "Reply with one short word.")
            .unwrap();
        wait_real_turn_success(&controller, Duration::from_secs(90));
        assert_eq!(controller.process_id(), pid);
        assert_eq!(controller.generation(), generation);
        assert_eq!(
            controller.session_id().as_deref(),
            Some(session_id.as_str())
        );

        controller
            .start_turn("local-2", "Reply with a different short word.")
            .unwrap();
        wait_real_turn_success(&controller, Duration::from_secs(90));
        assert_eq!(controller.process_id(), pid);
        assert_eq!(controller.generation(), generation);
        assert_eq!(
            controller.session_id().as_deref(),
            Some(session_id.as_str())
        );
        controller.shutdown_runtime().unwrap();

        let mut restore = ClaudeRuntimeLaunchConfig::new(&program, workspace.path())
            .with_host_instructions(host_instructions)
            .with_resume_session_id(Some(session_id.clone()));
        if let Some(path) = std::env::var_os("PATH") {
            restore = restore.with_env("PATH", path);
        }
        let restored =
            ClaudeStreamController::spawn("thread-real-claude-restore", restore).unwrap();
        restored.start_prepare(prepare).unwrap();
        let restored_session = wait_real_session_ready(&restored, Duration::from_secs(90));
        assert_eq!(restored_session, session_id);
        restored
            .start_turn("local-restore", "Reply with one short word.")
            .unwrap();
        wait_real_turn_success(&restored, Duration::from_secs(90));
        assert_eq!(restored.session_id().as_deref(), Some(session_id.as_str()));
        restored.shutdown_runtime().unwrap();
    }

    fn real_claude_program() -> Option<PathBuf> {
        if let Some(explicit) = std::env::var_os("PEDELEC_REAL_CLAUDE") {
            return Some(PathBuf::from(explicit));
        }
        let output = std::process::Command::new(if cfg!(windows) { "where.exe" } else { "which" })
            .arg("claude")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()?
            .trim()
            .to_string();
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }

    fn wait_real_session_ready(controller: &ClaudeStreamController, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut stderr = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for Claude SessionReady; stderr={stderr}"
            );
            match controller.recv_event_timeout(remaining.min(Duration::from_secs(5))) {
                Ok(ClaudeRuntimeEvent::SessionReady { session_id }) => return session_id,
                Ok(ClaudeRuntimeEvent::Stderr { text }) => stderr.push_str(&text),
                Ok(ClaudeRuntimeEvent::ProtocolError { message, .. }) => {
                    panic!("Claude prepare protocol error: {message}; stderr={stderr}");
                }
                Ok(ClaudeRuntimeEvent::Disconnected { reason, .. }) => {
                    panic!("Claude disconnected during prepare: {reason}; stderr={stderr}");
                }
                Ok(ClaudeRuntimeEvent::TurnCompleted {
                    success: false,
                    error,
                    ..
                }) => panic!("Claude prepare failed: {error:?}; stderr={stderr}"),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("Claude event channel disconnected during prepare; stderr={stderr}")
                }
            }
        }
    }

    fn wait_real_turn_success(controller: &ClaudeStreamController, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut stderr = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for Claude turn completion; stderr={stderr}"
            );
            match controller.recv_event_timeout(remaining.min(Duration::from_secs(5))) {
                Ok(ClaudeRuntimeEvent::TurnCompleted { success, error, .. }) => {
                    assert!(success, "Claude turn failed: {error:?}; stderr={stderr}");
                    return;
                }
                Ok(ClaudeRuntimeEvent::Stderr { text }) => stderr.push_str(&text),
                Ok(ClaudeRuntimeEvent::ProtocolError { message, .. }) => {
                    panic!("Claude turn protocol error: {message}; stderr={stderr}");
                }
                Ok(ClaudeRuntimeEvent::Disconnected { reason, .. }) => {
                    panic!("Claude disconnected during turn: {reason}; stderr={stderr}");
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("Claude event channel disconnected during turn; stderr={stderr}")
                }
            }
        }
    }
}
