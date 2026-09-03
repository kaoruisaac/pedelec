//! Reusable Desktop controller for one `pedelec-agent serve --provider <product>`
//! process generation.
//!
//! This module owns JSON-RPC 2.0 method names, session/turn correlation, and
//! normalized runtime events. It does not hard-code Core provider routing.

use crate::{
    PersistentProcessSpec, PersistentRuntimeController, ProtocolTrafficRecord,
    ProviderRuntimeController, RpcDisconnectReason, RpcEnvelopeMode, RpcError, RpcEvent,
    RuntimeControllerError, RuntimeEvent,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const PEDELEC_AGENT_PROTOCOL_VERSION: u32 = 1;
pub const PEDELEC_AGENT_SERVER_NAME: &str = "pedelec-agent";
pub const DEFAULT_PEDELEC_AGENT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_PEDELEC_AGENT_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PEDELEC_AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const FORMAL_TURN_METHODS: &[&str] = &[
    "turn/started",
    "turn/assistant_delta",
    "turn/assistant_message",
    "turn/usage",
    "turn/completed",
];

#[derive(Debug, Clone, PartialEq)]
pub struct PedelecAgentRuntimeLaunchConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub process_cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub max_frame_bytes: usize,
    pub control_timeout: Duration,
    pub graceful_shutdown_timeout: Duration,
    pub provider: String,
    pub client_name: String,
    pub client_version: String,
}

impl PedelecAgentRuntimeLaunchConfig {
    pub fn new(
        program: impl Into<PathBuf>,
        process_cwd: impl Into<PathBuf>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            process_cwd: process_cwd.into(),
            env: Vec::new(),
            max_frame_bytes: DEFAULT_PEDELEC_AGENT_MAX_FRAME_BYTES,
            control_timeout: DEFAULT_PEDELEC_AGENT_CONTROL_TIMEOUT,
            graceful_shutdown_timeout: DEFAULT_PEDELEC_AGENT_SHUTDOWN_TIMEOUT,
            provider: provider.into(),
            client_name: "pedelec-desktop".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
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
                    .args(self.args.clone())
                    .args(["serve", "--provider"])
                    .arg(self.provider.clone())
                    .cwd(self.process_cwd.clone())
                    .env_remove("PEDELEC_THREAD_ID")
                    .env_remove("PEDELEC_WORKSPACE_PATH")
                    .with_envs(&self.env);
            }
        }

        PersistentProcessSpec::new(self.program.clone())
            .args(self.args.clone())
            .args(["serve", "--provider"])
            .arg(self.provider.clone())
            .cwd(self.process_cwd.clone())
            .env_remove("PEDELEC_THREAD_ID")
            .env_remove("PEDELEC_WORKSPACE_PATH")
            .with_envs(&self.env)
    }

    fn initialize_params(&self) -> Value {
        json!({
            "protocolVersion": PEDELEC_AGENT_PROTOCOL_VERSION,
            "clientInfo": {
                "name": self.client_name,
                "version": self.client_version,
            }
        })
    }
}

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
pub struct PedelecAgentSessionConfig {
    pub model: String,
    pub workspace: PathBuf,
    pub host_instructions: Option<String>,
}

impl PedelecAgentSessionConfig {
    fn validate(&self) -> Result<(), PedelecAgentRuntimeError> {
        if self.model.trim().is_empty() {
            return Err(protocol_error(
                "session-config",
                "Pedelec Agent session model is empty",
            ));
        }
        if self.workspace.as_os_str().is_empty() {
            return Err(protocol_error(
                "session-config",
                "Pedelec Agent session workspace is empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PedelecAgentModelCapabilities {
    pub tools: bool,
    pub vision: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PedelecAgentSessionResult {
    pub agent_session_id: String,
    pub resumed: bool,
    pub already_loaded: bool,
    pub already_attached: bool,
    pub model_capabilities: PedelecAgentModelCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PedelecAgentTurnStartResult {
    pub turn_id: String,
    pub already_started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PedelecAgentSessionAttachment {
    pub pedelec_thread_id: String,
    pub agent_session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PedelecAgentTurnStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PedelecAgentCloseOutcome {
    AlreadyDetached,
    Closed,
    GenerationRetired { message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PedelecAgentRuntimeEvent {
    TurnStarted {
        pedelec_thread_id: String,
        agent_session_id: String,
        turn_id: String,
    },
    AssistantDelta {
        pedelec_thread_id: String,
        agent_session_id: String,
        turn_id: String,
        text: String,
    },
    AssistantMessage {
        pedelec_thread_id: String,
        agent_session_id: String,
        turn_id: String,
        text: String,
    },
    UsageUpdated {
        pedelec_thread_id: String,
        agent_session_id: String,
        turn_id: String,
        usage: Value,
    },
    TurnCompleted {
        pedelec_thread_id: String,
        agent_session_id: String,
        turn_id: String,
        status: PedelecAgentTurnStatus,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Value,
        pedelec_thread_id: Option<String>,
        agent_session_id: Option<String>,
        turn_id: Option<String>,
    },
    ProtocolError {
        pedelec_thread_id: Option<String>,
        agent_session_id: Option<String>,
        operation: String,
        message: String,
    },
    Disconnected {
        generation: u64,
        pid: u32,
        attachments: Vec<PedelecAgentSessionAttachment>,
        reason: RpcDisconnectReason,
    },
    Stderr {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PedelecAgentRuntimeError {
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

impl fmt::Display for PedelecAgentRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (operation, message) = match self {
            Self::RuntimeStart { operation, message }
            | Self::RuntimeDisconnected { operation, message }
            | Self::Protocol { operation, message }
            | Self::Request {
                operation, message, ..
            } => (operation, message),
        };
        write!(f, "Pedelec Agent server {operation} failed: {message}")
    }
}

impl std::error::Error for PedelecAgentRuntimeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionMeta {
    model_capabilities: PedelecAgentModelCapabilities,
    resumed: bool,
    model: String,
    workspace: PathBuf,
    host_instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTurn {
    session_id: String,
    local_turn_id: String,
}

#[derive(Debug, Default)]
struct SessionMappings {
    session_to_thread: HashMap<String, String>,
    thread_to_session: HashMap<String, String>,
    session_meta: HashMap<String, SessionMeta>,
    thread_locks: HashMap<String, Arc<Mutex<()>>>,
    session_locks: HashMap<String, Arc<Mutex<()>>>,
    pending_turns: HashMap<String, PendingTurn>,
}

impl SessionMappings {
    fn lock_for_thread(&mut self, pedelec_thread_id: &str) -> Arc<Mutex<()>> {
        self.thread_locks
            .entry(pedelec_thread_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn lock_for_session(&mut self, agent_session_id: &str) -> Arc<Mutex<()>> {
        self.session_locks
            .entry(agent_session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn attachments(&self) -> Vec<PedelecAgentSessionAttachment> {
        self.thread_to_session
            .iter()
            .map(
                |(pedelec_thread_id, agent_session_id)| PedelecAgentSessionAttachment {
                    pedelec_thread_id: pedelec_thread_id.clone(),
                    agent_session_id: agent_session_id.clone(),
                },
            )
            .collect()
    }

    fn clear(&mut self) -> Vec<PedelecAgentSessionAttachment> {
        let attachments = self.attachments();
        self.session_to_thread.clear();
        self.thread_to_session.clear();
        self.session_meta.clear();
        self.thread_locks.clear();
        self.session_locks.clear();
        self.pending_turns.clear();
        attachments
    }

    fn register(
        &mut self,
        pedelec_thread_id: &str,
        agent_session_id: &str,
        meta: SessionMeta,
    ) -> Result<(), PedelecAgentRuntimeError> {
        if let Some(existing) = self.thread_to_session.get(pedelec_thread_id) {
            if existing != agent_session_id {
                return Err(protocol_error(
                    "session-mapping",
                    format!("Pedelec thread {pedelec_thread_id} is already attached to {existing}"),
                ));
            }
        }
        if let Some(existing) = self.session_to_thread.get(agent_session_id) {
            if existing != pedelec_thread_id {
                return Err(protocol_error(
                    "session-mapping",
                    format!("Agent session {agent_session_id} is already attached to {existing}"),
                ));
            }
        }
        self.thread_to_session
            .insert(pedelec_thread_id.to_string(), agent_session_id.to_string());
        self.session_to_thread
            .insert(agent_session_id.to_string(), pedelec_thread_id.to_string());
        self.session_meta.insert(agent_session_id.to_string(), meta);
        Ok(())
    }

    fn remove_thread(&mut self, pedelec_thread_id: &str) {
        if let Some(session_id) = self.thread_to_session.remove(pedelec_thread_id) {
            self.session_to_thread.remove(&session_id);
            self.session_meta.remove(&session_id);
        }
        self.pending_turns.remove(pedelec_thread_id);
    }

    #[cfg(test)]
    fn lock_counts(&self) -> (usize, usize) {
        (self.thread_locks.len(), self.session_locks.len())
    }

    fn register_pending_turn(
        &mut self,
        pedelec_thread_id: &str,
        agent_session_id: &str,
        local_turn_id: &str,
    ) -> Result<(), PedelecAgentRuntimeError> {
        if self
            .thread_to_session
            .get(pedelec_thread_id)
            .map(String::as_str)
            != Some(agent_session_id)
        {
            return Err(protocol_error(
                "turn/start",
                format!(
                    "Pedelec thread {pedelec_thread_id} is not attached to session {agent_session_id}"
                ),
            ));
        }
        if let Some(existing) = self.pending_turns.get(pedelec_thread_id) {
            if existing.local_turn_id == local_turn_id && existing.session_id == agent_session_id {
                return Ok(());
            }
            return Err(request_error(
                "turn/start",
                format!("Pedelec thread {pedelec_thread_id} already has an active turn"),
                None,
            ));
        }
        self.pending_turns.insert(
            pedelec_thread_id.to_string(),
            PendingTurn {
                session_id: agent_session_id.to_string(),
                local_turn_id: local_turn_id.to_string(),
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
}

enum MappingLockKind {
    Thread,
    Session,
}

struct MappingLock {
    mappings: Arc<Mutex<SessionMappings>>,
    kind: MappingLockKind,
    key: String,
    lock: Arc<Mutex<()>>,
}

impl MappingLock {
    fn for_thread(mappings: &Arc<Mutex<SessionMappings>>, pedelec_thread_id: &str) -> Self {
        let lock = mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .lock_for_thread(pedelec_thread_id);
        Self {
            mappings: Arc::clone(mappings),
            kind: MappingLockKind::Thread,
            key: pedelec_thread_id.to_string(),
            lock,
        }
    }

    fn for_session(mappings: &Arc<Mutex<SessionMappings>>, agent_session_id: &str) -> Self {
        let lock = mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .lock_for_session(agent_session_id);
        Self {
            mappings: Arc::clone(mappings),
            kind: MappingLockKind::Session,
            key: agent_session_id.to_string(),
            lock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .expect("Pedelec Agent mapping lock poisoned")
    }
}

impl Drop for MappingLock {
    fn drop(&mut self) {
        let mut mappings = self
            .mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned");
        let map = match self.kind {
            MappingLockKind::Thread => &mut mappings.thread_locks,
            MappingLockKind::Session => &mut mappings.session_locks,
        };
        if let Some(current) = map.get(&self.key) {
            if Arc::ptr_eq(current, &self.lock) && Arc::strong_count(current) == 2 {
                map.remove(&self.key);
            }
        }
    }
}

/// One initialized `pedelec-agent` server process shared by many sessions.
pub struct PedelecAgentServerController {
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<SessionMappings>>,
    events: Mutex<Receiver<PedelecAgentRuntimeEvent>>,
    event_worker: Mutex<Option<thread::JoinHandle<()>>>,
    healthy: Arc<AtomicBool>,
    control_timeout: Duration,
    graceful_shutdown_timeout: Duration,
    provider: String,
}

impl fmt::Debug for PedelecAgentServerController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PedelecAgentServerController")
            .field("generation", &self.generation())
            .field("pid", &self.process_id())
            .field("healthy", &self.is_healthy())
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl PedelecAgentServerController {
    pub fn spawn(
        config: PedelecAgentRuntimeLaunchConfig,
    ) -> Result<Arc<Self>, PedelecAgentRuntimeError> {
        if config.program.as_os_str().is_empty() {
            return Err(start_error(
                "spawn",
                "Pedelec Agent executable path is empty",
            ));
        }
        if config.provider.trim().is_empty() {
            return Err(start_error("spawn", "Pedelec Agent provider is empty"));
        }
        let transport = Arc::new(
            PersistentRuntimeController::spawn_with_envelope_mode(
                config.process_spec(),
                config.max_frame_bytes,
                RpcEnvelopeMode::JsonRpc2,
            )
            .map_err(|error| start_error("spawn", error))?,
        );
        let initialize = match transport.request(
            "initialize",
            config.initialize_params(),
            config.control_timeout,
        ) {
            Ok(result) => result,
            Err(error) => {
                let _ = transport.shutdown_with_grace(Duration::from_millis(100));
                return Err(start_error("initialize", error));
            }
        };
        if let Err(error) = validate_initialize_result(&initialize, &config.provider) {
            let _ = transport.shutdown_with_grace(Duration::from_millis(100));
            return Err(error);
        }

        let (events_tx, events_rx) = mpsc::channel();
        let mappings = Arc::new(Mutex::new(SessionMappings::default()));
        transport.set_protocol_owner_resolver({
            let mappings = Arc::clone(&mappings);
            Arc::new(move |frame: &Value| resolve_protocol_owner(&mappings, frame))
        });
        let healthy = Arc::new(AtomicBool::new(true));
        let event_worker = thread::Builder::new()
            .name(format!("pedelec-agent-events-{}", transport.generation()))
            .spawn({
                let transport = Arc::clone(&transport);
                let mappings = Arc::clone(&mappings);
                let healthy = Arc::clone(&healthy);
                move || run_event_worker(transport, mappings, healthy, events_tx)
            })
            .map_err(|error| {
                let _ = transport.shutdown_with_grace(Duration::from_millis(100));
                start_error("event-worker", error)
            })?;

        Ok(Arc::new(Self {
            transport,
            mappings,
            events: Mutex::new(events_rx),
            event_worker: Mutex::new(Some(event_worker)),
            healthy,
            control_timeout: config.control_timeout,
            graceful_shutdown_timeout: config.graceful_shutdown_timeout,
            provider: config.provider,
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

    pub fn loaded_sessions(&self) -> Vec<PedelecAgentSessionAttachment> {
        self.mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .attachments()
    }

    pub fn loaded_session_id(&self, pedelec_thread_id: &str) -> Option<String> {
        self.mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .thread_to_session
            .get(pedelec_thread_id)
            .cloned()
    }

    pub fn ensure_session(
        &self,
        pedelec_thread_id: &str,
        persisted_session_id: Option<&str>,
        config: &PedelecAgentSessionConfig,
    ) -> Result<PedelecAgentSessionResult, PedelecAgentRuntimeError> {
        config.validate()?;
        self.require_healthy("session/open")?;
        if pedelec_thread_id.trim().is_empty() {
            return Err(protocol_error("session/open", "Pedelec thread id is empty"));
        }
        let persisted_session_id = persisted_session_id
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let thread_lock = MappingLock::for_thread(&self.mappings, pedelec_thread_id);
        let _thread_guard = thread_lock.lock();
        let session_lock = persisted_session_id
            .map(|session_id| MappingLock::for_session(&self.mappings, session_id));
        let _session_guard = session_lock.as_ref().map(MappingLock::lock);

        if let Some(fast_path) =
            self.already_loaded_session(pedelec_thread_id, persisted_session_id, config)?
        {
            return Ok(fast_path);
        }
        if let Some(session_id) = persisted_session_id {
            let mapped_thread = self
                .mappings
                .lock()
                .expect("Pedelec Agent mappings mutex poisoned")
                .session_to_thread
                .get(session_id)
                .cloned();
            if let Some(mapped_thread) = mapped_thread {
                if mapped_thread != pedelec_thread_id {
                    self.retire();
                    return Err(protocol_error(
                        "session/open",
                        format!(
                            "Agent session {session_id} is already attached to {mapped_thread}"
                        ),
                    ));
                }
            }
        }

        self.transport
            .register_protocol_log(pedelec_thread_id, &self.provider, &config.workspace);
        let params = session_open_params(pedelec_thread_id, persisted_session_id, config);
        let result = self
            .transport
            .request_scoped(
                pedelec_thread_id,
                "session/open",
                params,
                self.control_timeout,
            )
            .map_err(|error| self.map_request_error("session/open", error))?;
        let parsed = match parse_session_open_result(&result, persisted_session_id) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.retire();
                return Err(error);
            }
        };
        if let Err(error) = self
            .mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .register(
                pedelec_thread_id,
                &parsed.agent_session_id,
                SessionMeta {
                    model_capabilities: parsed.model_capabilities,
                    resumed: parsed.resumed,
                    model: config.model.clone(),
                    workspace: normalize_workspace_identity(&config.workspace),
                    host_instructions: config.host_instructions.clone(),
                },
            )
        {
            self.retire();
            return Err(error);
        }
        Ok(parsed)
    }

    pub fn start_turn(
        &self,
        pedelec_thread_id: &str,
        agent_session_id: &str,
        local_turn_id: &str,
        message: &str,
    ) -> Result<PedelecAgentTurnStartResult, PedelecAgentRuntimeError> {
        self.require_healthy("turn/start")?;
        if pedelec_thread_id.trim().is_empty()
            || agent_session_id.trim().is_empty()
            || local_turn_id.trim().is_empty()
        {
            return Err(protocol_error(
                "turn/start",
                "threadId, sessionId, and turnId are required",
            ));
        }

        let thread_lock = MappingLock::for_thread(&self.mappings, pedelec_thread_id);
        let _thread_guard = thread_lock.lock();
        let session_lock = MappingLock::for_session(&self.mappings, agent_session_id);
        let _session_guard = session_lock.lock();

        {
            let mut mappings = self
                .mappings
                .lock()
                .expect("Pedelec Agent mappings mutex poisoned");
            if let Err(error) =
                mappings.register_pending_turn(pedelec_thread_id, agent_session_id, local_turn_id)
            {
                if matches!(error, PedelecAgentRuntimeError::Protocol { .. }) {
                    drop(mappings);
                    self.retire();
                }
                return Err(error);
            }
        }

        let result = self.transport.request_scoped(
            pedelec_thread_id,
            "turn/start",
            json!({
                "threadId": pedelec_thread_id,
                "sessionId": agent_session_id,
                "turnId": local_turn_id,
                "message": message,
            }),
            self.control_timeout,
        );
        match result {
            Ok(result) => match parse_turn_start_result(&result, local_turn_id) {
                Ok(parsed) => Ok(parsed),
                Err(error) => {
                    self.retire();
                    Err(error)
                }
            },
            Err(RuntimeControllerError::Rpc(RpcError::Remote {
                code,
                message,
                data,
            })) => {
                self.mappings
                    .lock()
                    .expect("Pedelec Agent mappings mutex poisoned")
                    .remove_pending_turn(pedelec_thread_id, local_turn_id);
                Err(remote_request_error("turn/start", code, message, data))
            }
            Err(error) => {
                self.retire();
                Err(self.map_request_error("turn/start", error))
            }
        }
    }

    pub fn close_session(
        &self,
        pedelec_thread_id: &str,
        expected_session_id: Option<&str>,
        active_turn_id: Option<&str>,
    ) -> Result<PedelecAgentCloseOutcome, PedelecAgentRuntimeError> {
        if !self.is_healthy() {
            return Ok(PedelecAgentCloseOutcome::GenerationRetired {
                message: "Pedelec Agent runtime is not healthy".to_string(),
            });
        }
        let thread_lock = MappingLock::for_thread(&self.mappings, pedelec_thread_id);
        let _thread_guard = thread_lock.lock();

        let mapped_session_id = self
            .mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .thread_to_session
            .get(pedelec_thread_id)
            .cloned();
        let Some(agent_session_id) = mapped_session_id else {
            return Ok(PedelecAgentCloseOutcome::AlreadyDetached);
        };
        self.mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .pending_turns
            .remove(pedelec_thread_id);
        if let Some(expected) = expected_session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if expected != agent_session_id {
                self.retire();
                return Err(protocol_error(
                    "session/close",
                    format!(
                        "session/close expected {expected} but thread {pedelec_thread_id} is attached to {agent_session_id}"
                    ),
                ));
            }
        }

        let session_lock = MappingLock::for_session(&self.mappings, &agent_session_id);
        let _session_guard = session_lock.lock();

        let mut params = json!({
            "threadId": pedelec_thread_id,
            "sessionId": agent_session_id,
        });
        if let Some(active_turn_id) = active_turn_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            params["activeTurnId"] = json!(active_turn_id);
        }
        match self.transport.request_scoped(
            pedelec_thread_id,
            "session/close",
            params,
            self.control_timeout,
        ) {
            Ok(result) => {
                if !parse_session_close_result(&result) {
                    self.retire();
                    return Ok(PedelecAgentCloseOutcome::GenerationRetired {
                        message: "session/close returned a malformed success payload".to_string(),
                    });
                }
            }
            Err(RuntimeControllerError::Rpc(RpcError::Remote {
                code,
                message,
                data,
            })) => {
                if !is_idempotent_close_error(&data) {
                    self.retire();
                    return Ok(PedelecAgentCloseOutcome::GenerationRetired {
                        message: format!("session/close remote error: {message}"),
                    });
                }
                let _ = (code, message, data);
            }
            Err(error) => {
                self.retire();
                return Ok(PedelecAgentCloseOutcome::GenerationRetired {
                    message: error.to_string(),
                });
            }
        }

        self.mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .remove_thread(pedelec_thread_id);
        Ok(PedelecAgentCloseOutcome::Closed)
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<PedelecAgentRuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("Pedelec Agent events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn recv_event(&self) -> Result<PedelecAgentRuntimeEvent, mpsc::RecvError> {
        self.events
            .lock()
            .expect("Pedelec Agent events mutex poisoned")
            .recv()
    }

    pub fn recv_protocol_traffic_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProtocolTrafficRecord, RecvTimeoutError> {
        self.transport.recv_protocol_traffic_timeout(timeout)
    }

    pub fn retire(&self) {
        if self.healthy.swap(false, Ordering::AcqRel) {
            self.transport.retire();
        }
    }

    pub fn retire_for_protocol_error(&self) {
        self.retire();
    }

    fn already_loaded_session(
        &self,
        pedelec_thread_id: &str,
        persisted_session_id: Option<&str>,
        config: &PedelecAgentSessionConfig,
    ) -> Result<Option<PedelecAgentSessionResult>, PedelecAgentRuntimeError> {
        let mappings = self
            .mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned");
        let Some(mapped_session_id) = mappings.thread_to_session.get(pedelec_thread_id).cloned()
        else {
            return Ok(None);
        };
        if let Some(requested) = persisted_session_id {
            if requested != mapped_session_id {
                drop(mappings);
                self.retire();
                return Err(protocol_error(
                    "session/open",
                    format!(
                        "Pedelec thread {pedelec_thread_id} is already attached to {mapped_session_id}"
                    ),
                ));
            }
        }
        let Some(meta) = mappings.session_meta.get(&mapped_session_id).cloned() else {
            return Ok(None);
        };
        if meta.model != config.model {
            drop(mappings);
            self.retire();
            return Err(protocol_error(
                "session/open",
                format!(
                    "Agent session {mapped_session_id} is bound to model {} not {}",
                    meta.model, config.model
                ),
            ));
        }
        let requested_workspace = normalize_workspace_identity(&config.workspace);
        if meta.workspace != requested_workspace {
            drop(mappings);
            self.retire();
            return Err(protocol_error(
                "session/open",
                format!(
                    "Agent session {mapped_session_id} is bound to workspace {} not {}",
                    meta.workspace.display(),
                    requested_workspace.display()
                ),
            ));
        }
        if meta.host_instructions != config.host_instructions {
            return Ok(None);
        }
        Ok(Some(PedelecAgentSessionResult {
            agent_session_id: mapped_session_id,
            resumed: meta.resumed,
            already_loaded: true,
            already_attached: true,
            model_capabilities: meta.model_capabilities,
        }))
    }

    fn require_healthy(&self, operation: &str) -> Result<(), PedelecAgentRuntimeError> {
        if self.is_healthy() {
            Ok(())
        } else {
            Err(PedelecAgentRuntimeError::RuntimeDisconnected {
                operation: operation.to_string(),
                message: "Pedelec Agent runtime is not healthy".to_string(),
            })
        }
    }

    fn map_request_error(
        &self,
        operation: &str,
        error: RuntimeControllerError,
    ) -> PedelecAgentRuntimeError {
        match error {
            RuntimeControllerError::Rpc(RpcError::Disconnected { reason }) => {
                self.retire();
                PedelecAgentRuntimeError::RuntimeDisconnected {
                    operation: operation.to_string(),
                    message: format!("{reason:?}"),
                }
            }
            RuntimeControllerError::Rpc(RpcError::RequestTimeout { .. }) => {
                self.retire();
                PedelecAgentRuntimeError::Request {
                    operation: operation.to_string(),
                    message: error.to_string(),
                    details: None,
                }
            }
            RuntimeControllerError::Rpc(RpcError::InvalidMessage { .. }) => {
                self.retire();
                protocol_error(operation, error.to_string())
            }
            RuntimeControllerError::Rpc(RpcError::Remote {
                code,
                message,
                data,
            }) => remote_request_error(operation, code, message, data),
            other => {
                self.retire();
                PedelecAgentRuntimeError::Request {
                    operation: operation.to_string(),
                    message: other.to_string(),
                    details: None,
                }
            }
        }
    }
}

impl ProviderRuntimeController for PedelecAgentServerController {
    fn shutdown(&self) -> Result<(), String> {
        self.healthy.store(false, Ordering::Release);
        let _ = self
            .transport
            .request("shutdown", json!({}), self.graceful_shutdown_timeout);
        let result = self
            .transport
            .shutdown_with_grace(self.graceful_shutdown_timeout)
            .map_err(|error| error.to_string());
        if result.is_err() {
            self.transport.retire();
        }
        self.mappings
            .lock()
            .expect("Pedelec Agent mappings mutex poisoned")
            .clear();
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("Pedelec Agent event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        result.map(|_| ())
    }

    fn is_healthy(&self) -> bool {
        PedelecAgentServerController::is_healthy(self)
    }
}

impl Drop for PedelecAgentServerController {
    fn drop(&mut self) {
        self.healthy.store(false, Ordering::Release);
        self.transport.retire();
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("Pedelec Agent event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn resolve_protocol_owner(mappings: &Mutex<SessionMappings>, frame: &Value) -> Option<String> {
    let params = frame.get("params")?;
    let session_id = non_empty_string(params.get("sessionId"))?;
    mappings
        .lock()
        .ok()?
        .session_to_thread
        .get(&session_id)
        .cloned()
}

fn session_open_params(
    pedelec_thread_id: &str,
    persisted_session_id: Option<&str>,
    config: &PedelecAgentSessionConfig,
) -> Value {
    let mut params = json!({
        "threadId": pedelec_thread_id,
        "sessionId": persisted_session_id,
        "model": config.model,
        "workspacePath": config.workspace.to_string_lossy(),
    });
    if let Some(host_instructions) = &config.host_instructions {
        params["hostInstructions"] = json!(host_instructions);
    }
    params
}

fn normalize_workspace_identity(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn validate_initialize_result(
    result: &Value,
    expected_provider: &str,
) -> Result<(), PedelecAgentRuntimeError> {
    if !result.is_object() {
        return Err(start_error(
            "initialize",
            "initialize result was not an object",
        ));
    }
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| start_error("initialize", "initialize did not return protocolVersion"))?;
    if protocol_version != u64::from(PEDELEC_AGENT_PROTOCOL_VERSION) {
        return Err(start_error(
            "initialize",
            format!("unsupported protocolVersion {protocol_version}"),
        ));
    }
    let name = result
        .get("serverInfo")
        .and_then(Value::as_object)
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| start_error("initialize", "initialize did not return serverInfo.name"))?;
    if name != PEDELEC_AGENT_SERVER_NAME {
        return Err(start_error(
            "initialize",
            format!("unexpected serverInfo.name {name}"),
        ));
    }
    let provider = result
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| start_error("initialize", "initialize did not return provider"))?;
    if provider != expected_provider {
        return Err(start_error(
            "initialize",
            format!("initialize provider {provider} did not match {expected_provider}"),
        ));
    }
    let multiple_sessions = result
        .get("capabilities")
        .and_then(Value::as_object)
        .and_then(|capabilities| capabilities.get("multipleSessions"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !multiple_sessions {
        return Err(start_error(
            "initialize",
            "server did not advertise multipleSessions",
        ));
    }
    Ok(())
}

fn parse_session_open_result(
    result: &Value,
    requested_session_id: Option<&str>,
) -> Result<PedelecAgentSessionResult, PedelecAgentRuntimeError> {
    let agent_session_id = non_empty_string(result.get("sessionId"))
        .ok_or_else(|| protocol_error("session/open", "session/open did not return a sessionId"))?;
    if let Some(requested) = requested_session_id {
        if requested != agent_session_id {
            return Err(protocol_error(
                "session/open",
                format!(
                    "session/open returned {agent_session_id} instead of the requested persisted session"
                ),
            ));
        }
    }
    let resumed = result
        .get("resumed")
        .and_then(Value::as_bool)
        .ok_or_else(|| protocol_error("session/open", "session/open did not return resumed"))?;
    let already_attached = result
        .get("alreadyAttached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let capabilities = result.get("modelCapabilities").ok_or_else(|| {
        protocol_error(
            "session/open",
            "session/open did not return modelCapabilities",
        )
    })?;
    let tools = capabilities
        .get("tools")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            protocol_error(
                "session/open",
                "session/open modelCapabilities.tools is required",
            )
        })?;
    if !tools {
        return Err(protocol_error(
            "session/open",
            "session/open succeeded without tools capability",
        ));
    }
    let vision = capabilities
        .get("vision")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(PedelecAgentSessionResult {
        agent_session_id,
        resumed,
        already_loaded: false,
        already_attached,
        model_capabilities: PedelecAgentModelCapabilities { tools, vision },
    })
}

fn parse_turn_start_result(
    result: &Value,
    local_turn_id: &str,
) -> Result<PedelecAgentTurnStartResult, PedelecAgentRuntimeError> {
    let accepted = result
        .get("accepted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !accepted {
        return Err(protocol_error(
            "turn/start",
            "turn/start did not accept the turn",
        ));
    }
    let turn_id = non_empty_string(result.get("turnId"))
        .ok_or_else(|| protocol_error("turn/start", "turn/start did not return a turnId"))?;
    if turn_id != local_turn_id {
        return Err(protocol_error(
            "turn/start",
            format!("turn/start returned {turn_id} instead of {local_turn_id}"),
        ));
    }
    let already_started = result
        .get("alreadyStarted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(PedelecAgentTurnStartResult {
        turn_id,
        already_started,
    })
}

fn parse_session_close_result(result: &Value) -> bool {
    result.get("closed").and_then(Value::as_bool) == Some(true)
}

fn is_idempotent_close_error(data: &Option<Value>) -> bool {
    matches!(
        data.as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str),
        Some("ATTACHMENT_NOT_FOUND" | "SESSION_CLOSED")
    )
}

fn decode_notification(
    mappings: &Mutex<SessionMappings>,
    method: &str,
    params: &Value,
) -> Vec<PedelecAgentRuntimeEvent> {
    let mut mappings = mappings
        .lock()
        .expect("Pedelec Agent mappings mutex poisoned");
    if FORMAL_TURN_METHODS.contains(&method) {
        return decode_formal_turn(&mut mappings, method, params);
    }
    let agent_session_id = non_empty_string(params.get("sessionId"));
    let turn_id = non_empty_string(params.get("turnId"));
    let claimed_thread_id = non_empty_string(params.get("threadId"));
    if let Some(session_id) = agent_session_id.as_deref() {
        match mapped_thread(&mappings, session_id, claimed_thread_id.as_deref(), method) {
            Ok(pedelec_thread_id) => vec![PedelecAgentRuntimeEvent::Notification {
                method: method.to_string(),
                params: params.clone(),
                pedelec_thread_id: Some(pedelec_thread_id),
                agent_session_id: Some(session_id.to_string()),
                turn_id,
            }],
            Err(failure) => {
                drop_or_protocol_error(&mappings, claimed_thread_id.as_deref(), failure, method)
            }
        }
    } else {
        vec![PedelecAgentRuntimeEvent::Notification {
            method: method.to_string(),
            params: params.clone(),
            pedelec_thread_id: None,
            agent_session_id: None,
            turn_id,
        }]
    }
}

struct ProtocolFailure {
    pedelec_thread_id: Option<String>,
    agent_session_id: Option<String>,
    message: String,
}

impl ProtocolFailure {
    fn into_event(self, operation: &str) -> PedelecAgentRuntimeEvent {
        protocol_event(
            self.pedelec_thread_id.as_deref(),
            self.agent_session_id.as_deref(),
            operation,
            self.message,
        )
    }
}

fn decode_formal_turn(
    mappings: &mut SessionMappings,
    method: &str,
    params: &Value,
) -> Vec<PedelecAgentRuntimeEvent> {
    let Some(agent_session_id) = non_empty_string(params.get("sessionId")) else {
        return vec![protocol_event(
            None,
            None,
            method,
            format!("{method} did not contain sessionId"),
        )];
    };
    let claimed_thread_id = non_empty_string(params.get("threadId"));
    let pedelec_thread_id = match mapped_thread(
        mappings,
        &agent_session_id,
        claimed_thread_id.as_deref(),
        method,
    ) {
        Ok(thread_id) => thread_id,
        Err(failure) => {
            return drop_or_protocol_error(mappings, claimed_thread_id.as_deref(), failure, method);
        }
    };
    let Some(turn_id) = non_empty_string(params.get("turnId")) else {
        return vec![protocol_event(
            Some(&pedelec_thread_id),
            Some(&agent_session_id),
            method,
            format!("{method} did not contain turnId"),
        )];
    };
    let Some(pending) = mappings.pending_turns.get(&pedelec_thread_id) else {
        return Vec::new();
    };
    if pending.session_id != agent_session_id || pending.local_turn_id != turn_id {
        return vec![protocol_event(
            Some(&pedelec_thread_id),
            Some(&agent_session_id),
            method,
            format!("{method} turn correlation did not match the pending turn"),
        )];
    }

    vec![match method {
        "turn/started" => PedelecAgentRuntimeEvent::TurnStarted {
            pedelec_thread_id,
            agent_session_id,
            turn_id,
        },
        "turn/assistant_delta" => PedelecAgentRuntimeEvent::AssistantDelta {
            pedelec_thread_id,
            agent_session_id,
            turn_id,
            text: params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "turn/assistant_message" => PedelecAgentRuntimeEvent::AssistantMessage {
            pedelec_thread_id,
            agent_session_id,
            turn_id,
            text: params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "turn/usage" => PedelecAgentRuntimeEvent::UsageUpdated {
            pedelec_thread_id,
            agent_session_id,
            turn_id,
            usage: params.get("usage").cloned().unwrap_or(Value::Null),
        },
        "turn/completed" => {
            let status = match params.get("status").and_then(Value::as_str) {
                Some("completed") => PedelecAgentTurnStatus::Completed,
                Some("failed") => PedelecAgentTurnStatus::Failed,
                other => {
                    return vec![protocol_event(
                        Some(&pedelec_thread_id),
                        Some(&agent_session_id),
                        method,
                        format!("turn/completed had unsupported status {other:?}"),
                    )];
                }
            };
            mappings.pending_turns.remove(&pedelec_thread_id);
            PedelecAgentRuntimeEvent::TurnCompleted {
                pedelec_thread_id,
                agent_session_id,
                turn_id,
                status,
                error: params.get("error").cloned(),
            }
        }
        other => protocol_event(
            Some(&pedelec_thread_id),
            Some(&agent_session_id),
            other,
            format!("unsupported formal turn method {other}"),
        ),
    }]
}

fn drop_or_protocol_error(
    mappings: &SessionMappings,
    claimed_thread_id: Option<&str>,
    failure: ProtocolFailure,
    method: &str,
) -> Vec<PedelecAgentRuntimeEvent> {
    if claimed_thread_id.is_some_and(|thread_id| mappings.pending_turns.contains_key(thread_id)) {
        vec![failure.into_event(method)]
    } else {
        Vec::new()
    }
}

fn mapped_thread(
    mappings: &SessionMappings,
    agent_session_id: &str,
    claimed_thread_id: Option<&str>,
    method: &str,
) -> Result<String, ProtocolFailure> {
    let Some(pedelec_thread_id) = mappings.session_to_thread.get(agent_session_id).cloned() else {
        return Err(ProtocolFailure {
            pedelec_thread_id: claimed_thread_id.map(str::to_string),
            agent_session_id: Some(agent_session_id.to_string()),
            message: format!("{method} referenced unknown session {agent_session_id}"),
        });
    };
    if let Some(claimed) = claimed_thread_id {
        if claimed != pedelec_thread_id {
            return Err(ProtocolFailure {
                pedelec_thread_id: Some(pedelec_thread_id.clone()),
                agent_session_id: Some(agent_session_id.to_string()),
                message: format!(
                    "{method} threadId {claimed} did not match mapped thread {pedelec_thread_id}"
                ),
            });
        }
    }
    Ok(pedelec_thread_id)
}

fn protocol_event(
    pedelec_thread_id: Option<&str>,
    agent_session_id: Option<&str>,
    operation: &str,
    message: impl Into<String>,
) -> PedelecAgentRuntimeEvent {
    PedelecAgentRuntimeEvent::ProtocolError {
        pedelec_thread_id: pedelec_thread_id.map(str::to_string),
        agent_session_id: agent_session_id.map(str::to_string),
        operation: operation.to_string(),
        message: message.into(),
    }
}

fn run_event_worker(
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<SessionMappings>>,
    healthy: Arc<AtomicBool>,
    events: Sender<PedelecAgentRuntimeEvent>,
) {
    loop {
        match transport.recv_event_timeout(Duration::from_millis(50)) {
            Ok(RuntimeEvent::Rpc(event)) => match event {
                RpcEvent::Notification { method, params } => {
                    let decoded = decode_notification(&mappings, &method, &params);
                    let protocol_error = decoded.iter().find_map(|event| match event {
                        PedelecAgentRuntimeEvent::ProtocolError { message, .. } => {
                            Some(message.clone())
                        }
                        _ => None,
                    });
                    for event in decoded {
                        if events.send(event).is_err() {
                            return;
                        }
                    }
                    if let Some(message) = protocol_error {
                        healthy.store(false, Ordering::Release);
                        transport.retire();
                        send_disconnect(
                            &transport,
                            &mappings,
                            &events,
                            RpcDisconnectReason::MalformedFrame(message),
                        );
                        break;
                    }
                }
                RpcEvent::ServerRequest(request) => {
                    let method = request.method.clone();
                    let _ = transport.respond_error(
                        request.id,
                        json!(-32601),
                        format!("Pedelec Agent client does not support {method}"),
                        Some(json!({ "method": method })),
                    );
                }
                RpcEvent::Stderr { text } => {
                    if events
                        .send(PedelecAgentRuntimeEvent::Stderr { text })
                        .is_err()
                    {
                        return;
                    }
                }
                RpcEvent::Disconnected { reason } => {
                    healthy.store(false, Ordering::Release);
                    if !matches!(reason, RpcDisconnectReason::Explicit) {
                        transport.retire();
                        send_disconnect(&transport, &mappings, &events, reason);
                    }
                    break;
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
    events: &Sender<PedelecAgentRuntimeEvent>,
    reason: RpcDisconnectReason,
) {
    let attachments = mappings
        .lock()
        .expect("Pedelec Agent mappings mutex poisoned")
        .clear();
    let _ = events.send(PedelecAgentRuntimeEvent::Disconnected {
        generation: transport.generation(),
        pid: transport.process_id(),
        attachments,
        reason,
    });
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn start_error(operation: &str, error: impl fmt::Display) -> PedelecAgentRuntimeError {
    PedelecAgentRuntimeError::RuntimeStart {
        operation: operation.to_string(),
        message: error.to_string(),
    }
}

fn protocol_error(operation: &str, message: impl Into<String>) -> PedelecAgentRuntimeError {
    PedelecAgentRuntimeError::Protocol {
        operation: operation.to_string(),
        message: message.into(),
    }
}

fn request_error(
    operation: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> PedelecAgentRuntimeError {
    PedelecAgentRuntimeError::Request {
        operation: operation.to_string(),
        message: message.into(),
        details,
    }
}

fn remote_request_error(
    operation: &str,
    code: Option<Value>,
    message: String,
    data: Option<Value>,
) -> PedelecAgentRuntimeError {
    PedelecAgentRuntimeError::Request {
        operation: operation.to_string(),
        message: format!("remote RPC error: {message}"),
        details: Some(json!({
            "code": code,
            "message": message,
            "data": data,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderRuntimeController;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    struct FakeAgent {
        directory: tempfile::TempDir,
        log: PathBuf,
        init_mode: String,
        open_mode: String,
        turn_mode: String,
        close_mode: String,
        provider: String,
    }

    impl FakeAgent {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            Self {
                log: directory.path().join("requests.jsonl"),
                directory,
                init_mode: "ok".into(),
                open_mode: "ok".into(),
                turn_mode: "ok".into(),
                close_mode: "ok".into(),
                provider: "ollama".into(),
            }
        }

        fn init_mode(mut self, mode: &str) -> Self {
            self.init_mode = mode.to_string();
            self
        }

        fn open_mode(mut self, mode: &str) -> Self {
            self.open_mode = mode.to_string();
            self
        }

        fn turn_mode(mut self, mode: &str) -> Self {
            self.turn_mode = mode.to_string();
            self
        }

        fn close_mode(mut self, mode: &str) -> Self {
            self.close_mode = mode.to_string();
            self
        }

        fn launch(&self) -> PedelecAgentRuntimeLaunchConfig {
            let mut config = fake_launch_config(self.directory.path(), &self.provider)
                .with_env(
                    "FAKE_PEDELEC_AGENT_LOG",
                    self.log.to_string_lossy().into_owned(),
                )
                .with_env("FAKE_PEDELEC_AGENT_INIT_MODE", self.init_mode.clone())
                .with_env("FAKE_PEDELEC_AGENT_OPEN_MODE", self.open_mode.clone())
                .with_env("FAKE_PEDELEC_AGENT_TURN_MODE", self.turn_mode.clone())
                .with_env("FAKE_PEDELEC_AGENT_CLOSE_MODE", self.close_mode.clone())
                .with_env("FAKE_PEDELEC_AGENT_PROVIDER", self.provider.clone());
            if self.turn_mode == "timeout" || self.close_mode == "timeout" {
                config.control_timeout = Duration::from_secs(2);
            }
            config
        }

        fn methods(&self) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|request| request["method"].as_str().map(str::to_string))
                })
                .collect()
        }
    }

    fn fake_launch_config(
        cwd: impl Into<PathBuf>,
        provider: &str,
    ) -> PedelecAgentRuntimeLaunchConfig {
        let cwd = cwd.into();
        #[cfg(windows)]
        {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake_pedelec_agent.ps1");
            PedelecAgentRuntimeLaunchConfig::new("powershell.exe", cwd, provider).args([
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script.into_os_string(),
            ])
        }
        #[cfg(not(windows))]
        {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake_pedelec_agent.sh");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o755));
            }
            PedelecAgentRuntimeLaunchConfig::new(&script, cwd, provider)
        }
    }

    fn session_config(workspace: &Path) -> PedelecAgentSessionConfig {
        PedelecAgentSessionConfig {
            model: "qwen3:8b".into(),
            workspace: workspace.to_path_buf(),
            host_instructions: Some("stay local".into()),
        }
    }

    fn session_meta() -> SessionMeta {
        SessionMeta {
            model_capabilities: PedelecAgentModelCapabilities {
                tools: true,
                vision: false,
            },
            resumed: false,
            model: "qwen3:8b".into(),
            workspace: PathBuf::from("/tmp"),
            host_instructions: Some("stay local".into()),
        }
    }

    fn drain_traffic(controller: &PedelecAgentServerController) -> Vec<ProtocolTrafficRecord> {
        let mut records = Vec::new();
        while let Ok(record) = controller.recv_protocol_traffic_timeout(Duration::from_millis(80)) {
            records.push(record);
        }
        records
    }

    fn recv_matching<F>(
        controller: &PedelecAgentServerController,
        mut predicate: F,
    ) -> PedelecAgentRuntimeEvent
    where
        F: FnMut(&PedelecAgentRuntimeEvent) -> bool,
    {
        for _ in 0..40 {
            match controller.recv_event_timeout(Duration::from_secs(1)) {
                Ok(PedelecAgentRuntimeEvent::Stderr { .. }) => {}
                Ok(event) if predicate(&event) => return event,
                Ok(_) => {}
                Err(error) => panic!("timed out waiting for runtime event: {error}"),
            }
        }
        panic!("too many events without a match");
    }

    #[test]
    fn process_spec_uses_generic_provider_and_jsonrpc_defaults() {
        let config = PedelecAgentRuntimeLaunchConfig::new(
            "pedelec-agent",
            tempdir().unwrap().path(),
            "lmstudio",
        )
        .with_env("PEDELEC_CORE_IPC_RUNTIME_FILE", "runtime.json");
        let spec = config.process_spec();
        assert_eq!(
            spec.args,
            vec![
                OsString::from("serve"),
                OsString::from("--provider"),
                OsString::from("lmstudio"),
            ]
        );
        assert!(spec.env_remove.iter().any(|key| key == "PEDELEC_THREAD_ID"));
        assert!(!spec.args.iter().any(|arg| arg == "ollama"));
    }

    #[test]
    fn initialize_validation_rejects_provider_and_capability_mismatch() {
        assert!(validate_initialize_result(
            &json!({
                "protocolVersion": 1,
                "serverInfo": { "name": "pedelec-agent", "version": "1" },
                "provider": "ollama",
                "capabilities": { "multipleSessions": true }
            }),
            "ollama"
        )
        .is_ok());
        assert!(validate_initialize_result(
            &json!({
                "protocolVersion": 1,
                "serverInfo": { "name": "pedelec-agent" },
                "provider": "other",
                "capabilities": { "multipleSessions": true }
            }),
            "ollama"
        )
        .is_err());
        assert!(validate_initialize_result(
            &json!({
                "protocolVersion": 99,
                "serverInfo": { "name": "pedelec-agent" },
                "provider": "ollama",
                "capabilities": { "multipleSessions": true }
            }),
            "ollama"
        )
        .is_err());
        assert!(validate_initialize_result(
            &json!({
                "protocolVersion": 1,
                "serverInfo": { "name": "pedelec-agent" },
                "provider": "ollama",
                "capabilities": { "multipleSessions": false }
            }),
            "ollama"
        )
        .is_err());
        assert!(validate_initialize_result(&json!("not-an-object"), "ollama").is_err());
    }

    #[test]
    fn mappings_are_bidirectional_and_reject_collisions() {
        let mut mappings = SessionMappings::default();
        mappings
            .register("thread-a", "session-a", session_meta())
            .unwrap();
        assert_eq!(
            mappings
                .session_to_thread
                .get("session-a")
                .map(String::as_str),
            Some("thread-a")
        );
        assert!(mappings
            .register("thread-b", "session-a", session_meta(),)
            .is_err());
        assert!(mappings
            .register("thread-a", "session-b", session_meta(),)
            .is_err());
        mappings
            .register("thread-a", "session-a", session_meta())
            .unwrap();
        mappings.remove_thread("thread-a");
        assert!(mappings.session_to_thread.is_empty());
    }

    #[test]
    fn resume_and_turn_response_parsers_enforce_identity() {
        let ok = parse_session_open_result(
            &json!({
                "sessionId": "session-a",
                "resumed": true,
                "alreadyAttached": false,
                "modelCapabilities": { "tools": true, "vision": true }
            }),
            Some("session-a"),
        )
        .unwrap();
        assert_eq!(ok.agent_session_id, "session-a");
        assert!(ok.model_capabilities.tools);
        assert!(parse_session_open_result(
            &json!({
                "sessionId": "session-b",
                "resumed": true,
                "modelCapabilities": { "tools": true, "vision": false }
            }),
            Some("session-a"),
        )
        .is_err());
        assert!(parse_session_open_result(
            &json!({
                "sessionId": "session-a",
                "resumed": true,
                "modelCapabilities": { "tools": false, "vision": false }
            }),
            Some("session-a"),
        )
        .is_err());
        let turn = parse_turn_start_result(
            &json!({ "turnId": "turn-1", "accepted": true, "alreadyStarted": true }),
            "turn-1",
        )
        .unwrap();
        assert!(turn.already_started);
        assert!(parse_turn_start_result(
            &json!({ "turnId": "turn-2", "accepted": true }),
            "turn-1",
        )
        .is_err());
        assert!(parse_session_close_result(&json!({ "closed": true })));
        assert!(!parse_session_close_result(&json!("not-closed")));
    }

    #[test]
    fn decode_maps_formal_turn_events_and_keeps_attachment_after_completed() {
        let mappings = Mutex::new(SessionMappings::default());
        {
            let mut mappings = mappings.lock().unwrap();
            mappings
                .register("thread-a", "session-a", session_meta())
                .unwrap();
            mappings
                .register_pending_turn("thread-a", "session-a", "turn-1")
                .unwrap();
        }
        let identity = json!({
            "threadId": "thread-a",
            "sessionId": "session-a",
            "turnId": "turn-1"
        });
        assert!(matches!(
            decode_notification(&mappings, "turn/started", &identity).as_slice(),
            [PedelecAgentRuntimeEvent::TurnStarted { turn_id, .. }] if turn_id == "turn-1"
        ));
        let mut delta = identity.clone();
        delta["text"] = json!("hello");
        assert!(matches!(
            decode_notification(&mappings, "turn/assistant_delta", &delta).as_slice(),
            [PedelecAgentRuntimeEvent::AssistantDelta { text, .. }] if text == "hello"
        ));
        let mut message = identity.clone();
        message["text"] = json!("hello world");
        assert!(matches!(
            decode_notification(&mappings, "turn/assistant_message", &message).as_slice(),
            [PedelecAgentRuntimeEvent::AssistantMessage { text, .. }] if text == "hello world"
        ));
        let mut usage = identity.clone();
        usage["usage"] = json!({ "totalTokens": 3 });
        assert!(matches!(
            decode_notification(&mappings, "turn/usage", &usage).as_slice(),
            [PedelecAgentRuntimeEvent::UsageUpdated { usage, .. }] if usage["totalTokens"] == 3
        ));
        let mut completed = identity.clone();
        completed["status"] = json!("completed");
        assert!(matches!(
            decode_notification(&mappings, "turn/completed", &completed).as_slice(),
            [PedelecAgentRuntimeEvent::TurnCompleted {
                status: PedelecAgentTurnStatus::Completed,
                ..
            }]
        ));
        let mappings = mappings.lock().unwrap();
        assert!(!mappings.pending_turns.contains_key("thread-a"));
        assert_eq!(
            mappings
                .thread_to_session
                .get("thread-a")
                .map(String::as_str),
            Some("session-a")
        );
    }

    #[test]
    fn decode_retires_unknown_session_wrong_turn_and_malformed_formal_events() {
        let mappings = Mutex::new(SessionMappings::default());
        {
            let mut mappings = mappings.lock().unwrap();
            mappings
                .register("thread-a", "session-a", session_meta())
                .unwrap();
            mappings
                .register_pending_turn("thread-a", "session-a", "turn-1")
                .unwrap();
        }
        assert!(matches!(
            decode_notification(
                &mappings,
                "turn/started",
                &json!({ "threadId": "thread-a", "sessionId": "unknown", "turnId": "turn-1" })
            )
            .as_slice(),
            [PedelecAgentRuntimeEvent::ProtocolError { .. }]
        ));
        assert!(matches!(
            decode_notification(
                &mappings,
                "turn/started",
                &json!({ "threadId": "thread-a", "sessionId": "session-a", "turnId": "wrong" })
            )
            .as_slice(),
            [PedelecAgentRuntimeEvent::ProtocolError { .. }]
        ));
        assert!(matches!(
            decode_notification(
                &mappings,
                "turn/started",
                &json!({ "threadId": "thread-a", "turnId": "turn-1" })
            )
            .as_slice(),
            [PedelecAgentRuntimeEvent::ProtocolError { .. }]
        ));
        let tool = decode_notification(
            &mappings,
            "turn/tool_call",
            &json!({
                "threadId": "thread-a",
                "sessionId": "session-a",
                "turnId": "turn-1",
                "name": "read_file"
            }),
        );
        assert!(matches!(
            tool.as_slice(),
            [PedelecAgentRuntimeEvent::Notification { method, .. }] if method == "turn/tool_call"
        ));
    }

    #[test]
    fn unmapped_stale_turn_events_are_dropped_without_protocol_error() {
        let mappings = Mutex::new(SessionMappings::default());
        assert!(decode_notification(
            &mappings,
            "turn/assistant_delta",
            &json!({
                "threadId": "thread-a",
                "sessionId": "session-a",
                "turnId": "turn-1",
                "text": "stale"
            })
        )
        .is_empty());
        assert!(decode_notification(
            &mappings,
            "session",
            &json!({
                "type": "session",
                "sessionId": "0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176",
                "resumed": false
            })
        )
        .iter()
        .all(|event| !matches!(
            event,
            PedelecAgentRuntimeEvent::TurnStarted { .. }
                | PedelecAgentRuntimeEvent::AssistantMessage { .. }
                | PedelecAgentRuntimeEvent::ProtocolError { .. }
        )));
    }

    #[test]
    fn spawn_uses_jsonrpc2_and_validates_initialize() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        assert!(controller.is_healthy());
        let traffic = drain_traffic(&controller);
        let initialize = traffic
            .iter()
            .find(|record| {
                record.direction == "client_to_provider"
                    && record.kind == "request"
                    && record.message["method"] == "initialize"
            })
            .unwrap();
        assert_eq!(initialize.message["jsonrpc"], "2.0");
        assert_eq!(initialize.thread_id, None);
        assert_eq!(
            initialize.message["params"]["protocolVersion"],
            PEDELEC_AGENT_PROTOCOL_VERSION
        );
        assert_eq!(fake.methods(), vec!["initialize".to_string()]);
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn initialize_mismatch_retires_generation() {
        for mode in [
            "malformed",
            "bad-version",
            "wrong-name",
            "no-multiple-sessions",
        ] {
            let fake = FakeAgent::new().init_mode(mode);
            let error = PedelecAgentServerController::spawn(fake.launch()).unwrap_err();
            assert!(
                matches!(error, PedelecAgentRuntimeError::RuntimeStart { ref operation, .. } if operation == "initialize"),
                "mode {mode} produced {error:?}"
            );
        }
        let fake = FakeAgent::new();
        let mut config = fake.launch();
        config.provider = "other".into();
        let error = PedelecAgentServerController::spawn(config).unwrap_err();
        assert!(matches!(
            error,
            PedelecAgentRuntimeError::RuntimeStart { operation, .. } if operation == "initialize"
        ));
    }

    #[test]
    fn ensure_session_registers_mapping_and_already_loaded_skips_open() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let workspace = fake.directory.path();
        let first = controller
            .ensure_session("thread-a", None, &session_config(workspace))
            .unwrap();
        assert!(!first.already_loaded);
        assert_eq!(first.agent_session_id, "agent-session-1");
        assert_eq!(
            controller.loaded_session_id("thread-a").as_deref(),
            Some("agent-session-1")
        );
        let traffic = drain_traffic(&controller);
        assert!(traffic.iter().any(|record| {
            record.kind == "request"
                && record.message["method"] == "session/open"
                && record.thread_id.as_deref() == Some("thread-a")
                && record.message["jsonrpc"] == "2.0"
        }));
        let second = controller
            .ensure_session("thread-a", None, &session_config(workspace))
            .unwrap();
        assert!(second.already_loaded);
        assert_eq!(second.agent_session_id, "agent-session-1");
        let methods = fake.methods();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "session/open")
                .count(),
            1
        );
        let same = controller
            .ensure_session(
                "thread-a",
                Some("agent-session-1"),
                &session_config(workspace),
            )
            .unwrap();
        assert!(same.already_loaded);
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn already_loaded_rejects_changed_model_and_workspace() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let workspace = fake.directory.path();
        controller
            .ensure_session("thread-a", None, &session_config(workspace))
            .unwrap();
        let mut other_model = session_config(workspace);
        other_model.model = "other-model".into();
        let error = controller
            .ensure_session("thread-a", None, &other_model)
            .unwrap_err();
        assert!(matches!(
            error,
            PedelecAgentRuntimeError::Protocol { operation, .. } if operation == "session/open"
        ));
        assert!(!controller.is_healthy());

        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let other_workspace = tempdir().unwrap();
        let error = controller
            .ensure_session("thread-a", None, &session_config(other_workspace.path()))
            .unwrap_err();
        assert!(matches!(
            error,
            PedelecAgentRuntimeError::Protocol { operation, .. } if operation == "session/open"
        ));
        assert!(!controller.is_healthy());
        ProviderRuntimeController::shutdown(controller.as_ref()).ok();
    }

    #[test]
    fn already_loaded_resends_open_when_host_instructions_change() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let workspace = fake.directory.path();
        controller
            .ensure_session("thread-a", None, &session_config(workspace))
            .unwrap();
        let mut changed = session_config(workspace);
        changed.host_instructions = Some("updated host instructions".into());
        let refreshed = controller
            .ensure_session("thread-a", Some("agent-session-1"), &changed)
            .unwrap();
        assert!(!refreshed.already_loaded);
        assert_eq!(
            fake.methods()
                .iter()
                .filter(|method| method.as_str() == "session/open")
                .count(),
            2
        );
        let unchanged = controller
            .ensure_session("thread-a", Some("agent-session-1"), &changed)
            .unwrap();
        assert!(unchanged.already_loaded);
        assert_eq!(
            fake.methods()
                .iter()
                .filter(|method| method.as_str() == "session/open")
                .count(),
            2
        );
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn resume_identity_mismatch_and_mapping_conflicts_retire() {
        let fake = FakeAgent::new().open_mode("resume-mismatch");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let error = controller
            .ensure_session(
                "thread-a",
                Some("persisted-session"),
                &session_config(fake.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            PedelecAgentRuntimeError::Protocol { operation, .. } if operation == "session/open"
        ));
        assert!(!controller.is_healthy());

        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let conflict = controller
            .ensure_session(
                "thread-a",
                Some("other-session"),
                &session_config(fake.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(
            conflict,
            PedelecAgentRuntimeError::Protocol { .. }
        ));
        assert!(!controller.is_healthy());

        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let stolen = controller
            .ensure_session(
                "thread-b",
                Some(&opened.agent_session_id),
                &session_config(fake.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(stolen, PedelecAgentRuntimeError::Protocol { .. }));
        assert!(!controller.is_healthy());
    }

    #[test]
    fn start_turn_correlates_events_before_response_and_keeps_attachment() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let started = controller
            .start_turn("thread-a", &opened.agent_session_id, "turn-1", "hello")
            .unwrap();
        assert_eq!(started.turn_id, "turn-1");
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::TurnStarted { turn_id, .. } if turn_id == "turn-1"
            )
        });
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::AssistantDelta { text, .. } if text == "hello"
            )
        });
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::AssistantMessage { text, .. } if text == "hello world"
            )
        });
        recv_matching(&controller, |event| {
            matches!(event, PedelecAgentRuntimeEvent::UsageUpdated { .. })
        });
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::Notification { method, .. } if method == "turn/tool_call"
            )
        });
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::TurnCompleted {
                    status: PedelecAgentTurnStatus::Completed,
                    ..
                }
            )
        });
        assert!(controller.mappings.lock().unwrap().pending_turns.is_empty());
        assert_eq!(
            controller.loaded_session_id("thread-a").as_deref(),
            Some(opened.agent_session_id.as_str())
        );
        assert!(controller.is_healthy());
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn remote_admission_error_clears_pending_without_retiring() {
        let fake = FakeAgent::new().turn_mode("busy");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let error = controller
            .start_turn("thread-a", &opened.agent_session_id, "turn-1", "hello")
            .unwrap_err();
        match error {
            PedelecAgentRuntimeError::Request {
                operation, details, ..
            } => {
                assert_eq!(operation, "turn/start");
                assert_eq!(details.unwrap()["data"]["code"], "SESSION_BUSY");
            }
            other => panic!("expected request error, got {other:?}"),
        }
        assert!(controller.is_healthy());
        assert!(controller.mappings.lock().unwrap().pending_turns.is_empty());
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn turn_timeout_retires_without_evidence_heuristic() {
        let fake = FakeAgent::new().turn_mode("timeout");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let error = controller
            .start_turn("thread-a", &opened.agent_session_id, "turn-1", "hello")
            .unwrap_err();
        assert!(matches!(
            error,
            PedelecAgentRuntimeError::Request { operation, .. }
                | PedelecAgentRuntimeError::RuntimeDisconnected { operation, .. }
                if operation == "turn/start"
        ));
        assert!(!controller.is_healthy());
        assert!(!fake
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt"));
    }

    #[test]
    fn mismatched_turn_notifications_retire_generation() {
        for mode in ["wrong-turn", "unknown-session", "malformed"] {
            let fake = FakeAgent::new().turn_mode(mode);
            let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
            let opened = controller
                .ensure_session("thread-a", None, &session_config(fake.directory.path()))
                .unwrap();
            let _ = controller.start_turn("thread-a", &opened.agent_session_id, "turn-1", "hello");
            recv_matching(&controller, |event| {
                matches!(event, PedelecAgentRuntimeEvent::ProtocolError { .. })
            });
            assert!(!controller.is_healthy());
        }
    }

    #[test]
    fn pending_close_drops_stale_worker_events() {
        let fake = FakeAgent::new().turn_mode("pending");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        controller
            .start_turn("thread-a", &opened.agent_session_id, "turn-1", "hello")
            .unwrap();
        recv_matching(&controller, |event| {
            matches!(
                event,
                PedelecAgentRuntimeEvent::TurnStarted { turn_id, .. } if turn_id == "turn-1"
            )
        });
        assert_eq!(
            controller
                .close_session("thread-a", Some(&opened.agent_session_id), Some("turn-1"))
                .unwrap(),
            PedelecAgentCloseOutcome::Closed
        );
        std::thread::sleep(Duration::from_millis(200));
        let mut stale = Vec::new();
        while let Ok(event) = controller.recv_event_timeout(Duration::from_millis(50)) {
            stale.push(event);
        }
        assert!(
            stale.iter().all(|event| {
                !matches!(
                    event,
                    PedelecAgentRuntimeEvent::ProtocolError { .. }
                        | PedelecAgentRuntimeEvent::AssistantDelta { .. }
                        | PedelecAgentRuntimeEvent::TurnCompleted { .. }
                )
            }),
            "stale events after close: {stale:?}"
        );
        assert!(controller.is_healthy());
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn close_removes_mapping_and_missing_mapping_is_idempotent() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        assert_eq!(
            controller
                .close_session("thread-a", Some(&opened.agent_session_id), Some("turn-1"))
                .unwrap(),
            PedelecAgentCloseOutcome::Closed
        );
        assert!(controller.loaded_session_id("thread-a").is_none());
        assert_eq!(
            controller.close_session("thread-a", None, None).unwrap(),
            PedelecAgentCloseOutcome::AlreadyDetached
        );
        assert!(!fake
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt"));
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn close_reclaims_thread_and_session_lock_entries() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let opened = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        assert_eq!(
            controller
                .close_session("thread-a", Some(&opened.agent_session_id), None)
                .unwrap(),
            PedelecAgentCloseOutcome::Closed
        );
        assert_eq!(controller.mappings.lock().unwrap().lock_counts(), (0, 0));
        ProviderRuntimeController::shutdown(controller.as_ref()).unwrap();
    }

    #[test]
    fn ambiguous_close_retires_generation() {
        let fake = FakeAgent::new().close_mode("timeout");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let outcome = controller.close_session("thread-a", None, None).unwrap();
        assert!(matches!(
            outcome,
            PedelecAgentCloseOutcome::GenerationRetired { .. }
        ));
        assert!(!controller.is_healthy());

        let fake = FakeAgent::new().close_mode("malformed");
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let outcome = controller.close_session("thread-a", None, None).unwrap();
        assert!(matches!(
            outcome,
            PedelecAgentCloseOutcome::GenerationRetired { .. }
        ));
        assert!(!controller.is_healthy());
    }

    #[test]
    fn disconnect_includes_attachments_and_owner_resolver_routes_traffic() {
        let fake = FakeAgent::new();
        let controller = PedelecAgentServerController::spawn(fake.launch()).unwrap();
        let first = controller
            .ensure_session("thread-a", None, &session_config(fake.directory.path()))
            .unwrap();
        let second = controller
            .ensure_session("thread-b", None, &session_config(fake.directory.path()))
            .unwrap();
        let traffic = drain_traffic(&controller);
        assert!(traffic.iter().any(|record| {
            record.message["method"] == "session/open"
                && record.thread_id.as_deref() == Some("thread-a")
                && record.direction == "client_to_provider"
        }));
        assert!(traffic.iter().any(|record| {
            record.kind == "response"
                && record.thread_id.as_deref() == Some("thread-a")
                && record.message.get("result").is_some()
        }));
        let _ = controller.start_turn("thread-a", &first.agent_session_id, "turn-1", "hello");
        let routed = drain_traffic(&controller);
        assert!(routed.iter().any(|record| {
            record.direction == "provider_to_client"
                && record.kind == "notification"
                && record.thread_id.as_deref() == Some("thread-a")
                && record.message["method"] == "turn/started"
        }));
        controller.retire();
        let disconnected = recv_matching(&controller, |event| {
            matches!(event, PedelecAgentRuntimeEvent::Disconnected { .. })
        });
        match disconnected {
            PedelecAgentRuntimeEvent::Disconnected { attachments, .. } => {
                assert!(attachments.iter().any(|attachment| {
                    attachment.pedelec_thread_id == "thread-a"
                        && attachment.agent_session_id == first.agent_session_id
                }));
                assert!(attachments.iter().any(|attachment| {
                    attachment.pedelec_thread_id == "thread-b"
                        && attachment.agent_session_id == second.agent_session_id
                }));
            }
            other => panic!("expected disconnect, got {other:?}"),
        }
    }
}
