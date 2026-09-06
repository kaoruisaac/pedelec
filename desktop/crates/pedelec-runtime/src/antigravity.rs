use crate::jsonl::{JsonLineChannel, JsonLineError, JsonLineEvent};
use crate::owner::ProviderRuntimeController;
use crate::persistent_process::{
    PersistentProcess, PersistentProcessError, PersistentProcessSpec, PersistentWriter,
    ProcessExitKind, ProcessGeneration,
};
use crate::protocol::{ProtocolTrafficLogger, ProtocolTrafficRecord};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvError, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const DEFAULT_ANTIGRAVITY_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityReasoningEffort {
    Low,
    Medium,
    High,
}

impl AntigravityReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AntigravityRuntimeLaunchConfig {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub model: Option<String>,
    pub effort: Option<AntigravityReasoningEffort>,
    pub conversation_id: Option<String>,
    pub max_frame_bytes: usize,
}

impl AntigravityRuntimeLaunchConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            env: Vec::new(),
            model: None,
            effort: None,
            conversation_id: None,
            max_frame_bytes: DEFAULT_ANTIGRAVITY_MAX_FRAME_BYTES,
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

    pub fn with_effort(mut self, effort: Option<AntigravityReasoningEffort>) -> Self {
        self.effort = effort;
        self
    }

    pub fn with_conversation_id(mut self, conversation_id: Option<String>) -> Self {
        self.conversation_id = conversation_id;
        self
    }

    pub fn process_spec(&self) -> PersistentProcessSpec {
        let mut args = vec![
            OsString::from("--input-format"),
            OsString::from("stream-json"),
            OsString::from("--output-format"),
            OsString::from("stream-json"),
        ];
        if let Some(conversation_id) = self
            .conversation_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args.push(OsString::from("--conversation"));
            args.push(OsString::from(conversation_id));
        }
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
        if let Some(effort) = self.effort {
            args.push(OsString::from("--effort"));
            args.push(OsString::from(effort.as_str()));
        }
        args.extend([
            OsString::from("--agent"),
            OsString::from("pedelec-runtime"),
            OsString::from("--disable-slash-commands"),
            OsString::from("--mode"),
            OsString::from("accept-edits"),
            OsString::from("--dangerously-skip-permissions"),
        ]);

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
pub enum AntigravityRuntimeEvent {
    SessionReady {
        conversation_id: String,
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
    /// Terminal result usage is cumulative for the Antigravity conversation.
    /// It is intentionally separate from step-level usage telemetry.
    CumulativeUsageUpdated {
        local_turn_id: Option<String>,
        usage: Value,
    },
    TurnCompleted {
        local_turn_id: Option<String>,
        status: String,
        success: bool,
        error: Option<Value>,
        response: Option<String>,
        conversation_id: Option<String>,
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
pub enum AntigravityRuntimeError {
    RuntimeStart(String),
    Busy,
    ShuttingDown,
    Write(String),
}

impl fmt::Display for AntigravityRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeStart(error) => write!(f, "Antigravity runtime start failed: {error}"),
            Self::Busy => write!(f, "Antigravity runtime already has an active operation"),
            Self::ShuttingDown => write!(f, "Antigravity runtime is shutting down"),
            Self::Write(error) => write!(f, "Antigravity stdin write failed: {error}"),
        }
    }
}

impl std::error::Error for AntigravityRuntimeError {}

impl From<PersistentProcessError> for AntigravityRuntimeError {
    fn from(value: PersistentProcessError) -> Self {
        Self::RuntimeStart(value.to_string())
    }
}

impl From<JsonLineError> for AntigravityRuntimeError {
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
    saw_init: bool,
}

#[derive(Debug)]
struct ControllerState {
    conversation_id: Option<String>,
    bootstrap_sent: bool,
    active: Option<ActiveOperation>,
    fatal: bool,
    shutdown: bool,
}

#[derive(Debug)]
pub struct AntigravityStreamController {
    thread_id: String,
    process: Arc<PersistentProcess>,
    writer: PersistentWriter,
    state: Arc<Mutex<ControllerState>>,
    events: Arc<Mutex<mpsc::Receiver<AntigravityRuntimeEvent>>>,
    traffic: Arc<Mutex<mpsc::Receiver<ProtocolTrafficRecord>>>,
    event_tx: mpsc::Sender<AntigravityRuntimeEvent>,
    traffic_tx: mpsc::Sender<ProtocolTrafficRecord>,
    protocol_logger: ProtocolTrafficLogger,
    operation_gate: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
}

impl AntigravityStreamController {
    pub fn spawn(
        thread_id: impl Into<String>,
        config: AntigravityRuntimeLaunchConfig,
    ) -> Result<Arc<Self>, AntigravityRuntimeError> {
        let thread_id = thread_id.into();
        let process = Arc::new(PersistentProcess::spawn(config.process_spec())?);
        let stdout = process.take_stdout()?;
        let stderr = process.take_stderr()?;
        let channel = JsonLineChannel::spawn(stdout, stderr, config.max_frame_bytes)?;
        let writer = process.writer();
        let (event_tx, events) = mpsc::channel();
        let (traffic_tx, traffic) = mpsc::channel();
        let protocol_logger = ProtocolTrafficLogger::default();
        protocol_logger.register_protocol_log(&thread_id, "antigravity", &config.cwd);
        let conversation_id = config
            .conversation_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let state = Arc::new(Mutex::new(ControllerState {
            bootstrap_sent: conversation_id.is_some(),
            conversation_id,
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

    pub fn conversation_id(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.conversation_id.clone())
    }

    pub fn needs_bootstrap(&self) -> bool {
        self.state
            .lock()
            .map(|state| !state.bootstrap_sent)
            .unwrap_or(false)
    }

    pub fn mark_bootstrap_sent(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.bootstrap_sent = true;
        }
    }

    pub fn start_prepare(&self, prompt: &str) -> Result<(), AntigravityRuntimeError> {
        self.start_operation(ActiveOperationKind::Prepare, None, prompt)
    }

    pub fn start_turn(
        &self,
        local_turn_id: &str,
        prompt: &str,
    ) -> Result<(), AntigravityRuntimeError> {
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
    ) -> Result<(), AntigravityRuntimeError> {
        let _gate = self
            .operation_gate
            .lock()
            .expect("Antigravity operation gate mutex poisoned");
        {
            let mut state = self
                .state
                .lock()
                .expect("Antigravity controller state mutex poisoned");
            if state.shutdown || self.shutdown.load(Ordering::Acquire) {
                return Err(AntigravityRuntimeError::ShuttingDown);
            }
            if state.fatal || !self.process.is_running() {
                return Err(AntigravityRuntimeError::RuntimeStart(
                    "runtime generation is not healthy".to_string(),
                ));
            }
            if state.active.is_some() {
                return Err(AntigravityRuntimeError::Busy);
            }
            state.active = Some(ActiveOperation {
                kind,
                local_turn_id: local_turn_id.clone(),
                saw_init: false,
            });
        }

        let message = json!({
            "event": "user",
            "message": { "content": prompt },
        });
        let write_result = write_json_line(&self.writer, &message);
        if let Err(error) = write_result {
            if let Ok(mut state) = self.state.lock() {
                state.active = None;
                state.fatal = true;
            }
            let _ = self.process.retire();
            return Err(AntigravityRuntimeError::Write(error));
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
                    .send(AntigravityRuntimeEvent::TurnStarted { local_turn_id });
            }
        }
        Ok(())
    }

    pub fn recv_event(&self) -> Result<AntigravityRuntimeEvent, RecvError> {
        self.events
            .lock()
            .expect("Antigravity events mutex poisoned")
            .recv()
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AntigravityRuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("Antigravity events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn recv_protocol_traffic_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProtocolTrafficRecord, RecvTimeoutError> {
        self.traffic
            .lock()
            .expect("Antigravity traffic mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn shutdown_runtime(&self) -> Result<(), AntigravityRuntimeError> {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.active = None;
        }
        self.process
            .shutdown(DEFAULT_SHUTDOWN_GRACE)
            .map(|_| ())
            .map_err(|error| AntigravityRuntimeError::RuntimeStart(error.to_string()))
    }
}

impl ProviderRuntimeController for AntigravityStreamController {
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

fn write_json_line(writer: &PersistentWriter, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

struct StreamLoopContext {
    thread_id: String,
    process: Arc<PersistentProcess>,
    state: Arc<Mutex<ControllerState>>,
    event_tx: mpsc::Sender<AntigravityRuntimeEvent>,
    traffic_tx: mpsc::Sender<ProtocolTrafficRecord>,
    protocol_logger: ProtocolTrafficLogger,
    operation_gate: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
    channel: JsonLineChannel,
}

fn spawn_stream_event_loop(context: StreamLoopContext) {
    thread::Builder::new()
        .name(format!("pedelec-antigravity-stream-{}", context.thread_id))
        .spawn(move || stream_event_loop(context))
        .expect("could not start Antigravity stream event loop");
}

fn stream_event_loop(context: StreamLoopContext) {
    let exit_rx = context.process.subscribe_exit();
    loop {
        match context.channel.recv_timeout(Duration::from_millis(25)) {
            Ok(JsonLineEvent::StdoutFrame(frame)) => {
                let _gate = context
                    .operation_gate
                    .lock()
                    .expect("Antigravity operation gate mutex poisoned");
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
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::Stderr { text });
            }
            Ok(JsonLineEvent::StdoutEof { clean }) => {
                if !context.shutdown.load(Ordering::Acquire) && !state_is_fatal(&context.state) {
                    mark_disconnected(
                        &context,
                        if clean {
                            "Antigravity stdout closed unexpectedly".to_string()
                        } else {
                            "Antigravity stdout ended with an incomplete JSON frame".to_string()
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
                                "Antigravity process exited unexpectedly (code {:?})",
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
                    mark_disconnected(
                        &context,
                        "Antigravity stream channel disconnected".to_string(),
                    );
                }
                break;
            }
        }
    }
}

fn handle_stdout_frame(context: &StreamLoopContext, frame: Value) -> Result<(), String> {
    let object = frame
        .as_object()
        .ok_or_else(|| "Antigravity protocol frame must be a JSON object".to_string())?;
    let event = object
        .get("event")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Antigravity protocol frame is missing event".to_string())?;
    match event {
        "init" => handle_init(context, object),
        "step_update" => handle_step_update(context, object),
        "result" => handle_result(context, object),
        _ => Ok(()),
    }
}

fn handle_init(
    context: &StreamLoopContext,
    object: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let conversation_id = object
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Antigravity init is missing conversation_id".to_string())?
        .to_string();
    let mut state = context
        .state
        .lock()
        .map_err(|_| "Antigravity controller state mutex poisoned".to_string())?;
    let active_kind = {
        let active = state
            .active
            .as_mut()
            .ok_or_else(|| "Antigravity init arrived without an active operation".to_string())?;
        if active.saw_init {
            return Err("Antigravity emitted duplicate init for one operation".to_string());
        }
        active.saw_init = true;
        active.kind
    };
    if let Some(existing) = state.conversation_id.as_deref() {
        if existing != conversation_id {
            return Err(format!(
                "Antigravity conversation id changed from {existing} to {conversation_id}"
            ));
        }
        return Ok(());
    }
    state.conversation_id = Some(conversation_id.clone());
    if active_kind == ActiveOperationKind::UserTurn {
        drop(state);
        let _ = context
            .event_tx
            .send(AntigravityRuntimeEvent::SessionReady { conversation_id });
    }
    Ok(())
}

fn handle_step_update(
    context: &StreamLoopContext,
    object: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let (active, conversation_known) = {
        let state = context
            .state
            .lock()
            .map_err(|_| "Antigravity controller state mutex poisoned".to_string())?;
        (
            state.active.clone().ok_or_else(|| {
                "Antigravity step_update arrived without an active operation".to_string()
            })?,
            state.conversation_id.is_some(),
        )
    };
    if !conversation_known && !active.saw_init {
        return Err("Antigravity step_update arrived before initial init".to_string());
    }
    if active.kind == ActiveOperationKind::Prepare {
        return Ok(());
    }
    let local_turn_id = active
        .local_turn_id
        .ok_or_else(|| "Antigravity user turn is missing local turn id".to_string())?;
    if let Some(usage) = object
        .get("usage")
        .cloned()
        .filter(|value| !value.is_null())
    {
        let _ = context
            .event_tx
            .send(AntigravityRuntimeEvent::UsageUpdated {
                local_turn_id: local_turn_id.clone(),
                usage,
            });
    }
    if object.get("step_type").and_then(Value::as_str) == Some("agent_response") {
        if let Some(text) = object
            .get("text_delta")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let _ = context
                .event_tx
                .send(AntigravityRuntimeEvent::AssistantDelta {
                    local_turn_id,
                    text: text.to_string(),
                });
        }
    }
    Ok(())
}

fn handle_result(
    context: &StreamLoopContext,
    object: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let result = object
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| "Antigravity result event is missing result object".to_string())?;
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Antigravity result is missing status".to_string())?
        .to_string();
    let result_conversation_id = result
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let response = result
        .get("response")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let usage = result
        .get("usage")
        .cloned()
        .filter(|value| !value.is_null());
    let error = result
        .get("error")
        .cloned()
        .filter(|value| !value.is_null());

    let (active, conversation_id) = {
        let mut state = context
            .state
            .lock()
            .map_err(|_| "Antigravity controller state mutex poisoned".to_string())?;
        let active = state
            .active
            .take()
            .ok_or_else(|| "Antigravity result arrived without an active operation".to_string())?;
        if state.conversation_id.is_none() && !active.saw_init {
            return Err("Antigravity result arrived before initial init".to_string());
        }
        if let Some(result_id) = result_conversation_id.as_deref() {
            if let Some(existing) = state.conversation_id.as_deref() {
                if existing != result_id {
                    return Err(format!(
                        "Antigravity result conversation id changed from {existing} to {result_id}"
                    ));
                }
            } else {
                state.conversation_id = Some(result_id.to_string());
            }
        }
        let conversation_id = state.conversation_id.clone().ok_or_else(|| {
            "Antigravity result arrived before conversation identity was established".to_string()
        })?;
        (active, conversation_id)
    };

    let success = status == "SUCCESS";
    match active.kind {
        ActiveOperationKind::Prepare => {
            if let Some(usage) = usage {
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::CumulativeUsageUpdated {
                        local_turn_id: None,
                        usage,
                    });
            }
            if success {
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::SessionReady { conversation_id });
            } else {
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::TurnCompleted {
                        local_turn_id: None,
                        status,
                        success: false,
                        error,
                        response,
                        conversation_id: Some(conversation_id),
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
                .ok_or_else(|| "Antigravity user turn is missing local turn id".to_string())?;
            if let Some(usage) = usage {
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::CumulativeUsageUpdated {
                        local_turn_id: Some(local_turn_id.clone()),
                        usage,
                    });
            }
            if let Some(text) = response.clone() {
                let _ = context
                    .event_tx
                    .send(AntigravityRuntimeEvent::AssistantMessage {
                        local_turn_id: local_turn_id.clone(),
                        text,
                    });
            }
            let _ = context
                .event_tx
                .send(AntigravityRuntimeEvent::TurnCompleted {
                    local_turn_id: Some(local_turn_id),
                    status,
                    success,
                    error,
                    response,
                    conversation_id: Some(conversation_id),
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
    let _ = context
        .event_tx
        .send(AntigravityRuntimeEvent::ProtocolError {
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
    let _ = context
        .event_tx
        .send(AntigravityRuntimeEvent::Disconnected {
            generation: context.process.generation(),
            pid: context.process.pid(),
            reason,
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    struct FakeAntigravity {
        directory: tempfile::TempDir,
        program: PathBuf,
        log: PathBuf,
        malformed: bool,
        delayed: bool,
    }

    impl FakeAntigravity {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let log = directory.path().join("stdin.jsonl");
            #[cfg(windows)]
            let program = {
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_antigravity_stream.ps1");
                let wrapper = directory.path().join("fake-agy.cmd");
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
                    .join("tests/fixtures/fake_antigravity_stream.sh");
                let wrapper = directory.path().join("fake-agy");
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

        fn launch(&self, conversation_id: Option<&str>) -> AntigravityRuntimeLaunchConfig {
            let mut config =
                AntigravityRuntimeLaunchConfig::new(self.program.clone(), self.directory.path())
                    .with_conversation_id(conversation_id.map(str::to_string))
                    .with_env("FAKE_AGY_LOG", self.log.to_string_lossy().into_owned());
            if self.malformed {
                config = config.with_env("FAKE_AGY_MALFORMED", "1");
            }
            if self.delayed {
                config = config.with_env("FAKE_AGY_DELAY_MS", "250");
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
    fn launch_spec_contains_persistent_stream_flags_and_no_prompt_flag() {
        let config = AntigravityRuntimeLaunchConfig::new("agy", ".")
            .with_model(Some("gemini-test".to_string()))
            .with_effort(Some(AntigravityReasoningEffort::High))
            .with_conversation_id(Some("conv-1".to_string()));
        let spec = config.process_spec();
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--input-format", "stream-json"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--conversation", "conv-1"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "gemini-test"]));
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--agent", "pedelec-runtime"]));
        assert!(args.contains(&"--disable-slash-commands".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--prompt".to_string()));
    }

    #[test]
    fn agy_frames_preserve_multibyte_text_when_utf8_is_split_between_reads() {
        let cases = [
            json!({
                "event": "step_update",
                "step_type": "agent_response",
                "text_delta": "哈囉-切片"
            }),
            json!({
                "event": "result",
                "result": {
                    "status": "SUCCESS",
                    "conversation_id": "agy-conversation-fresh",
                    "response": "完成-切片"
                }
            }),
        ];

        for expected in cases {
            let mut bytes = serde_json::to_vec(&expected).unwrap();
            bytes.push(b'\n');
            let split = bytes
                .windows(3)
                .position(|window| window == "哈".as_bytes())
                .map(|index| index + 1)
                .or_else(|| {
                    bytes
                        .windows(3)
                        .position(|window| window == "完".as_bytes())
                        .map(|index| index + 1)
                })
                .expect("fixture should contain a multibyte code point");
            let mut framer = crate::jsonl::JsonLineFramer::new(4096).unwrap();
            assert!(framer.push(&bytes[..split]).unwrap().is_empty());
            let frames = framer.push(&bytes[split..]).unwrap();
            assert_eq!(frames, vec![expected]);
        }
    }

    #[test]
    fn prepare_waits_for_terminal_success_and_suppresses_prepare_semantics() {
        let fixture = FakeAntigravity::new();
        let controller =
            AntigravityStreamController::spawn("thread-prepare", fixture.launch(None)).unwrap();
        controller.start_prepare("[Session Preparation]").unwrap();

        let event = controller
            .recv_event_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            event,
            AntigravityRuntimeEvent::SessionReady {
                conversation_id: "agy-conversation-fresh".to_string()
            }
        );
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        let frames = fixture.stdin_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["event"], "user");
        assert_eq!(frames[0]["message"]["content"], "[Session Preparation]");

        let mut traffic = Vec::new();
        while let Ok(record) = controller.recv_protocol_traffic_timeout(Duration::from_millis(50)) {
            traffic.push(record);
        }
        assert!(traffic.iter().all(|record| record.kind == "event"));
        assert!(traffic
            .iter()
            .any(|record| record.message["event"] == "init"));
        assert!(traffic
            .iter()
            .any(|record| record.message["event"] == "step_update"));
        assert!(traffic
            .iter()
            .any(|record| record.message["event"] == "result"));
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn direct_first_turn_captures_init_and_reuses_one_process_for_next_turn() {
        let fixture = FakeAntigravity::new();
        let controller =
            AntigravityStreamController::spawn("thread-direct", fixture.launch(None)).unwrap();
        let pid = controller.process_id();
        controller.start_turn("local-1", "first").unwrap();
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_secs(2)).unwrap(),
            AntigravityRuntimeEvent::TurnStarted { local_turn_id } if local_turn_id == "local-1"
        ));

        let mut saw_ready = false;
        let mut saw_delta = false;
        let mut saw_final = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap()
            {
                AntigravityRuntimeEvent::SessionReady { conversation_id } => {
                    saw_ready = conversation_id == "agy-conversation-fresh";
                }
                AntigravityRuntimeEvent::AssistantDelta { text, .. } => {
                    saw_delta |= text == "哈囉-1";
                }
                AntigravityRuntimeEvent::AssistantMessage { text, .. } => {
                    saw_final |= text == "final-1";
                }
                AntigravityRuntimeEvent::TurnCompleted { success, .. } => {
                    assert!(success);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_ready && saw_delta && saw_final);
        assert_eq!(controller.process_id(), pid);

        controller.start_turn("local-2", "second").unwrap();
        assert!(matches!(
            controller.recv_event_timeout(Duration::from_secs(2)).unwrap(),
            AntigravityRuntimeEvent::TurnStarted { local_turn_id } if local_turn_id == "local-2"
        ));
        let mut second_ready = false;
        let mut second_final = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap()
            {
                AntigravityRuntimeEvent::SessionReady { .. } => second_ready = true,
                AntigravityRuntimeEvent::AssistantMessage { text, .. } => {
                    second_final |= text == "final-2";
                }
                AntigravityRuntimeEvent::TurnCompleted { success, .. } => {
                    assert!(success);
                    break;
                }
                _ => {}
            }
        }
        assert!(!second_ready);
        assert!(second_final);
        assert_eq!(controller.process_id(), pid);
        assert_eq!(fixture.stdin_frames().len(), 2);
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn resumed_conversation_does_not_require_init_and_starts_bootstrapped() {
        let fixture = FakeAntigravity::new();
        let controller = AntigravityStreamController::spawn(
            "thread-resume",
            fixture.launch(Some("persisted-conversation")),
        )
        .unwrap();
        assert!(!controller.needs_bootstrap());
        controller.start_turn("local-resume", "resume").unwrap();

        let mut saw_ready = false;
        let mut completed = false;
        while !completed {
            match controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap()
            {
                AntigravityRuntimeEvent::SessionReady { .. } => saw_ready = true,
                AntigravityRuntimeEvent::TurnCompleted {
                    success,
                    conversation_id,
                    ..
                } => {
                    assert!(success);
                    assert_eq!(conversation_id.as_deref(), Some("persisted-conversation"));
                    completed = true;
                }
                _ => {}
            }
        }
        assert!(!saw_ready);
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn unsuccessful_prepare_reports_terminal_failure_without_session_ready() {
        let fixture = FakeAntigravity::new();
        let controller =
            AntigravityStreamController::spawn("thread-fail", fixture.launch(None)).unwrap();
        controller
            .start_prepare("[Session Preparation] FAIL_RESULT")
            .unwrap();
        let event = controller
            .recv_event_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            event,
            AntigravityRuntimeEvent::TurnCompleted {
                local_turn_id: None,
                success: false,
                status,
                ..
            } if status == "ERROR"
        ));
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn active_operation_rejects_a_second_user_line_until_result() {
        let fixture = FakeAntigravity::new().delayed();
        let controller =
            AntigravityStreamController::spawn("thread-busy", fixture.launch(None)).unwrap();
        controller.start_turn("local-1", "first").unwrap();
        assert_eq!(
            controller.start_turn("local-2", "second"),
            Err(AntigravityRuntimeError::Busy)
        );
        loop {
            if matches!(
                controller
                    .recv_event_timeout(Duration::from_secs(3))
                    .unwrap(),
                AntigravityRuntimeEvent::TurnCompleted { .. }
            ) {
                break;
            }
        }
        controller.shutdown_runtime().unwrap();
    }

    #[test]
    fn malformed_stdout_is_fatal_and_reports_protocol_error() {
        let fixture = FakeAntigravity::new().malformed();
        let controller =
            AntigravityStreamController::spawn("thread-malformed", fixture.launch(None)).unwrap();
        controller.start_turn("local-1", "first").unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap(),
            AntigravityRuntimeEvent::TurnStarted { .. }
        ));
        let event = controller
            .recv_event_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            event,
            AntigravityRuntimeEvent::ProtocolError {
                local_turn_id: Some(local_turn_id),
                ..
            } if local_turn_id == "local-1"
        ));
        assert!(!controller.is_healthy());
    }
}
