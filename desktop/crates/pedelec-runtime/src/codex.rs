//! Codex App Server lifecycle and protocol mapping.
//!
//! This module owns the provider-process boundary for Codex.  Core supplies
//! typed session values through the IPC adapter; this module is the only place
//! that knows the App Server method names and wire field names.

use crate::{
    PersistentProcessSpec, PersistentRuntimeController, RpcDisconnectReason, RpcError, RpcEvent,
    RuntimeControllerError, RuntimeEvent,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub const CODEX_RUNTIME_KEY: &str = "codex";
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl CodexReasoningEffort {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexApprovalPolicy {
    Never,
}

impl CodexApprovalPolicy {
    fn as_wire_value(self) -> &'static str {
        match self {
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSandboxMode {
    ReadOnly,
}

impl CodexSandboxMode {
    fn as_wire_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexTurnSandboxPolicy {
    DangerFullAccess,
}

impl CodexTurnSandboxPolicy {
    fn as_wire_value(self) -> Value {
        match self {
            Self::DangerFullAccess => json!({ "type": "dangerFullAccess" }),
        }
    }
}

/// The per-turn overrides owned by Pedelec. These are deliberately separate
/// from `CodexSessionConfig`: creating/resuming a thread uses a read-only
/// policy, while an admitted user turn explicitly receives full access.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexTurnConfig {
    pub input: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<CodexReasoningEffort>,
    pub approval_policy: CodexApprovalPolicy,
    pub sandbox_policy: CodexTurnSandboxPolicy,
}

/// Typed values that are safe to map into `thread/start` and `thread/resume`.
/// In particular, this type deliberately has no CLI argv field.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexSessionConfig {
    pub model: Option<String>,
    pub effort: Option<CodexReasoningEffort>,
    pub cwd: PathBuf,
    pub approval_policy: CodexApprovalPolicy,
    pub sandbox: CodexSandboxMode,
    pub developer_instructions: String,
    pub config: HashMap<String, Value>,
}

impl CodexSessionConfig {
    pub fn validate(&self) -> Result<(), CodexRuntimeError> {
        if self.cwd.as_os_str().is_empty() {
            return Err(CodexRuntimeError::Protocol {
                operation: "session-config".to_string(),
                message: "Codex session cwd is empty".to_string(),
            });
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(CodexRuntimeError::Protocol {
                operation: "session-config".to_string(),
                message: "Codex session model is empty".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexRuntimeLaunchConfig {
    pub program: PathBuf,
    pub process_cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub max_frame_bytes: usize,
    pub control_timeout: Duration,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
}

impl CodexRuntimeLaunchConfig {
    pub fn new(program: impl Into<PathBuf>, process_cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            process_cwd: process_cwd.into(),
            env: Vec::new(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            control_timeout: DEFAULT_CONTROL_TIMEOUT,
            client_name: "pedelec".to_string(),
            client_title: "Pedelec".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    fn process_spec(&self) -> PersistentProcessSpec {
        #[cfg(windows)]
        {
            let extension = self
                .program
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(extension.as_str(), "cmd" | "bat") {
                return PersistentProcessSpec::new("cmd.exe")
                    .args(["/d", "/c", "call"])
                    .arg(self.program.as_os_str())
                    .arg("app-server")
                    .cwd(self.process_cwd.clone())
                    .env_remove("PEDELEC_THREAD_ID")
                    .env_remove("PEDELEC_WORKSPACE_PATH")
                    .with_envs(&self.env);
            }
        }

        PersistentProcessSpec::new(self.program.clone())
            .arg("app-server")
            .cwd(self.process_cwd.clone())
            .env_remove("PEDELEC_THREAD_ID")
            .env_remove("PEDELEC_WORKSPACE_PATH")
            .with_envs(&self.env)
    }

    fn initialize_params(&self) -> Value {
        json!({
            "clientInfo": {
                "name": self.client_name,
                "title": self.client_title,
                "version": self.client_version,
            },
            // Pedelec only consumes stable initialize/thread APIs.  Do not
            // opt into experimental capabilities to expose unrelated APIs.
            "capabilities": {}
        })
    }
}

// Small extension kept private to this module so process launch config remains
// expressive without exposing a provider-specific env abstraction in the
// generic persistent-process type.
trait PersistentProcessSpecEnv {
    fn with_envs(self, env: &[(OsString, OsString)]) -> Self;
}

impl PersistentProcessSpecEnv for PersistentProcessSpec {
    fn with_envs(mut self, env: &[(OsString, OsString)]) -> Self {
        for (key, value) in env {
            self = self.env(key.clone(), value.clone());
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionAttachment {
    pub pedelec_thread_id: String,
    pub provider_thread_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexRuntimeEvent {
    Notification {
        method: String,
        params: Value,
        pedelec_thread_id: Option<String>,
        provider_thread_id: Option<String>,
        provider_turn_id: Option<String>,
    },
    UnmatchedResponse {
        id: crate::RpcId,
        response: Value,
    },
    Stderr {
        text: String,
    },
    TurnStarted {
        pedelec_thread_id: String,
        provider_thread_id: String,
        provider_turn_id: String,
    },
    AssistantDelta {
        pedelec_thread_id: String,
        provider_thread_id: String,
        provider_turn_id: Option<String>,
        text: String,
    },
    AssistantMessage {
        pedelec_thread_id: String,
        provider_thread_id: String,
        provider_turn_id: Option<String>,
        text: String,
    },
    UsageUpdated {
        pedelec_thread_id: String,
        provider_thread_id: String,
        provider_turn_id: Option<String>,
        usage: Value,
    },
    TurnCompleted {
        pedelec_thread_id: String,
        provider_thread_id: String,
        provider_turn_id: Option<String>,
        status: CodexTurnStatus,
        error: Option<Value>,
    },
    ProtocolError {
        pedelec_thread_id: Option<String>,
        provider_thread_id: Option<String>,
        operation: String,
        message: String,
    },
    /// A server-to-client request was rejected explicitly. This is a
    /// diagnostic event; rejection itself is not a transport failure because
    /// the provider received a completed JSON-RPC error response.
    ServerRequestRejected {
        id: crate::RpcId,
        method: String,
    },
    Disconnected {
        generation: u64,
        pid: u32,
        attachments: Vec<CodexSessionAttachment>,
        reason: RpcDisconnectReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexTurnStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexRuntimeError {
    RuntimeStart {
        operation: String,
        message: String,
    },
    RuntimeDisconnected {
        operation: String,
        message: String,
    },
    Protocol {
        operation: String,
        message: String,
    },
    Request {
        operation: String,
        message: String,
        details: Option<Value>,
    },
}

impl fmt::Display for CodexRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (operation, message) = match self {
            Self::RuntimeStart { operation, message }
            | Self::RuntimeDisconnected { operation, message }
            | Self::Protocol { operation, message }
            | Self::Request {
                operation, message, ..
            } => (operation, message),
        };
        write!(f, "Codex App Server {operation} failed: {message}")
    }
}

impl std::error::Error for CodexRuntimeError {}

#[derive(Debug, Default)]
struct SessionMappings {
    provider_to_pedelec: HashMap<String, String>,
    pedelec_to_provider: HashMap<String, String>,
    session_locks: HashMap<String, Arc<Mutex<()>>>,
    pending_turns: HashMap<String, PendingTurn>,
    completed_local_turns: HashSet<String>,
    completed_provider_turns: HashSet<(String, String)>,
    completed_provider_turn_status: HashMap<(String, String), CodexTurnStatus>,
    turn_changed: Arc<Condvar>,
}

#[derive(Debug, Clone)]
struct PendingTurn {
    provider_thread_id: String,
    local_turn_id: String,
    provider_turn_id: Option<String>,
    saw_evidence: bool,
    completed_assistant_items: HashSet<String>,
    completed_assistant_text_counts: HashMap<String, usize>,
}

impl SessionMappings {
    fn lock_for(&mut self, pedelec_thread_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(pedelec_thread_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn attachments(&self) -> Vec<CodexSessionAttachment> {
        self.pedelec_to_provider
            .iter()
            .map(
                |(pedelec_thread_id, provider_thread_id)| CodexSessionAttachment {
                    pedelec_thread_id: pedelec_thread_id.clone(),
                    provider_thread_id: provider_thread_id.clone(),
                },
            )
            .collect()
    }

    fn clear(&mut self) -> Vec<CodexSessionAttachment> {
        let attachments = self.attachments();
        self.provider_to_pedelec.clear();
        self.pedelec_to_provider.clear();
        self.session_locks.clear();
        self.pending_turns.clear();
        self.completed_local_turns.clear();
        self.completed_provider_turns.clear();
        self.completed_provider_turn_status.clear();
        self.turn_changed.notify_all();
        attachments
    }

    fn register_pending_turn(
        &mut self,
        pedelec_thread_id: &str,
        provider_thread_id: &str,
        local_turn_id: &str,
    ) -> Result<(), CodexRuntimeError> {
        if self.pending_turns.contains_key(pedelec_thread_id) {
            return Err(CodexRuntimeError::Protocol {
                operation: "turn/start".to_string(),
                message: format!("Pedelec thread {pedelec_thread_id} already has an active turn"),
            });
        }
        if self
            .provider_to_pedelec
            .get(provider_thread_id)
            .map(String::as_str)
            != Some(pedelec_thread_id)
        {
            return Err(CodexRuntimeError::Protocol {
                operation: "turn/start".to_string(),
                message: format!(
                    "Codex thread {provider_thread_id} is not attached to Pedelec thread {pedelec_thread_id}"
                ),
            });
        }
        self.pending_turns.insert(
            pedelec_thread_id.to_string(),
            PendingTurn {
                provider_thread_id: provider_thread_id.to_string(),
                local_turn_id: local_turn_id.to_string(),
                provider_turn_id: None,
                saw_evidence: false,
                completed_assistant_items: HashSet::new(),
                completed_assistant_text_counts: HashMap::new(),
            },
        );
        Ok(())
    }

    fn remove_pending_turn(&mut self, pedelec_thread_id: &str, local_turn_id: &str) {
        if self
            .pending_turns
            .get(pedelec_thread_id)
            .is_some_and(|turn| turn.local_turn_id == local_turn_id)
        {
            self.pending_turns.remove(pedelec_thread_id);
        }
    }

    fn turn_has_evidence(&self, local_turn_id: &str) -> bool {
        self.pending_turns
            .values()
            .any(|turn| turn.local_turn_id == local_turn_id && turn.saw_evidence)
            || self.completed_local_turns.contains(local_turn_id)
    }

    fn remember_completed(&mut self, turn: &PendingTurn, status: CodexTurnStatus) {
        self.completed_local_turns
            .insert(turn.local_turn_id.clone());
        if let Some(provider_turn_id) = &turn.provider_turn_id {
            let key = (turn.provider_thread_id.clone(), provider_turn_id.clone());
            self.completed_provider_turns.insert(key.clone());
            self.completed_provider_turn_status.insert(key, status);
        }
        // This is only a late-frame guard, not durable history. Keep the
        // process-local sets bounded if a long-lived runtime handles many
        // turns.
        if self.completed_local_turns.len() > 2048 {
            self.completed_local_turns.clear();
        }
        if self.completed_provider_turns.len() > 2048 {
            self.completed_provider_turns.clear();
            self.completed_provider_turn_status.clear();
        }
        self.turn_changed.notify_all();
    }

    fn register(
        &mut self,
        pedelec_thread_id: &str,
        provider_thread_id: &str,
    ) -> Result<(), CodexRuntimeError> {
        if let Some(existing) = self.pedelec_to_provider.get(pedelec_thread_id) {
            if existing != provider_thread_id {
                return Err(CodexRuntimeError::Protocol {
                    operation: "session-mapping".to_string(),
                    message: format!(
                        "Pedelec thread {pedelec_thread_id} is already attached to {existing}"
                    ),
                });
            }
        }
        if let Some(existing) = self.provider_to_pedelec.get(provider_thread_id) {
            if existing != pedelec_thread_id {
                return Err(CodexRuntimeError::Protocol {
                    operation: "session-mapping".to_string(),
                    message: format!(
                        "Codex thread {provider_thread_id} is already attached to {existing}"
                    ),
                });
            }
        }
        self.pedelec_to_provider.insert(
            pedelec_thread_id.to_string(),
            provider_thread_id.to_string(),
        );
        self.provider_to_pedelec.insert(
            provider_thread_id.to_string(),
            pedelec_thread_id.to_string(),
        );
        Ok(())
    }

    fn remove_pedelec(&mut self, pedelec_thread_id: &str) {
        if let Some(provider_thread_id) = self.pedelec_to_provider.remove(pedelec_thread_id) {
            self.provider_to_pedelec.remove(&provider_thread_id);
        }
    }

    fn completed_status(
        &self,
        provider_thread_id: &str,
        provider_turn_id: &str,
    ) -> Option<CodexTurnStatus> {
        self.completed_provider_turn_status
            .get(&(provider_thread_id.to_string(), provider_turn_id.to_string()))
            .copied()
    }
}

/// One initialized Codex App Server process shared by all Pedelec threads.
pub struct CodexAppServerController {
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<SessionMappings>>,
    events: Mutex<Receiver<CodexRuntimeEvent>>,
    event_worker: Mutex<Option<thread::JoinHandle<()>>>,
    healthy: Arc<AtomicBool>,
    control_timeout: Duration,
}

impl fmt::Debug for CodexAppServerController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexAppServerController")
            .field("generation", &self.generation())
            .field("pid", &self.process_id())
            .field("healthy", &self.is_healthy())
            .finish_non_exhaustive()
    }
}

impl CodexAppServerController {
    pub fn spawn(config: CodexRuntimeLaunchConfig) -> Result<Arc<Self>, CodexRuntimeError> {
        if config.program.as_os_str().is_empty() {
            return Err(CodexRuntimeError::RuntimeStart {
                operation: "spawn".to_string(),
                message: "Codex executable path is empty".to_string(),
            });
        }
        let transport = Arc::new(
            PersistentRuntimeController::spawn(config.process_spec(), config.max_frame_bytes)
                .map_err(|error| runtime_start_error("spawn", error))?,
        );

        if let Err(error) = transport.request(
            "initialize",
            config.initialize_params(),
            config.control_timeout,
        ) {
            let _ = transport.shutdown_with_grace(Duration::from_millis(100));
            return Err(runtime_start_error("initialize", error));
        }
        if let Err(error) = transport.notify("initialized", json!({})) {
            let _ = transport.shutdown_with_grace(Duration::from_millis(100));
            return Err(runtime_start_error("initialized", error));
        }

        let (events_tx, events_rx) = mpsc::channel();
        let mappings = Arc::new(Mutex::new(SessionMappings::default()));
        transport.set_protocol_owner_resolver({
            let mappings = Arc::clone(&mappings);
            Arc::new(move |frame: &Value| {
                let params = frame.get("params")?;
                let provider_thread_id = notification_thread_id(params, params.get("turn"))?;
                mappings
                    .lock()
                    .ok()?
                    .provider_to_pedelec
                    .get(&provider_thread_id)
                    .cloned()
            })
        });
        let healthy = Arc::new(AtomicBool::new(true));
        let event_transport = Arc::clone(&transport);
        let event_mappings = Arc::clone(&mappings);
        let event_healthy = Arc::clone(&healthy);
        let event_worker = thread::Builder::new()
            .name(format!("pedelec-codex-events-{}", transport.generation()))
            .spawn(move || {
                run_event_worker(event_transport, event_mappings, event_healthy, events_tx)
            })
            .map_err(|error| runtime_start_error("event-worker", error))?;

        Ok(Arc::new(Self {
            transport,
            mappings,
            events: Mutex::new(events_rx),
            event_worker: Mutex::new(Some(event_worker)),
            healthy,
            control_timeout: config.control_timeout,
        }))
    }

    pub fn process_id(&self) -> u32 {
        self.transport.process_id()
    }

    pub fn generation(&self) -> u64 {
        self.transport.generation()
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.transport.is_healthy()
    }

    pub fn loaded_provider_thread_id(&self, pedelec_thread_id: &str) -> Option<String> {
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .cloned()
    }

    pub fn loaded_pedelec_thread_id(&self, provider_thread_id: &str) -> Option<String> {
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .provider_to_pedelec
            .get(provider_thread_id)
            .cloned()
    }

    pub fn loaded_sessions(&self) -> Vec<CodexSessionAttachment> {
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .attachments()
    }

    pub fn remove_session(&self, pedelec_thread_id: &str) {
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .remove_pedelec(pedelec_thread_id);
    }

    /// Ensure a Pedelec session is attached to this process generation.
    /// Different Pedelec threads use independent locks and can issue RPCs in
    /// parallel; only repeated operations for one thread are single-flight.
    pub fn ensure_session(
        &self,
        pedelec_thread_id: &str,
        provider_thread_id: Option<&str>,
        config: &CodexSessionConfig,
    ) -> Result<CodexSessionResult, CodexRuntimeError> {
        config.validate()?;
        if !self.is_healthy() {
            return Err(CodexRuntimeError::RuntimeDisconnected {
                operation: "session".to_string(),
                message: "Codex App Server runtime is not healthy".to_string(),
            });
        }
        let lock = {
            let mut mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
            mappings.lock_for(pedelec_thread_id)
        };
        let _guard = lock.lock().expect("Codex session lock poisoned");

        if let Some(provider_thread_id) = provider_thread_id {
            let loaded = self
                .mappings
                .lock()
                .expect("Codex mappings mutex poisoned")
                .pedelec_to_provider
                .get(pedelec_thread_id)
                .cloned();
            if loaded.as_deref() == Some(provider_thread_id) {
                return Ok(CodexSessionResult {
                    provider_thread_id: provider_thread_id.to_string(),
                    resumed: false,
                    already_loaded: true,
                });
            }
        }

        self.transport
            .register_protocol_log(pedelec_thread_id, "codex", &config.cwd);
        let requested_provider_thread_id = provider_thread_id.map(str::to_string);
        let (operation, method, params) = match provider_thread_id {
            Some(provider_thread_id) => (
                "thread/resume",
                "thread/resume",
                build_thread_resume_params(provider_thread_id, config),
            ),
            None => (
                "thread/start",
                "thread/start",
                build_thread_start_params(config),
            ),
        };
        let result = self
            .transport
            .request_scoped(pedelec_thread_id, method, params, self.control_timeout)
            .map_err(|error| self.map_request_error(operation, error))?;
        let provider_thread_id = match parse_provider_thread_id(operation, &result) {
            Ok(provider_thread_id) => provider_thread_id,
            Err(error) => {
                self.retire();
                return Err(error);
            }
        };
        if operation == "thread/resume"
            && requested_provider_thread_id.as_deref() != Some(provider_thread_id.as_str())
        {
            self.retire();
            return Err(CodexRuntimeError::Protocol {
                operation: operation.to_string(),
                message: format!(
                    "thread/resume returned provider thread id {provider_thread_id} instead of the requested persisted thread"
                ),
            });
        }
        if let Err(error) = self
            .mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .register(pedelec_thread_id, &provider_thread_id)
        {
            self.retire();
            return Err(error);
        }
        Ok(CodexSessionResult {
            provider_thread_id,
            resumed: operation == "thread/resume",
            already_loaded: false,
        })
    }

    /// Admit one user turn after its provider thread has been loaded. The
    /// pending turn is registered before the request is written so the event
    /// worker can safely process `turn/started` (and even completion) before
    /// the RPC response arrives.
    pub fn start_turn(
        &self,
        pedelec_thread_id: &str,
        provider_thread_id: &str,
        local_turn_id: &str,
        config: &CodexTurnConfig,
    ) -> Result<CodexTurnStartResult, CodexRuntimeError> {
        if config.cwd.as_os_str().is_empty() {
            return Err(CodexRuntimeError::Protocol {
                operation: "turn/start".to_string(),
                message: "Codex turn cwd is empty".to_string(),
            });
        }
        if config
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(CodexRuntimeError::Protocol {
                operation: "turn/start".to_string(),
                message: "Codex turn model is empty".to_string(),
            });
        }
        if !self.is_healthy() {
            return Err(CodexRuntimeError::RuntimeDisconnected {
                operation: "turn/start".to_string(),
                message: "Codex App Server runtime is not healthy".to_string(),
            });
        }

        let lock = {
            let mut mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
            mappings.lock_for(pedelec_thread_id)
        };
        let _guard = lock.lock().expect("Codex session lock poisoned");
        {
            let mut mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
            mappings.register_pending_turn(pedelec_thread_id, provider_thread_id, local_turn_id)?;
        }

        let result = self.transport.request(
            "turn/start",
            build_turn_start_params(provider_thread_id, config),
            self.control_timeout,
        );
        match result {
            Ok(_) => Ok(CodexTurnStartResult {
                response_received: true,
                evidence_seen: self.turn_has_evidence(local_turn_id),
            }),
            Err(error) => {
                let evidence_seen = self.turn_has_evidence(local_turn_id);
                if evidence_seen {
                    // The request may have started side effects. Notification
                    // lifecycle is authoritative once any turn evidence was
                    // observed, so never turn this into a replay/error merely
                    // because the admission response was late or missing.
                    return Ok(CodexTurnStartResult {
                        response_received: false,
                        evidence_seen: true,
                    });
                }
                self.mappings
                    .lock()
                    .expect("Codex mappings mutex poisoned")
                    .remove_pending_turn(pedelec_thread_id, local_turn_id);
                Err(self.map_request_error("turn/start", error))
            }
        }
    }

    /// Interrupt the active provider turn for one Pedelec thread and wait for
    /// the authoritative `turn/completed` notification. A successful RPC
    /// response alone is intentionally insufficient: Codex may acknowledge
    /// the request while the turn is still mutating the workspace.
    pub fn interrupt_turn(
        &self,
        pedelec_thread_id: &str,
        active_turn_id: Option<&str>,
    ) -> Result<(), CodexRuntimeError> {
        if !self.is_healthy() {
            return Err(CodexRuntimeError::RuntimeDisconnected {
                operation: "turn/interrupt".to_string(),
                message: "Codex App Server runtime is not healthy".to_string(),
            });
        }

        let lock = {
            let mut mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
            mappings.lock_for(pedelec_thread_id)
        };
        let _guard = lock.lock().expect("Codex session lock poisoned");
        let (provider_thread_id, provider_turn_id) = {
            let mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
            let provider_thread_id = mappings
                .pedelec_to_provider
                .get(pedelec_thread_id)
                .cloned()
                .ok_or_else(|| CodexRuntimeError::Protocol {
                    operation: "turn/interrupt".to_string(),
                    message: format!(
                        "Pedelec thread {pedelec_thread_id} is not attached to a Codex thread"
                    ),
                })?;
            let pending = mappings
                .pending_turns
                .get(pedelec_thread_id)
                .ok_or_else(|| CodexRuntimeError::Protocol {
                    operation: "turn/interrupt".to_string(),
                    message: format!("Pedelec thread {pedelec_thread_id} has no active Codex turn"),
                })?;
            let provider_turn_id = match active_turn_id {
                Some(active_turn_id)
                    if pending.provider_turn_id.as_deref() == Some(active_turn_id) => {
                        active_turn_id.to_string()
                    }
                Some(active_turn_id) if pending.local_turn_id == active_turn_id => pending
                    .provider_turn_id
                    .clone()
                    .ok_or_else(|| CodexRuntimeError::Protocol {
                        operation: "turn/interrupt".to_string(),
                        message: format!(
                            "Codex has not assigned a provider turn id for Pedelec turn {active_turn_id}"
                        ),
                    })?,
                Some(active_turn_id) => {
                    return Err(CodexRuntimeError::Protocol {
                        operation: "turn/interrupt".to_string(),
                        message: format!(
                            "active provider turn {active_turn_id} does not match the pending turn"
                        ),
                    });
                }
                None => pending.provider_turn_id.clone().ok_or_else(|| {
                    CodexRuntimeError::Protocol {
                        operation: "turn/interrupt".to_string(),
                        message: "active Codex turn id is not available".to_string(),
                    }
                })?,
            };
            (provider_thread_id, provider_turn_id)
        };

        let deadline = std::time::Instant::now() + self.control_timeout;
        self.transport
            .request(
                "turn/interrupt",
                json!({
                    "threadId": provider_thread_id,
                    "turnId": provider_turn_id,
                }),
                self.control_timeout,
            )
            .map_err(|error| self.map_request_error("turn/interrupt", error))?;

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let completed = match self.wait_for_turn_completion(
            &provider_thread_id,
            &provider_turn_id,
            remaining,
        ) {
            Ok(status) => status,
            Err(error) => {
                // The provider may still be mutating the workspace when the
                // terminal proof is absent. Retire this generation here as
                // well as at the IPC safety boundary so direct runtime users
                // cannot accidentally leave an unconfirmed turn running.
                self.retire();
                return Err(error);
            }
        };
        if completed != CodexTurnStatus::Interrupted {
            self.retire();
            return Err(CodexRuntimeError::Protocol {
                operation: "turn/interrupt".to_string(),
                message: format!("Codex turn completed with {completed:?} instead of interrupted"),
            });
        }
        Ok(())
    }

    /// Unsubscribe a loaded provider thread. Unsubscribe is local cleanup,
    /// not deletion of the persisted Codex conversation. Transport failures
    /// that do not prove the connection is dead are best-effort: the caller
    /// still needs to be able to finish the Pedelec thread locally.
    pub fn unsubscribe_session(&self, pedelec_thread_id: &str) -> Result<(), CodexRuntimeError> {
        if !self.is_healthy() {
            return Err(CodexRuntimeError::RuntimeDisconnected {
                operation: "thread/unsubscribe".to_string(),
                message: "Codex App Server runtime is not healthy".to_string(),
            });
        }
        let provider_thread_id = self
            .mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .cloned();
        let Some(provider_thread_id) = provider_thread_id else {
            return Ok(());
        };

        let result = self.transport.request(
            "thread/unsubscribe",
            json!({ "threadId": provider_thread_id }),
            self.control_timeout,
        );
        match result {
            Ok(_) => {}
            Err(RuntimeControllerError::Rpc(RpcError::Remote { .. })) => {
                // Unsubscribe is best-effort while the transport remains
                // healthy. A shared runtime must survive a remote cleanup
                // rejection for one session.
            }
            Err(RuntimeControllerError::Rpc(RpcError::RequestTimeout { .. })) => {
                // The request may have reached Codex, but this is an idle
                // local cleanup path. Do not kill a healthy shared runtime
                // merely because its best-effort unsubscribe reply was late.
            }
            Err(error) => return Err(self.map_request_error("thread/unsubscribe", error)),
        }
        self.remove_session(pedelec_thread_id);
        Ok(())
    }

    /// Perform the complete persistent end sequence for one attached
    /// session. The returned success means interrupt proof (when needed) and
    /// unsubscribe have both completed; Core still owns final local cleanup.
    pub fn end_session(
        &self,
        pedelec_thread_id: &str,
        active_turn_id: Option<&str>,
    ) -> Result<(), CodexRuntimeError> {
        if active_turn_id.is_some() {
            self.interrupt_turn(pedelec_thread_id, active_turn_id)?;
        }
        self.unsubscribe_session(pedelec_thread_id)
    }

    fn wait_for_turn_completion(
        &self,
        provider_thread_id: &str,
        provider_turn_id: &str,
        timeout: Duration,
    ) -> Result<CodexTurnStatus, CodexRuntimeError> {
        let deadline = std::time::Instant::now() + timeout;
        let mut mappings = self.mappings.lock().expect("Codex mappings mutex poisoned");
        loop {
            if !self.is_healthy() {
                return Err(CodexRuntimeError::RuntimeDisconnected {
                    operation: "turn/interrupt".to_string(),
                    message:
                        "Codex App Server runtime disconnected while waiting for turn completion"
                            .to_string(),
                });
            }
            if let Some(status) = mappings.completed_status(provider_thread_id, provider_turn_id) {
                return Ok(status);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(CodexRuntimeError::Request {
                    operation: "turn/interrupt".to_string(),
                    message: "timed out waiting for interrupted turn completion".to_string(),
                    details: Some(json!({
                        "providerThreadId": provider_thread_id,
                        "providerTurnId": provider_turn_id,
                    })),
                });
            }
            let changed = Arc::clone(&mappings.turn_changed);
            let (next, wait) = changed
                .wait_timeout(mappings, remaining)
                .expect("Codex mappings mutex poisoned");
            mappings = next;
            if !self.is_healthy() {
                return Err(CodexRuntimeError::RuntimeDisconnected {
                    operation: "turn/interrupt".to_string(),
                    message:
                        "Codex App Server runtime disconnected while waiting for turn completion"
                            .to_string(),
                });
            }
            if wait.timed_out() {
                return Err(CodexRuntimeError::Request {
                    operation: "turn/interrupt".to_string(),
                    message: "timed out waiting for interrupted turn completion".to_string(),
                    details: Some(json!({
                        "providerThreadId": provider_thread_id,
                        "providerTurnId": provider_turn_id,
                    })),
                });
            }
        }
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<CodexRuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("Codex events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn recv_event(&self) -> Result<CodexRuntimeEvent, mpsc::RecvError> {
        self.events
            .lock()
            .expect("Codex events mutex poisoned")
            .recv()
    }

    fn map_request_error(
        &self,
        operation: &str,
        error: RuntimeControllerError,
    ) -> CodexRuntimeError {
        match error {
            RuntimeControllerError::Rpc(RpcError::Disconnected { reason }) => {
                self.retire();
                CodexRuntimeError::RuntimeDisconnected {
                    operation: operation.to_string(),
                    message: format!("{reason:?}"),
                }
            }
            RuntimeControllerError::Rpc(RpcError::RequestTimeout { .. }) => {
                // A timed-out thread/start may have been accepted by Codex
                // even though its response was lost. Retire this generation
                // before a later attempt so we never replay that ambiguous
                // session creation on the same connection.
                self.retire();
                CodexRuntimeError::Request {
                    operation: operation.to_string(),
                    message: error.to_string(),
                    details: None,
                }
            }
            RuntimeControllerError::Rpc(RpcError::InvalidMessage { .. }) => {
                self.retire();
                CodexRuntimeError::Protocol {
                    operation: operation.to_string(),
                    message: error.to_string(),
                }
            }
            RuntimeControllerError::Rpc(RpcError::Remote {
                code,
                message,
                data,
            }) => CodexRuntimeError::Request {
                operation: operation.to_string(),
                message: format!("remote RPC error: {message}"),
                details: Some(json!({
                    "code": code,
                    "message": message,
                    "data": data,
                })),
            },
            other => {
                self.retire();
                CodexRuntimeError::Request {
                    operation: operation.to_string(),
                    message: other.to_string(),
                    details: None,
                }
            }
        }
    }

    fn turn_has_evidence(&self, local_turn_id: &str) -> bool {
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .turn_has_evidence(local_turn_id)
    }

    fn retire(&self) {
        if self.healthy.swap(false, Ordering::AcqRel) {
            self.transport.retire();
        }
    }

    /// Retire after a known protocol-shape violation. The next operation may
    /// create a fresh runtime generation; the malformed stream must not be
    /// allowed to continue producing events.
    pub fn retire_for_protocol_error(&self) {
        self.retire();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionResult {
    pub provider_thread_id: String,
    pub resumed: bool,
    pub already_loaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexTurnStartResult {
    pub response_received: bool,
    pub evidence_seen: bool,
}

impl crate::ProviderRuntimeController for CodexAppServerController {
    fn shutdown(&self) -> Result<(), String> {
        self.healthy.store(false, Ordering::Release);
        let result = self
            .transport
            .shutdown_with_grace(Duration::from_secs(1))
            .map_err(|error| error.to_string());
        if result.is_err() {
            self.transport.retire();
        }
        self.mappings
            .lock()
            .expect("Codex mappings mutex poisoned")
            .clear();
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("Codex event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        result.map(|_| ())
    }

    fn is_healthy(&self) -> bool {
        CodexAppServerController::is_healthy(self)
    }
}

impl Drop for CodexAppServerController {
    fn drop(&mut self) {
        self.healthy.store(false, Ordering::Release);
        // The event worker owns a receive loop over the transport. Retire the
        // child before joining it so dropping the last controller reference
        // cannot wait forever for an otherwise idle App Server.
        self.transport.retire();
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("Codex event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn build_thread_start_params(config: &CodexSessionConfig) -> Value {
    let mut params = Map::new();
    params.insert("model".to_string(), optional_string(&config.model));
    params.insert(
        "cwd".to_string(),
        Value::String(config.cwd.to_string_lossy().into_owned()),
    );
    params.insert(
        "approvalPolicy".to_string(),
        Value::String(config.approval_policy.as_wire_value().to_string()),
    );
    params.insert(
        "sandbox".to_string(),
        Value::String(config.sandbox.as_wire_value().to_string()),
    );
    params.insert(
        "developerInstructions".to_string(),
        Value::String(config.developer_instructions.clone()),
    );
    let mut config_values = config_map(&config.config);
    if let Some(effort) = config.effort {
        config_values.insert(
            "model_reasoning_effort".to_string(),
            Value::String(effort.as_str().to_string()),
        );
    }
    params.insert("config".to_string(), Value::Object(config_values));
    params.insert("ephemeral".to_string(), Value::Bool(false));
    Value::Object(params)
}

fn build_thread_resume_params(provider_thread_id: &str, config: &CodexSessionConfig) -> Value {
    let mut params = match build_thread_start_params(config) {
        Value::Object(params) => params,
        _ => unreachable!(),
    };
    params.insert(
        "threadId".to_string(),
        Value::String(provider_thread_id.to_string()),
    );
    Value::Object(params)
}

fn build_turn_start_params(provider_thread_id: &str, config: &CodexTurnConfig) -> Value {
    let mut params = Map::new();
    params.insert(
        "threadId".to_string(),
        Value::String(provider_thread_id.to_string()),
    );
    params.insert(
        "input".to_string(),
        json!([{ "type": "text", "text": config.input }]),
    );
    params.insert(
        "cwd".to_string(),
        Value::String(config.cwd.to_string_lossy().into_owned()),
    );
    params.insert("model".to_string(), optional_string(&config.model));
    params.insert(
        "effort".to_string(),
        config
            .effort
            .map(|effort| Value::String(effort.as_str().to_string()))
            .unwrap_or(Value::Null),
    );
    params.insert(
        "approvalPolicy".to_string(),
        Value::String(config.approval_policy.as_wire_value().to_string()),
    );
    params.insert(
        "sandboxPolicy".to_string(),
        config.sandbox_policy.as_wire_value(),
    );
    Value::Object(params)
}

fn config_map(config: &HashMap<String, Value>) -> Map<String, Value> {
    config
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn optional_string(value: &Option<String>) -> Value {
    value
        .as_ref()
        .map(|value| Value::String(value.clone()))
        .unwrap_or(Value::Null)
}

fn parse_provider_thread_id(operation: &str, result: &Value) -> Result<String, CodexRuntimeError> {
    let provider_thread_id = result
        .get("thread")
        .and_then(Value::as_object)
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CodexRuntimeError::Protocol {
            operation: operation.to_string(),
            message: "response did not contain a non-empty result.thread.id".to_string(),
        })?;
    Ok(provider_thread_id.to_string())
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn notification_thread_id(params: &Value, turn: Option<&Value>) -> Option<String> {
    non_empty_string(params.get("threadId"))
        .or_else(|| turn.and_then(|turn| non_empty_string(turn.get("threadId"))))
}

fn pending_turn_id(
    mappings: &SessionMappings,
    provider_thread_id: Option<&str>,
    provider_turn_id: Option<&str>,
) -> Option<String> {
    mappings
        .pending_turns
        .iter()
        .find(|(_, turn)| {
            let thread_matches = provider_thread_id.is_some_and(|id| turn.provider_thread_id == id);
            let turn_matches = provider_turn_id.is_none_or(|id| {
                let active_matches = if provider_thread_id.is_none() {
                    turn.provider_turn_id.as_deref() == Some(id)
                } else {
                    turn.provider_turn_id
                        .as_deref()
                        .is_none_or(|active| active == id)
                };
                active_matches
                    && !mappings
                        .completed_provider_turns
                        .contains(&(turn.provider_thread_id.clone(), id.to_string()))
            });
            // A provider turn ID is only sufficient once `turn/started` has
            // associated it with a pending turn. Without either a mapped
            // provider thread or that exact association, never guess which
            // concurrent Pedelec thread owns the notification.
            (thread_matches && turn_matches)
                || (provider_thread_id.is_none() && provider_turn_id.is_some() && turn_matches)
        })
        .map(|(pedelec_thread_id, _)| pedelec_thread_id.clone())
}

fn protocol_event(
    mappings: &SessionMappings,
    provider_thread_id: Option<&str>,
    operation: &str,
    message: impl Into<String>,
) -> CodexRuntimeEvent {
    CodexRuntimeEvent::ProtocolError {
        pedelec_thread_id: provider_thread_id
            .and_then(|id| mappings.provider_to_pedelec.get(id).cloned()),
        provider_thread_id: provider_thread_id.map(str::to_string),
        operation: operation.to_string(),
        message: message.into(),
    }
}

fn decode_codex_notification(
    mappings: &Arc<Mutex<SessionMappings>>,
    method: &str,
    params: &Value,
) -> Vec<CodexRuntimeEvent> {
    let mut mappings = mappings.lock().expect("Codex mappings mutex poisoned");
    match method {
        "turn/started" => {
            let Some(turn) = params.get("turn") else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "turn/started notification did not contain turn",
                )];
            };
            let Some(provider_turn_id) = non_empty_string(turn.get("id")) else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, Some(turn)).as_deref(),
                    method,
                    "turn/started notification did not contain a non-empty turn.id",
                )];
            };
            let provider_thread_id = notification_thread_id(params, Some(turn));
            let Some(provider_thread_id_value) = provider_thread_id.as_deref() else {
                return vec![protocol_event(
                    &mappings,
                    None,
                    method,
                    "turn/started notification did not contain a provider thread id",
                )];
            };
            let Some(pedelec_thread_id) = pending_turn_id(
                &mappings,
                Some(provider_thread_id_value),
                Some(provider_turn_id.as_str()),
            ) else {
                return Vec::new();
            };
            let Some(pending) = mappings.pending_turns.get_mut(&pedelec_thread_id) else {
                return Vec::new();
            };
            match pending.provider_turn_id.as_deref() {
                Some(active) if active == provider_turn_id => return Vec::new(),
                Some(_) => return Vec::new(),
                None => {
                    pending.provider_turn_id = Some(provider_turn_id.clone());
                }
            }
            pending.saw_evidence = true;
            vec![CodexRuntimeEvent::TurnStarted {
                pedelec_thread_id,
                provider_thread_id: provider_thread_id_value.to_string(),
                provider_turn_id,
            }]
        }
        "item/agentMessage/delta" => {
            let Some(provider_turn_id) = non_empty_string(params.get("turnId")) else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "assistant delta notification did not contain a turn id",
                )];
            };
            let Some(provider_thread_id) = notification_thread_id(params, None) else {
                return vec![protocol_event(
                    &mappings,
                    None,
                    method,
                    "assistant delta notification did not contain a provider thread id",
                )];
            };
            let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                return vec![protocol_event(
                    &mappings,
                    Some(provider_thread_id.as_str()),
                    method,
                    "assistant delta notification did not contain delta text",
                )];
            };
            let Some(pedelec_thread_id) = pending_turn_id(
                &mappings,
                Some(provider_thread_id.as_str()),
                Some(provider_turn_id.as_str()),
            ) else {
                return Vec::new();
            };
            let Some(pending) = mappings.pending_turns.get_mut(&pedelec_thread_id) else {
                return Vec::new();
            };
            if pending
                .provider_turn_id
                .as_deref()
                .is_some_and(|active| provider_turn_id != active)
            {
                return Vec::new();
            }
            pending.provider_turn_id = Some(provider_turn_id.clone());
            pending.saw_evidence = true;
            if delta.is_empty() {
                return Vec::new();
            }
            vec![CodexRuntimeEvent::AssistantDelta {
                pedelec_thread_id,
                provider_thread_id: pending.provider_thread_id.clone(),
                provider_turn_id: Some(provider_turn_id),
                text: delta.to_string(),
            }]
        }
        "item/completed" => {
            let Some(item) = params.get("item") else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "item/completed notification did not contain item",
                )];
            };
            let Some(item) = item.as_object() else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "item/completed notification item was not an object",
                )];
            };
            if non_empty_string(item.get("type")).as_deref() != Some("agentMessage") {
                return Vec::new();
            }
            let Some(provider_turn_id) = non_empty_string(params.get("turnId")) else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "item/completed notification did not contain a turn id",
                )];
            };
            let Some(provider_thread_id) = notification_thread_id(params, None) else {
                return vec![protocol_event(
                    &mappings,
                    None,
                    method,
                    "item/completed notification did not contain a provider thread id",
                )];
            };
            let Some(pedelec_thread_id) = pending_turn_id(
                &mappings,
                Some(provider_thread_id.as_str()),
                Some(provider_turn_id.as_str()),
            ) else {
                return Vec::new();
            };
            let Some(text) = non_empty_text(item.get("text")) else {
                return Vec::new();
            };
            let item_id = non_empty_string(item.get("id"));
            let Some(pending) = mappings.pending_turns.get_mut(&pedelec_thread_id) else {
                return Vec::new();
            };
            if pending
                .provider_turn_id
                .as_deref()
                .is_some_and(|active| provider_turn_id != active)
            {
                return Vec::new();
            }
            pending.provider_turn_id = Some(provider_turn_id);
            pending.saw_evidence = true;
            if item_id
                .as_ref()
                .is_some_and(|item_id| !pending.completed_assistant_items.insert(item_id.clone()))
            {
                return Vec::new();
            }
            *pending
                .completed_assistant_text_counts
                .entry(text.clone())
                .or_default() += 1;
            vec![CodexRuntimeEvent::AssistantMessage {
                pedelec_thread_id,
                provider_thread_id: pending.provider_thread_id.clone(),
                provider_turn_id: pending.provider_turn_id.clone(),
                text,
            }]
        }
        "thread/tokenUsage/updated" => {
            let usage = params
                .get("tokenUsage")
                .or_else(|| params.get("usage"))
                .cloned();
            let Some(usage) = usage else {
                // Usage is optional telemetry. A missing or malformed usage
                // payload must never invalidate the active turn.
                return Vec::new();
            };
            let provider_turn_id = non_empty_string(params.get("turnId"));
            let provider_thread_id = notification_thread_id(params, None);
            let Some(pedelec_thread_id) = pending_turn_id(
                &mappings,
                provider_thread_id.as_deref(),
                provider_turn_id.as_deref(),
            ) else {
                return Vec::new();
            };
            let Some(pending) = mappings.pending_turns.get_mut(&pedelec_thread_id) else {
                return Vec::new();
            };
            pending.saw_evidence = true;
            vec![CodexRuntimeEvent::UsageUpdated {
                pedelec_thread_id,
                provider_thread_id: pending.provider_thread_id.clone(),
                provider_turn_id: pending.provider_turn_id.clone().or(provider_turn_id),
                usage,
            }]
        }
        "turn/completed" => {
            let Some(turn) = params.get("turn") else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, None).as_deref(),
                    method,
                    "turn/completed notification did not contain turn",
                )];
            };
            let Some(provider_turn_id) = non_empty_string(turn.get("id")) else {
                return vec![protocol_event(
                    &mappings,
                    notification_thread_id(params, Some(turn)).as_deref(),
                    method,
                    "turn/completed notification did not contain a non-empty turn.id",
                )];
            };
            let Some(provider_thread_id) = notification_thread_id(params, Some(turn)) else {
                return vec![protocol_event(
                    &mappings,
                    None,
                    method,
                    "turn/completed notification did not contain a provider thread id",
                )];
            };
            let Some(pedelec_thread_id) = pending_turn_id(
                &mappings,
                Some(provider_thread_id.as_str()),
                Some(provider_turn_id.as_str()),
            ) else {
                return Vec::new();
            };
            let Some(status) = non_empty_string(turn.get("status")) else {
                return vec![protocol_event(
                    &mappings,
                    Some(provider_thread_id.as_str()),
                    method,
                    "turn/completed notification did not contain turn.status",
                )];
            };
            let status = match status.as_str() {
                "completed" => CodexTurnStatus::Completed,
                "failed" => CodexTurnStatus::Failed,
                "interrupted" => CodexTurnStatus::Interrupted,
                _ => {
                    return vec![protocol_event(
                        &mappings,
                        Some(provider_thread_id.as_str()),
                        method,
                        format!("unknown turn completion status {status}"),
                    )]
                }
            };
            let Some(pending) = mappings.pending_turns.get_mut(&pedelec_thread_id) else {
                return Vec::new();
            };
            if pending
                .provider_turn_id
                .as_deref()
                .is_some_and(|active| active != provider_turn_id)
            {
                return Vec::new();
            }
            pending.provider_turn_id = Some(provider_turn_id.clone());
            pending.saw_evidence = true;
            let mut events = Vec::new();
            let mut unmatched_completed_text_counts =
                pending.completed_assistant_text_counts.clone();
            let consume_completed_text = |counts: &mut HashMap<String, usize>, text: &str| {
                if let Some(count) = counts.get_mut(text) {
                    if *count > 0 {
                        *count -= 1;
                        return true;
                    }
                }
                false
            };
            if let Some(items) = turn.get("items").and_then(Value::as_array) {
                for item in items {
                    if non_empty_string(item.get("type")).as_deref() != Some("agentMessage") {
                        continue;
                    }
                    let Some(text) = non_empty_text(item.get("text")) else {
                        continue;
                    };
                    let item_id = non_empty_string(item.get("id"));
                    if item_id
                        .as_ref()
                        .is_some_and(|item_id| pending.completed_assistant_items.contains(item_id))
                    {
                        consume_completed_text(&mut unmatched_completed_text_counts, &text);
                        continue;
                    }
                    if consume_completed_text(&mut unmatched_completed_text_counts, &text) {
                        if let Some(item_id) = item_id {
                            pending.completed_assistant_items.insert(item_id);
                        }
                        continue;
                    }
                    if let Some(item_id) = item_id {
                        pending.completed_assistant_items.insert(item_id);
                    }
                    *pending
                        .completed_assistant_text_counts
                        .entry(text.clone())
                        .or_default() += 1;
                    events.push(CodexRuntimeEvent::AssistantMessage {
                        pedelec_thread_id: pedelec_thread_id.clone(),
                        provider_thread_id: pending.provider_thread_id.clone(),
                        provider_turn_id: pending.provider_turn_id.clone(),
                        text,
                    });
                }
            }
            let completed = pending.clone();
            mappings.remember_completed(&completed, status);
            mappings.pending_turns.remove(&pedelec_thread_id);
            events.push(CodexRuntimeEvent::TurnCompleted {
                pedelec_thread_id,
                provider_thread_id: completed.provider_thread_id,
                provider_turn_id: completed.provider_turn_id,
                status,
                error: turn.get("error").cloned(),
            });
            events
        }
        // Thread lifecycle notifications are useful to a full App Server
        // client, but Pedelec's session mapping is established from the
        // thread/start or thread/resume response. Keep unrelated notifications
        // as diagnostics and remain forward-compatible with new methods.
        _ => {
            let provider_thread_id = notification_thread_id(params, None);
            let pedelec_thread_id = provider_thread_id
                .as_deref()
                .and_then(|id| mappings.provider_to_pedelec.get(id).cloned());
            let provider_turn_id = non_empty_string(params.get("turnId"));
            vec![CodexRuntimeEvent::Notification {
                method: method.to_string(),
                params: params.clone(),
                pedelec_thread_id,
                provider_thread_id,
                provider_turn_id,
            }]
        }
    }
}

fn runtime_start_error(operation: &str, error: impl fmt::Display) -> CodexRuntimeError {
    CodexRuntimeError::RuntimeStart {
        operation: operation.to_string(),
        message: error.to_string(),
    }
}

fn run_event_worker(
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<SessionMappings>>,
    healthy: Arc<AtomicBool>,
    events: Sender<CodexRuntimeEvent>,
) {
    loop {
        match transport.recv_event_timeout(Duration::from_millis(50)) {
            Ok(RuntimeEvent::Rpc(event)) => match event {
                RpcEvent::Notification { method, params } => {
                    let decoded = decode_codex_notification(&mappings, &method, &params);
                    let protocol_error = decoded.iter().find_map(|event| match event {
                        CodexRuntimeEvent::ProtocolError { message, .. } => Some(message.clone()),
                        _ => None,
                    });
                    for event in decoded {
                        if events.send(event).is_err() {
                            return;
                        }
                    }
                    if protocol_error.is_some() {
                        healthy.store(false, Ordering::Release);
                        transport.retire();
                        send_disconnect(
                            &transport,
                            &mappings,
                            &events,
                            RpcDisconnectReason::MalformedFrame(
                                protocol_error
                                    .unwrap_or_else(|| "invalid Codex notification".to_string()),
                            ),
                        );
                        break;
                    }
                }
                RpcEvent::ServerRequest(request) => {
                    // Pedelec intentionally does not advertise/use dynamic
                    // tools, terminal/create, or approval RPCs in this phase.
                    // Reject unsupported requests explicitly so Codex cannot
                    // remain blocked waiting for a client response.
                    let request_id = request.id.clone();
                    let method = request.method.clone();
                    let response = transport.respond_error(
                        request_id,
                        json!(-32601),
                        format!("Pedelec Codex client does not support {method}"),
                        Some(json!({ "method": method })),
                    );
                    if events
                        .send(CodexRuntimeEvent::ServerRequestRejected {
                            id: request.id,
                            method: request.method,
                        })
                        .is_err()
                    {
                        return;
                    }
                    if response.is_err() {
                        // The response writer already marked the RPC peer
                        // disconnected. The reader/event side will emit the
                        // fatal disconnect notification.
                    }
                }
                RpcEvent::Stderr { text } => {
                    if events.send(CodexRuntimeEvent::Stderr { text }).is_err() {
                        return;
                    }
                }
                RpcEvent::Disconnected { reason } => {
                    healthy.store(false, Ordering::Release);
                    if !matches!(reason, RpcDisconnectReason::Explicit) {
                        // EOF/transport failure can be observed before the
                        // child has exited. Retire the process as well so a
                        // disconnected generation cannot keep mutating the
                        // workspace without a routable client.
                        transport.retire();
                        send_disconnect(&transport, &mappings, &events, reason);
                    }
                    break;
                }
                RpcEvent::UnmatchedResponse { id, response } => {
                    if events
                        .send(CodexRuntimeEvent::UnmatchedResponse { id, response })
                        .is_err()
                    {
                        return;
                    }
                }
            },
            Ok(RuntimeEvent::ProcessExit(exit)) => {
                if matches!(exit.kind, crate::ProcessExitKind::UnexpectedExit) {
                    healthy.store(false, Ordering::Release);
                    send_disconnect(
                        &transport,
                        &mappings,
                        &events,
                        RpcDisconnectReason::UncleanEof,
                    );
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn send_disconnect(
    transport: &PersistentRuntimeController,
    mappings: &Mutex<SessionMappings>,
    events: &Sender<CodexRuntimeEvent>,
    reason: RpcDisconnectReason,
) {
    let attachments = mappings
        .lock()
        .expect("Codex mappings mutex poisoned")
        .clear();
    let _ = events.send(CodexRuntimeEvent::Disconnected {
        generation: transport.generation(),
        pid: transport.process_id(),
        attachments,
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderRuntimeController;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Barrier;
    use tempfile::tempdir;

    struct FakeAppServer {
        _directory: tempfile::TempDir,
        program: PathBuf,
        log: PathBuf,
    }

    impl FakeAppServer {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let (program_name, source) = if cfg!(windows) {
                (
                    "fake-codex.cmd",
                    r#"@echo off
 powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$counter=0; $turnCounter=0; while($null -ne ($line=[Console]::In.ReadLine())) { Add-Content -LiteralPath $env:FAKE_CODEX_LOG -Value $line; $request=$line | ConvertFrom-Json; if($null -eq $request.id) { continue }; if($request.method -eq 'initialize') { $result=@{userAgent='fake-codex';codexHome='fake-home';platformFamily='windows';platformOs='windows'} } elseif($request.method -eq 'thread/start') { $counter++; $result=@{thread=@{id=('codex-thread-' + $counter)}} } elseif($request.method -eq 'thread/resume') { $result=@{thread=@{id=$request.params.threadId}} } elseif($request.method -eq 'turn/start') { $turnCounter++; $threadId=$request.params.threadId; $turnId=('provider-turn-' + $turnCounter); $started=@{method='turn/started';params=@{threadId=$threadId;turn=@{id=$turnId;status='inProgress'}}} | ConvertTo-Json -Compress -Depth 10; [Console]::Out.WriteLine($started); $delta=@{method='item/agentMessage/delta';params=@{threadId=$threadId;turnId=$turnId;itemId=('item-' + $turnCounter);delta='hello'}} | ConvertTo-Json -Compress -Depth 10; [Console]::Out.WriteLine($delta); $completed=@{method='turn/completed';params=@{threadId=$threadId;turn=@{id=$turnId;status='completed';items=@(@{id=('item-' + $turnCounter);type='agentMessage';text='hello'})}}} | ConvertTo-Json -Compress -Depth 10; [Console]::Out.WriteLine($completed); [Console]::Out.Flush(); $result=@{turn=@{id=$turnId}} } else { $result=@{} }; $response=@{id=$request.id;result=$result} | ConvertTo-Json -Compress -Depth 10; [Console]::Out.WriteLine($response); [Console]::Out.Flush() }"
"#,
                )
            } else {
                (
                    "fake-codex.sh",
                    r#"#!/bin/sh
 counter=0
 turn_counter=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$FAKE_CODEX_LOG"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{"userAgent":"fake-codex","codexHome":"fake-home","platformFamily":"unix","platformOs":"unix"}}\n' "$id" ;;
    *'"method":"initialized"'*) : ;;
    *'"method":"thread/start"'*) counter=$((counter + 1)); printf '{"id":%s,"result":{"thread":{"id":"codex-thread-%s"}}}\n' "$id" "$counter" ;;
     *'"method":"thread/resume"'*) thread_id=$(printf '%s' "$line" | sed -n 's/.*"threadId":"\([^"]*\)".*/\1/p'); printf '{"id":%s,"result":{"thread":{"id":"%s"}}}\n' "$id" "$thread_id" ;;
     *'"method":"turn/start"'*) turn_counter=$((turn_counter + 1)); thread_id=$(printf '%s' "$line" | sed -n 's/.*"threadId":"\([^"]*\)".*/\1/p'); turn_id="provider-turn-$turn_counter"; item_id="item-$turn_counter"; printf '{"method":"turn/started","params":{"threadId":"%s","turn":{"id":"%s","status":"inProgress"}}}\n' "$thread_id" "$turn_id"; printf '{"method":"item/agentMessage/delta","params":{"threadId":"%s","turnId":"%s","itemId":"%s","delta":"hello"}}\n' "$thread_id" "$turn_id" "$item_id"; printf '{"method":"turn/completed","params":{"threadId":"%s","turn":{"id":"%s","status":"completed","items":[{"id":"%s","type":"agentMessage","text":"hello"}]}}}\n' "$thread_id" "$turn_id" "$item_id"; printf '{"id":%s,"result":{"turn":{"id":"%s"}}}\n' "$id" "$turn_id" ;;
    *) printf '{"id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#,
                )
            };
            let program = directory.path().join(program_name);
            fs::write(&program, source).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            let log = directory.path().join("requests.jsonl");
            Self {
                _directory: directory,
                program,
                log,
            }
        }

        fn launch_config(&self) -> CodexRuntimeLaunchConfig {
            CodexRuntimeLaunchConfig::new(&self.program, self._directory.path())
                .with_env("FAKE_CODEX_LOG", self.log.to_string_lossy().into_owned())
        }

        fn methods(&self) -> Vec<String> {
            let contents = fs::read_to_string(&self.log).unwrap_or_default();
            contents
                .lines()
                .filter_map(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|request| request["method"].as_str().map(str::to_string))
                })
                .collect()
        }
    }

    fn protocol_records(workspace: &Path, thread_id: &str) -> Vec<Value> {
        let log_dir = workspace.join(".pedelec-runtime").join("logs");
        let path = fs::read_dir(log_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(&format!("-{thread_id}-")))
            })
            .unwrap();
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_protocol_request_has_response(records: &[Value], method: &str) {
        let request = records
            .iter()
            .find(|record| {
                record["direction"] == "client_to_provider"
                    && record["kind"] == "request"
                    && record["message"]["method"] == method
            })
            .unwrap_or_else(|| panic!("missing protocol request for {method}"));
        let request_id = request["message"]["id"].clone();
        assert!(
            records.iter().any(|record| {
                record["direction"] == "provider_to_client"
                    && record["kind"] == "response"
                    && record["message"]["id"] == request_id
            }),
            "missing protocol response for {method}"
        );
    }

    #[test]
    fn start_params_are_typed_and_do_not_include_legacy_cli_args() {
        let params = build_thread_start_params(&CodexSessionConfig {
            model: Some("gpt-test".into()),
            effort: Some(CodexReasoningEffort::High),
            cwd: PathBuf::from("C:/workspace/project"),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "Pedelec context".into(),
            config: HashMap::from([("skills.include_instructions".into(), json!(false))]),
        });
        assert_eq!(params["model"], json!("gpt-test"));
        assert_eq!(params["cwd"], json!("C:/workspace/project"));
        assert_eq!(params["approvalPolicy"], json!("never"));
        assert_eq!(params["sandbox"], json!("read-only"));
        assert_eq!(
            params["config"]["skills.include_instructions"],
            json!(false)
        );
        assert!(!params.to_string().contains("exec"));
        assert!(!params.to_string().contains("-c"));
    }

    #[test]
    fn resume_params_reassert_the_full_session_configuration() {
        let params = build_thread_resume_params(
            "codex-thread",
            &CodexSessionConfig {
                model: Some("gpt-test".into()),
                effort: None,
                cwd: PathBuf::from("/workspace/project"),
                approval_policy: CodexApprovalPolicy::Never,
                sandbox: CodexSandboxMode::ReadOnly,
                developer_instructions: "instructions".into(),
                config: HashMap::from([("skills.include_instructions".into(), json!(false))]),
            },
        );
        assert_eq!(params["threadId"], json!("codex-thread"));
        assert_eq!(params["developerInstructions"], json!("instructions"));
        assert_eq!(params["sandbox"], json!("read-only"));
    }

    #[test]
    fn turn_params_use_raw_text_and_reassert_full_access_policy() {
        let params = build_turn_start_params(
            "codex-thread",
            &CodexTurnConfig {
                input: "hello\nworld".into(),
                cwd: PathBuf::from("C:/workspace/project"),
                model: Some("gpt-test".into()),
                effort: Some(CodexReasoningEffort::High),
                approval_policy: CodexApprovalPolicy::Never,
                sandbox_policy: CodexTurnSandboxPolicy::DangerFullAccess,
            },
        );
        assert_eq!(params["threadId"], json!("codex-thread"));
        assert_eq!(
            params["input"],
            json!([{ "type": "text", "text": "hello\nworld" }])
        );
        assert_eq!(params["cwd"], json!("C:/workspace/project"));
        assert_eq!(params["model"], json!("gpt-test"));
        assert_eq!(params["effort"], json!("high"));
        assert_eq!(params["approvalPolicy"], json!("never"));
        assert_eq!(
            params["sandboxPolicy"],
            json!({ "type": "dangerFullAccess" })
        );
        assert!(!params.to_string().contains("[User Message]"));
    }

    #[test]
    fn launch_process_spec_is_provider_scoped_and_has_no_thread_values() {
        let config = CodexRuntimeLaunchConfig::new("codex", tempdir().unwrap().path())
            .with_env("PEDELEC_PROVIDER", "codex")
            .with_env("PEDELEC_CORE_IPC_RUNTIME_FILE", "runtime.json");
        let spec = config.process_spec();
        assert_eq!(spec.args, vec![OsString::from("app-server")]);
        assert!(spec.env_remove.iter().any(|key| key == "PEDELEC_THREAD_ID"));
        assert!(spec
            .env_remove
            .iter()
            .any(|key| key == "PEDELEC_WORKSPACE_PATH"));
        assert!(!spec
            .env
            .iter()
            .any(|(key, _)| { key == "PEDELEC_THREAD_ID" || key == "PEDELEC_WORKSPACE_PATH" }));
    }

    #[test]
    fn response_identity_comes_only_from_result_thread_id() {
        let result = json!({ "thread": { "id": "codex-thread" }, "threadId": "wrong" });
        assert_eq!(
            parse_provider_thread_id("thread/start", &result).unwrap(),
            "codex-thread"
        );
        assert!(parse_provider_thread_id("thread/start", &json!({"threadId":"wrong"})).is_err());
    }

    #[test]
    fn mappings_are_bidirectional_and_reject_collisions() {
        let mut mappings = SessionMappings::default();
        mappings.register("pedelec-a", "codex-a").unwrap();
        assert_eq!(
            mappings.provider_to_pedelec.get("codex-a"),
            Some(&"pedelec-a".to_string())
        );
        assert!(mappings.register("pedelec-b", "codex-a").is_err());
        mappings.remove_pedelec("pedelec-a");
        assert!(mappings.provider_to_pedelec.is_empty());
    }

    #[test]
    fn notification_thread_id_supports_direct_and_nested_turn_shapes() {
        assert_eq!(
            notification_thread_id(&json!({"threadId":"direct"}), None).as_deref(),
            Some("direct")
        );
        let params = json!({"turn":{"threadId":"nested"}});
        assert_eq!(
            notification_thread_id(&params, params.get("turn")).as_deref(),
            Some("nested")
        );
    }

    #[test]
    fn notification_decoder_maps_deltas_and_terminal_completion() {
        let mappings = Arc::new(Mutex::new(SessionMappings::default()));
        {
            let mut mappings = mappings.lock().unwrap();
            mappings.register("pedelec-a", "codex-a").unwrap();
            mappings
                .register_pending_turn("pedelec-a", "codex-a", "local-a")
                .unwrap();
        }

        let started = decode_codex_notification(
            &mappings,
            "turn/started",
            &json!({
                "threadId": "codex-a",
                "turn": { "id": "provider-turn-a", "status": "inProgress" }
            }),
        );
        assert_eq!(
            started,
            vec![CodexRuntimeEvent::TurnStarted {
                pedelec_thread_id: "pedelec-a".into(),
                provider_thread_id: "codex-a".into(),
                provider_turn_id: "provider-turn-a".into(),
            }]
        );

        let delta = decode_codex_notification(
            &mappings,
            "item/agentMessage/delta",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "itemId": "item-a",
                "delta": "hello"
            }),
        );
        assert!(matches!(
            delta.as_slice(),
            [CodexRuntimeEvent::AssistantDelta { text, .. }] if text == "hello"
        ));
        let whitespace_delta = decode_codex_notification(
            &mappings,
            "item/agentMessage/delta",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "itemId": "item-a",
                "delta": " world"
            }),
        );
        assert!(matches!(
            whitespace_delta.as_slice(),
            [CodexRuntimeEvent::AssistantDelta { text, .. }] if text == " world"
        ));

        let terminal_item = decode_codex_notification(
            &mappings,
            "item/completed",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "item": { "id": "item-a", "type": "agentMessage", "text": "hello world!" }
            }),
        );
        assert!(matches!(
            terminal_item.as_slice(),
            [CodexRuntimeEvent::AssistantMessage { text, .. }] if text == "hello world!"
        ));

        let completed = decode_codex_notification(
            &mappings,
            "turn/completed",
            &json!({
                "threadId": "codex-a",
                "turn": {
                    "id": "provider-turn-a",
                    "status": "completed",
                    "items": [
                        { "id": "item-a", "type": "agentMessage", "text": "hello world!" }
                    ]
                }
            }),
        );
        assert!(matches!(
            completed.as_slice(),
            [CodexRuntimeEvent::TurnCompleted {
                status: CodexTurnStatus::Completed,
                ..
            }]
        ));
        assert!(decode_codex_notification(
            &mappings,
            "turn/completed",
            &json!({
                "threadId": "codex-a",
                "turn": { "id": "provider-turn-a", "status": "completed" }
            }),
        )
        .is_empty());
    }

    #[test]
    fn turn_completed_items_emit_full_assistant_message_as_fallback() {
        let mappings = Arc::new(Mutex::new(SessionMappings::default()));
        {
            let mut mappings = mappings.lock().unwrap();
            mappings.register("pedelec-a", "codex-a").unwrap();
            mappings
                .register_pending_turn("pedelec-a", "codex-a", "local-a")
                .unwrap();
        }

        let _ = decode_codex_notification(
            &mappings,
            "turn/started",
            &json!({
                "threadId": "codex-a",
                "turn": { "id": "provider-turn-a", "status": "inProgress" }
            }),
        );
        let delta = decode_codex_notification(
            &mappings,
            "item/agentMessage/delta",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "itemId": "item-a",
                "delta": "hello wor"
            }),
        );
        assert!(matches!(
            delta.as_slice(),
            [CodexRuntimeEvent::AssistantDelta { text, .. }] if text == "hello wor"
        ));

        let completed = decode_codex_notification(
            &mappings,
            "turn/completed",
            &json!({
                "threadId": "codex-a",
                "turn": {
                    "id": "provider-turn-a",
                    "status": "completed",
                    "items": [
                        { "id": "item-a", "type": "agentMessage", "text": "hello world!" }
                    ]
                }
            }),
        );
        assert!(matches!(
            completed.as_slice(),
            [
                CodexRuntimeEvent::AssistantMessage { text, .. },
                CodexRuntimeEvent::TurnCompleted { status: CodexTurnStatus::Completed, .. }
            ] if text == "hello world!"
        ));
    }

    #[test]
    fn final_message_fallback_deduplicates_when_item_id_presence_differs() {
        fn started_mappings() -> Arc<Mutex<SessionMappings>> {
            let mappings = Arc::new(Mutex::new(SessionMappings::default()));
            {
                let mut mappings = mappings.lock().unwrap();
                mappings.register("pedelec-a", "codex-a").unwrap();
                mappings
                    .register_pending_turn("pedelec-a", "codex-a", "local-a")
                    .unwrap();
            }
            let _ = decode_codex_notification(
                &mappings,
                "turn/started",
                &json!({
                    "threadId": "codex-a",
                    "turn": { "id": "provider-turn-a", "status": "inProgress" }
                }),
            );
            mappings
        }

        let mappings = started_mappings();
        let item_completed = decode_codex_notification(
            &mappings,
            "item/completed",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "item": { "type": "agentMessage", "text": "hello world" }
            }),
        );
        assert!(matches!(
            item_completed.as_slice(),
            [CodexRuntimeEvent::AssistantMessage { text, .. }] if text == "hello world"
        ));
        let turn_completed = decode_codex_notification(
            &mappings,
            "turn/completed",
            &json!({
                "threadId": "codex-a",
                "turn": {
                    "id": "provider-turn-a",
                    "status": "completed",
                    "items": [
                        { "id": "item-a", "type": "agentMessage", "text": "hello world" }
                    ]
                }
            }),
        );
        assert!(matches!(
            turn_completed.as_slice(),
            [CodexRuntimeEvent::TurnCompleted {
                status: CodexTurnStatus::Completed,
                ..
            }]
        ));

        let mappings = started_mappings();
        let item_completed = decode_codex_notification(
            &mappings,
            "item/completed",
            &json!({
                "threadId": "codex-a",
                "turnId": "provider-turn-a",
                "item": { "id": "item-b", "type": "agentMessage", "text": "same answer" }
            }),
        );
        assert!(matches!(
            item_completed.as_slice(),
            [CodexRuntimeEvent::AssistantMessage { text, .. }] if text == "same answer"
        ));
        let turn_completed = decode_codex_notification(
            &mappings,
            "turn/completed",
            &json!({
                "threadId": "codex-a",
                "turn": {
                    "id": "provider-turn-a",
                    "status": "completed",
                    "items": [
                        { "type": "agentMessage", "text": "same answer" }
                    ]
                }
            }),
        );
        assert!(matches!(
            turn_completed.as_slice(),
            [CodexRuntimeEvent::TurnCompleted {
                status: CodexTurnStatus::Completed,
                ..
            }]
        ));
    }

    #[test]
    fn successful_initialize_without_server_info_reaches_thread_start() {
        let fixture = FakeAppServer::new();
        let controller = CodexAppServerController::spawn(fixture.launch_config()).unwrap();
        controller
            .ensure_session(
                "pedelec-a",
                None,
                &CodexSessionConfig {
                    model: None,
                    effort: None,
                    cwd: fixture._directory.path().to_path_buf(),
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox: CodexSandboxMode::ReadOnly,
                    developer_instructions: String::new(),
                    config: HashMap::new(),
                },
            )
            .unwrap();

        assert_eq!(
            fixture.methods(),
            vec!["initialize", "initialized", "thread/start"]
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn fake_app_server_handshake_and_sessions_share_one_process() {
        let fixture = FakeAppServer::new();
        let controller = CodexAppServerController::spawn(fixture.launch_config()).unwrap();
        let process_id = controller.process_id();
        let config = CodexSessionConfig {
            model: Some("gpt-test".into()),
            effort: Some(CodexReasoningEffort::High),
            cwd: fixture._directory.path().join("workspace"),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "Pedelec instructions".into(),
            config: HashMap::from([("skills.include_instructions".into(), json!(false))]),
        };

        let first = controller
            .ensure_session("pedelec-a", None, &config)
            .unwrap();
        assert_eq!(first.provider_thread_id, "codex-thread-1");
        assert!(!first.already_loaded);
        assert_eq!(controller.process_id(), process_id);

        let already_loaded = controller
            .ensure_session("pedelec-a", Some("codex-thread-1"), &config)
            .unwrap();
        assert!(already_loaded.already_loaded);
        assert_eq!(
            fixture.methods(),
            vec!["initialize", "initialized", "thread/start"]
        );

        let second = controller
            .ensure_session("pedelec-b", None, &config)
            .unwrap();
        assert_eq!(second.provider_thread_id, "codex-thread-2");
        assert_eq!(controller.process_id(), process_id);
        assert_eq!(
            controller.loaded_provider_thread_id("pedelec-b").as_deref(),
            Some("codex-thread-2")
        );
        assert_eq!(
            controller
                .loaded_pedelec_thread_id("codex-thread-1")
                .as_deref(),
            Some("pedelec-a")
        );
        assert_eq!(
            fixture.methods(),
            vec!["initialize", "initialized", "thread/start", "thread/start"]
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn turn_stream_is_authoritative_even_when_notifications_precede_response() {
        let fixture = FakeAppServer::new();
        let controller = CodexAppServerController::spawn(fixture.launch_config()).unwrap();
        let session = controller
            .ensure_session(
                "pedelec-a",
                None,
                &CodexSessionConfig {
                    model: Some("gpt-test".into()),
                    effort: Some(CodexReasoningEffort::High),
                    cwd: fixture._directory.path().to_path_buf(),
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox: CodexSandboxMode::ReadOnly,
                    developer_instructions: "instructions".into(),
                    config: HashMap::new(),
                },
            )
            .unwrap();

        let result = controller
            .start_turn(
                "pedelec-a",
                &session.provider_thread_id,
                "local-a",
                &CodexTurnConfig {
                    input: "actual user text".into(),
                    cwd: fixture._directory.path().to_path_buf(),
                    model: Some("gpt-test".into()),
                    effort: Some(CodexReasoningEffort::High),
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox_policy: CodexTurnSandboxPolicy::DangerFullAccess,
                },
            )
            .unwrap();
        assert!(result.response_received);

        let mut events = Vec::new();
        for _ in 0..4 {
            events.push(
                controller
                    .recv_event_timeout(Duration::from_secs(1))
                    .unwrap(),
            );
        }
        assert!(matches!(
            events.as_slice(),
            [
                CodexRuntimeEvent::TurnStarted { provider_turn_id, .. },
                CodexRuntimeEvent::AssistantDelta { text: delta, .. },
                CodexRuntimeEvent::AssistantMessage { text: message, .. },
                CodexRuntimeEvent::TurnCompleted { status: CodexTurnStatus::Completed, .. }
            ] if provider_turn_id == "provider-turn-1" && delta == "hello" && message == "hello"
        ));

        let requests = fs::read_to_string(&fixture.log).unwrap();
        let turn_request = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|request| request["method"] == "turn/start")
            .unwrap();
        assert_eq!(
            turn_request["params"]["input"],
            json!([{ "type": "text", "text": "actual user text" }])
        );
        assert_eq!(
            turn_request["params"]["sandboxPolicy"],
            json!({ "type": "dangerFullAccess" })
        );
        let records = protocol_records(fixture._directory.path(), "pedelec-a");
        assert_protocol_request_has_response(&records, "thread/start");
        assert_protocol_request_has_response(&records, "turn/start");
        assert!(records.iter().any(|record| {
            record["direction"] == "provider_to_client"
                && record["kind"] == "notification"
                && record["message"]["method"] == "turn/started"
        }));
        assert!(records.iter().any(|record| {
            record["direction"] == "provider_to_client"
                && record["kind"] == "notification"
                && record["message"]["method"] == "item/agentMessage/delta"
        }));
        assert!(records.iter().any(|record| {
            record["direction"] == "provider_to_client"
                && record["kind"] == "notification"
                && record["message"]["method"] == "turn/completed"
        }));
        controller.shutdown().unwrap();
    }

    #[test]
    fn interrupt_waits_for_the_interrupted_terminal_event() {
        let fixture = FakeAppServer::new();
        let mut launch = fixture.launch_config();
        launch.control_timeout = Duration::from_secs(1);
        let controller = CodexAppServerController::spawn(launch).unwrap();
        let config = CodexSessionConfig {
            model: None,
            effort: None,
            cwd: fixture._directory.path().to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "instructions".into(),
            config: HashMap::new(),
        };
        let session = controller
            .ensure_session("pedelec-a", None, &config)
            .unwrap();
        {
            let mut mappings = controller.mappings.lock().unwrap();
            mappings
                .register_pending_turn("pedelec-a", &session.provider_thread_id, "local-a")
                .unwrap();
            mappings
                .pending_turns
                .get_mut("pedelec-a")
                .unwrap()
                .provider_turn_id = Some("provider-turn-interrupt".into());
        }

        let interrupt_controller = Arc::clone(&controller);
        let interrupt = thread::spawn(move || {
            interrupt_controller.interrupt_turn("pedelec-a", Some("local-a"))
        });
        thread::sleep(Duration::from_millis(50));
        {
            let mut mappings = controller.mappings.lock().unwrap();
            let pending = mappings.pending_turns.get("pedelec-a").unwrap().clone();
            mappings.remember_completed(&pending, CodexTurnStatus::Interrupted);
            mappings.pending_turns.remove("pedelec-a");
        }
        assert!(interrupt.join().unwrap().is_ok());
        assert_eq!(
            fixture.methods(),
            vec![
                "initialize",
                "initialized",
                "thread/start",
                "turn/interrupt"
            ]
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn interrupt_response_without_terminal_event_is_not_confirmation() {
        let fixture = FakeAppServer::new();
        let mut launch = fixture.launch_config();
        launch.control_timeout = Duration::from_secs(1);
        let controller = CodexAppServerController::spawn(launch).unwrap();
        let config = CodexSessionConfig {
            model: None,
            effort: None,
            cwd: fixture._directory.path().to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "instructions".into(),
            config: HashMap::new(),
        };
        let session = controller
            .ensure_session("pedelec-a", None, &config)
            .unwrap();
        {
            let mut mappings = controller.mappings.lock().unwrap();
            mappings
                .register_pending_turn("pedelec-a", &session.provider_thread_id, "local-a")
                .unwrap();
            mappings
                .pending_turns
                .get_mut("pedelec-a")
                .unwrap()
                .provider_turn_id = Some("provider-turn-interrupt".into());
        }

        let error = controller
            .interrupt_turn("pedelec-a", Some("local-a"))
            .unwrap_err();
        assert!(matches!(
            error,
            CodexRuntimeError::Request { operation, message, .. }
                if operation == "turn/interrupt"
                    && message.contains("timed out waiting for interrupted turn completion")
        ));
        assert!(!controller.is_healthy());
        controller.shutdown().unwrap();
    }

    #[test]
    fn normal_end_interrupts_then_unsubscribes_the_loaded_session() {
        let fixture = FakeAppServer::new();
        let mut launch = fixture.launch_config();
        launch.control_timeout = Duration::from_secs(1);
        let controller = CodexAppServerController::spawn(launch).unwrap();
        let config = CodexSessionConfig {
            model: None,
            effort: None,
            cwd: fixture._directory.path().to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "instructions".into(),
            config: HashMap::new(),
        };
        let session = controller
            .ensure_session("pedelec-a", None, &config)
            .unwrap();
        {
            let mut mappings = controller.mappings.lock().unwrap();
            mappings
                .register_pending_turn("pedelec-a", &session.provider_thread_id, "local-a")
                .unwrap();
            mappings
                .pending_turns
                .get_mut("pedelec-a")
                .unwrap()
                .provider_turn_id = Some("provider-turn-interrupt".into());
            mappings.completed_provider_turn_status.insert(
                (
                    session.provider_thread_id.clone(),
                    "provider-turn-interrupt".into(),
                ),
                CodexTurnStatus::Interrupted,
            );
        }

        controller
            .end_session("pedelec-a", Some("local-a"))
            .unwrap();
        assert_eq!(controller.loaded_provider_thread_id("pedelec-a"), None);
        assert_eq!(
            fixture.methods(),
            vec![
                "initialize",
                "initialized",
                "thread/start",
                "turn/interrupt",
                "thread/unsubscribe"
            ]
        );
        let records = protocol_records(fixture._directory.path(), "pedelec-a");
        assert_protocol_request_has_response(&records, "turn/interrupt");
        assert_protocol_request_has_response(&records, "thread/unsubscribe");
        controller.shutdown().unwrap();
    }

    #[test]
    fn replacement_runtime_resumes_detached_provider_session() {
        let first_fixture = FakeAppServer::new();
        let first = CodexAppServerController::spawn(first_fixture.launch_config()).unwrap();
        let config = CodexSessionConfig {
            model: None,
            effort: None,
            cwd: first_fixture._directory.path().to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: "instructions".into(),
            config: HashMap::new(),
        };
        let session = first.ensure_session("pedelec-a", None, &config).unwrap();
        first.shutdown().unwrap();

        let second_fixture = FakeAppServer::new();
        let second = CodexAppServerController::spawn(second_fixture.launch_config()).unwrap();
        let resumed = second
            .ensure_session("pedelec-a", Some(&session.provider_thread_id), &config)
            .unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.provider_thread_id, session.provider_thread_id);
        assert_eq!(
            second_fixture.methods(),
            vec!["initialize", "initialized", "thread/resume"]
        );
        second.shutdown().unwrap();
    }

    #[test]
    fn concurrent_sessions_use_distinct_ids_on_one_runtime() {
        let fixture = FakeAppServer::new();
        let controller = CodexAppServerController::spawn(fixture.launch_config()).unwrap();
        let process_id = controller.process_id();
        let config = CodexSessionConfig {
            model: None,
            effort: None,
            cwd: fixture._directory.path().to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: String::new(),
            config: HashMap::new(),
        };
        let barrier = Arc::new(Barrier::new(3));
        let first_controller = Arc::clone(&controller);
        let first_barrier = Arc::clone(&barrier);
        let first_config = config.clone();
        let first = thread::spawn(move || {
            first_barrier.wait();
            first_controller
                .ensure_session("pedelec-a", None, &first_config)
                .unwrap()
        });
        let second_controller = Arc::clone(&controller);
        let second_barrier = Arc::clone(&barrier);
        let second_config = config.clone();
        let second = thread::spawn(move || {
            second_barrier.wait();
            second_controller
                .ensure_session("pedelec-b", None, &second_config)
                .unwrap()
        });
        barrier.wait();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_ne!(first.provider_thread_id, second.provider_thread_id);
        assert_eq!(controller.process_id(), process_id);
        assert_eq!(
            fixture
                .methods()
                .into_iter()
                .filter(|method| method == "initialize")
                .count(),
            1
        );
        assert_eq!(
            fixture
                .methods()
                .into_iter()
                .filter(|method| method == "initialized")
                .count(),
            1
        );
        assert_eq!(
            fixture
                .methods()
                .into_iter()
                .filter(|method| method == "thread/start")
                .count(),
            2
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn invalid_session_config_is_rejected_before_rpc() {
        let config = CodexSessionConfig {
            model: Some("   ".into()),
            effort: None,
            cwd: Path::new("").to_path_buf(),
            approval_policy: CodexApprovalPolicy::Never,
            sandbox: CodexSandboxMode::ReadOnly,
            developer_instructions: String::new(),
            config: HashMap::new(),
        };
        assert!(matches!(
            config.validate(),
            Err(CodexRuntimeError::Protocol { operation, .. }) if operation == "session-config"
        ));
    }
}
