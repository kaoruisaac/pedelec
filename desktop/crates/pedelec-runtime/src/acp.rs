//! Provider-neutral Agent Client Protocol v1 controller.
//!
//! Provider adapters own executable discovery and provider-specific policy.
//! This module owns ACP transport, session/turn correlation, permission
//! responses, and normalization of the subset of updates Pedelec consumes.

use crate::{
    PersistentProcessSpec, PersistentRuntimeController, ProviderRuntimeController,
    RpcDisconnectReason, RpcEnvelopeMode, RpcError, RpcEvent, RpcServerRequest,
    RuntimeControllerError, RuntimeEvent,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const ACP_PROTOCOL_VERSION: u16 = 1;
const ACP_CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub struct AcpLaunchConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub process_cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub max_frame_bytes: usize,
    pub control_timeout: Duration,
    pub prompt_timeout: Duration,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    pub provider_label: String,
    pub authentication: AcpAuthentication,
}

impl AcpLaunchConfig {
    pub fn new(
        provider_label: impl Into<String>,
        program: impl Into<PathBuf>,
        process_cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            process_cwd: process_cwd.into(),
            env: Vec::new(),
            max_frame_bytes: crate::DEFAULT_MAX_FRAME_BYTES,
            control_timeout: crate::DEFAULT_CONTROL_TIMEOUT,
            prompt_timeout: Duration::from_secs(24 * 60 * 60),
            client_name: "pedelec".to_string(),
            client_title: "Pedelec".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            provider_label: provider_label.into(),
            authentication: AcpAuthentication::None,
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

    pub fn with_authentication(mut self, authentication: AcpAuthentication) -> Self {
        self.authentication = authentication;
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
                let mut spec = PersistentProcessSpec::new("cmd.exe")
                    .args(["/d", "/c", "call"])
                    .arg(self.program.as_os_str())
                    .args(self.args.clone())
                    .cwd(self.process_cwd.clone())
                    .env_remove("PEDELEC_THREAD_ID")
                    .env_remove("PEDELEC_WORKSPACE_PATH");
                for (key, value) in &self.env {
                    if !is_thread_scoped_env(key) {
                        spec = spec.env(key.clone(), value.clone());
                    }
                }
                return spec;
            }
        }

        let mut spec = PersistentProcessSpec::new(self.program.clone())
            .args(self.args.clone())
            .cwd(self.process_cwd.clone())
            .env_remove("PEDELEC_THREAD_ID")
            .env_remove("PEDELEC_WORKSPACE_PATH");
        for (key, value) in &self.env {
            if !is_thread_scoped_env(key) {
                spec = spec.env(key.clone(), value.clone());
            }
        }
        spec
    }
}

fn is_thread_scoped_env(key: &OsString) -> bool {
    matches!(
        key.to_string_lossy().as_ref(),
        "PEDELEC_THREAD_ID" | "PEDELEC_WORKSPACE_PATH"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AcpAuthentication {
    #[default]
    None,
    MethodId(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcpSessionConfig {
    pub cwd: PathBuf,
    /// Typed ACP `session/set_config_option` payloads, applied after attach.
    pub config_options: Vec<AcpConfigOptionUpdate>,
}

impl AcpSessionConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            config_options: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AcpRuntimeError> {
        if self.cwd.as_os_str().is_empty() || !self.cwd.is_absolute() {
            return Err(AcpRuntimeError::Protocol {
                operation: "session/config".to_string(),
                message: "ACP session cwd must be an absolute path".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcpConfigOptionUpdate {
    pub config_id: String,
    /// The value advertised by the matching ACP session config option.
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSessionAttachment {
    pub pedelec_thread_id: String,
    pub provider_session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpPermissionDecision {
    AllowOnce,
    RejectOnce,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcpPermissionRequest {
    pub pedelec_thread_id: String,
    pub provider_session_id: String,
    pub tool_call: Value,
    pub options: Value,
}

pub trait AcpPermissionResolver: Send + Sync {
    fn resolve(&self, request: &AcpPermissionRequest) -> AcpPermissionDecision;
}

/// Provider-specific ACP server requests are handled by the provider adapter.
/// Keeping this hook below the controller prevents provider method names from
/// leaking into the shared ACP lifecycle implementation.
pub trait AcpExtensionRequestHandler: Send + Sync {
    fn handle(&self, request: &RpcServerRequest) -> Option<Value>;
}

impl<F> AcpExtensionRequestHandler for F
where
    F: Fn(&RpcServerRequest) -> Option<Value> + Send + Sync,
{
    fn handle(&self, request: &RpcServerRequest) -> Option<Value> {
        self(request)
    }
}

impl<F> AcpPermissionResolver for F
where
    F: Fn(&AcpPermissionRequest) -> AcpPermissionDecision + Send + Sync,
{
    fn resolve(&self, request: &AcpPermissionRequest) -> AcpPermissionDecision {
        self(request)
    }
}

/// Conservative default policy snapshot. It approves only when every path
/// exposed by the tool call is inside the workspace and at least one path is
/// present. Provider adapters may supply a richer resolver for command-aware
/// sandbox policy, but cannot accidentally fall back to allow-all.
#[derive(Debug, Clone)]
pub struct AcpWorkspacePermissionPolicy {
    workspace: PathBuf,
}

impl AcpWorkspacePermissionPolicy {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            workspace: canonicalize_for_policy(&workspace),
        }
    }
}

impl AcpPermissionResolver for AcpWorkspacePermissionPolicy {
    fn resolve(&self, request: &AcpPermissionRequest) -> AcpPermissionDecision {
        let mut paths = Vec::new();
        collect_tool_paths(&request.tool_call, &mut paths);
        if !paths.is_empty()
            && paths.iter().all(|path| {
                let path = if path.is_absolute() {
                    normalize_path(path)
                } else {
                    normalize_path(&self.workspace.join(path))
                };
                path_is_within(&canonicalize_for_policy(&path), &self.workspace)
            })
        {
            AcpPermissionDecision::AllowOnce
        } else {
            AcpPermissionDecision::RejectOnce
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpTurnStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcpRuntimeEvent {
    SessionReady {
        pedelec_thread_id: String,
        provider_session_id: String,
        config_options: Option<Value>,
    },
    AssistantDelta {
        pedelec_thread_id: String,
        provider_session_id: String,
        local_turn_id: String,
        text: String,
    },
    AssistantMessage {
        pedelec_thread_id: String,
        provider_session_id: String,
        local_turn_id: String,
        text: String,
    },
    UsageUpdated {
        pedelec_thread_id: String,
        provider_session_id: String,
        local_turn_id: Option<String>,
        usage: Value,
    },
    TurnCompleted {
        pedelec_thread_id: String,
        provider_session_id: String,
        local_turn_id: String,
        status: AcpTurnStatus,
        stop_reason: String,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Value,
        pedelec_thread_id: Option<String>,
        provider_session_id: Option<String>,
        local_turn_id: Option<String>,
    },
    PermissionResolved {
        pedelec_thread_id: String,
        provider_session_id: String,
        option_id: String,
        decision: AcpPermissionDecision,
    },
    Stderr {
        text: String,
    },
    ProtocolError {
        pedelec_thread_id: Option<String>,
        provider_session_id: Option<String>,
        operation: String,
        message: String,
    },
    Disconnected {
        generation: u64,
        pid: u32,
        attachments: Vec<AcpSessionAttachment>,
        reason: RpcDisconnectReason,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcpRuntimeError {
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

impl fmt::Display for AcpRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (operation, message) = match self {
            Self::RuntimeStart { operation, message }
            | Self::RuntimeDisconnected { operation, message }
            | Self::Protocol { operation, message }
            | Self::Request {
                operation, message, ..
            } => (operation, message),
        };
        write!(f, "ACP {operation} failed: {message}")
    }
}

impl std::error::Error for AcpRuntimeError {}

#[derive(Debug, Default)]
struct AcpMappings {
    provider_to_pedelec: HashMap<String, String>,
    pedelec_to_provider: HashMap<String, String>,
    session_config_options: HashMap<String, Value>,
    session_modes: HashMap<String, Value>,
    session_origins: HashMap<String, AcpSessionOrigin>,
    pending_turns: HashMap<String, AcpPendingTurn>,
    turn_changed: Arc<Condvar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpSessionOrigin {
    New,
    Loaded,
}

#[derive(Debug)]
struct AcpPendingTurn {
    local_turn_id: String,
    messages: Vec<(String, String)>,
    update_revision: u64,
    response_received: bool,
    complete_messages: HashSet<String>,
}

impl AcpPendingTurn {
    fn append(&mut self, message_id: Option<&str>, text: &str) {
        let key = message_id.unwrap_or("__unidentified__");
        if self.complete_messages.contains(key) {
            return;
        }
        if let Some((_, message)) = self.messages.iter_mut().find(|(id, _)| id == key) {
            message.push_str(text);
        } else {
            self.messages.push((key.to_string(), text.to_string()));
        }
    }

    /// Records a provider update that carries a complete assistant message.
    /// Providers are allowed to send chunks followed by a complete message;
    /// in that case the complete value replaces the aggregate and only an
    /// unseen suffix is exposed as a delta.
    fn set_complete(&mut self, message_id: Option<&str>, text: &str) -> Option<String> {
        let key = message_id.unwrap_or("__unidentified__").to_string();
        let was_complete = self.complete_messages.contains(&key);
        let matching_index = self.messages.iter().position(|(id, _)| id == &key);

        // Some agents omit messageId on chunks but include one on a complete
        // message (or do the reverse). If there is only one unambiguously
        // compatible aggregate, fold the complete value into it instead of
        // emitting the same assistant text a second time.
        if matching_index.is_none() {
            let aggregate = self
                .messages
                .iter()
                .map(|(_, message)| message.as_str())
                .collect::<String>();
            if aggregate == text && !self.messages.is_empty() {
                for (id, _) in &self.messages {
                    self.complete_messages.insert(id.clone());
                }
                self.complete_messages.insert(key);
                return None;
            }
        }
        let compatible_index = matching_index.or_else(|| {
            (self.messages.len() == 1).then(|| {
                let current = &self.messages[0].1;
                (text.starts_with(current) || current.starts_with(text)).then_some(0)
            })?
        });

        let Some(index) = compatible_index else {
            self.messages.push((key.clone(), text.to_string()));
            self.complete_messages.insert(key);
            return (!was_complete && !text.is_empty()).then(|| text.to_string());
        };
        let existing_key = self.messages[index].0.clone();
        let message = &mut self.messages[index].1;
        let delta = text
            .strip_prefix(message.as_str())
            .filter(|suffix| !suffix.is_empty())
            .map(ToOwned::to_owned);
        if message != text {
            *message = text.to_string();
        }
        self.complete_messages.insert(key);
        self.complete_messages.insert(existing_key);
        delta
    }
}

/// One initialized ACP process shared by all sessions for one provider.
pub struct AcpController {
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<AcpMappings>>,
    event_tx: mpsc::Sender<AcpRuntimeEvent>,
    events: Mutex<Receiver<AcpRuntimeEvent>>,
    event_worker: Mutex<Option<thread::JoinHandle<()>>>,
    healthy: Arc<AtomicBool>,
    capabilities: Value,
    control_timeout: Duration,
    prompt_timeout: Duration,
    provider_label: String,
}

impl fmt::Debug for AcpController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcpController")
            .field("provider", &self.provider_label)
            .field("generation", &self.generation())
            .field("pid", &self.process_id())
            .field("healthy", &self.is_healthy())
            .finish_non_exhaustive()
    }
}

impl AcpController {
    pub fn spawn(
        config: AcpLaunchConfig,
        permission_resolver: Arc<dyn AcpPermissionResolver>,
    ) -> Result<Arc<Self>, AcpRuntimeError> {
        Self::spawn_with_extension_handler(config, permission_resolver, None)
    }

    pub fn spawn_with_extension_handler(
        config: AcpLaunchConfig,
        permission_resolver: Arc<dyn AcpPermissionResolver>,
        extension_handler: Option<Arc<dyn AcpExtensionRequestHandler>>,
    ) -> Result<Arc<Self>, AcpRuntimeError> {
        if config.program.as_os_str().is_empty() {
            return Err(start_error("spawn", "ACP executable path is empty"));
        }
        let transport = Arc::new(
            PersistentRuntimeController::spawn_with_envelope_mode(
                config.process_spec(),
                config.max_frame_bytes,
                RpcEnvelopeMode::JsonRpc2,
            )
            .map_err(|error| start_error("spawn", error))?,
        );
        let initialize = transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": config.client_name,
                        "title": config.client_title,
                        "version": config.client_version,
                    }
                }),
                config.control_timeout,
            )
            .map_err(|error| request_error("initialize", error))?;
        if initialize.get("protocolVersion").and_then(Value::as_u64)
            != Some(u64::from(ACP_PROTOCOL_VERSION))
        {
            let _ = transport.shutdown_with_grace(Duration::from_millis(100));
            return Err(start_error(
                "initialize",
                "agent did not negotiate ACP protocol version 1",
            ));
        }
        if let AcpAuthentication::MethodId(method_id) = &config.authentication {
            let advertised =
                initialize
                    .get("authMethods")
                    .and_then(Value::as_array)
                    .map(|methods| {
                        methods.iter().any(|method| {
                            method.get("id").and_then(Value::as_str) == Some(method_id)
                                || method.get("methodId").and_then(Value::as_str) == Some(method_id)
                        })
                    });
            match advertised {
                Some(true) | None => {
                    transport
                        .request(
                            "authenticate",
                            json!({ "methodId": method_id }),
                            config.control_timeout,
                        )
                        .map_err(|error| request_error("authenticate", error))?;
                }
                Some(false) => {
                    let has_methods = initialize
                        .get("authMethods")
                        .and_then(Value::as_array)
                        .is_some_and(|methods| !methods.is_empty());
                    if has_methods {
                        let _ = transport.shutdown_with_grace(Duration::from_millis(100));
                        return Err(protocol_error(
                            "authenticate",
                            format!(
                                "configured authentication method {method_id} was not advertised"
                            ),
                        ));
                    }
                    // An empty authMethods array means the provider is already
                    // authenticated through its normal CLI environment/login.
                }
            }
        }
        let capabilities = initialize
            .get("agentCapabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let (events_tx, events_rx) = mpsc::channel();
        let worker_events_tx = events_tx.clone();
        let mappings = Arc::new(Mutex::new(AcpMappings::default()));
        transport.set_protocol_owner_resolver({
            let mappings = Arc::clone(&mappings);
            Arc::new(move |frame: &Value| {
                let provider_session_id = frame
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str)?;
                mappings
                    .lock()
                    .ok()?
                    .provider_to_pedelec
                    .get(provider_session_id)
                    .cloned()
            })
        });
        let healthy = Arc::new(AtomicBool::new(true));
        let event_worker = thread::Builder::new()
            .name(format!("pedelec-acp-events-{}", transport.generation()))
            .spawn({
                let transport = Arc::clone(&transport);
                let mappings = Arc::clone(&mappings);
                let healthy = Arc::clone(&healthy);
                let extension_handler = extension_handler.clone();
                move || {
                    run_event_worker(
                        transport,
                        mappings,
                        healthy,
                        permission_resolver,
                        extension_handler,
                        worker_events_tx,
                    )
                }
            })
            .map_err(|error| start_error("event-worker", error))?;

        Ok(Arc::new(Self {
            transport,
            mappings,
            event_tx: events_tx,
            events: Mutex::new(events_rx),
            event_worker: Mutex::new(Some(event_worker)),
            healthy,
            capabilities,
            control_timeout: config.control_timeout,
            prompt_timeout: config.prompt_timeout,
            provider_label: config.provider_label,
        }))
    }

    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    pub fn supports_load(&self) -> bool {
        self.capabilities
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false)
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

    pub fn retire_for_protocol_error(&self) {
        self.healthy.store(false, Ordering::Release);
        self.transport.retire();
    }

    pub fn ensure_session(
        &self,
        pedelec_thread_id: &str,
        persisted_provider_session_id: Option<&str>,
        config: &AcpSessionConfig,
    ) -> Result<String, AcpRuntimeError> {
        config.validate()?;
        if !self.is_healthy() {
            return Err(AcpRuntimeError::RuntimeDisconnected {
                operation: "session".to_string(),
                message: format!("{} ACP runtime is not healthy", self.provider_label),
            });
        }
        if persisted_provider_session_id.is_some_and(|session_id| session_id.trim().is_empty()) {
            return Err(protocol_error(
                "session/load",
                "persisted provider session id is empty",
            ));
        }
        if let Some(existing) = self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .cloned()
        {
            if persisted_provider_session_id.is_none_or(|persisted| persisted == existing) {
                return Ok(existing);
            }
            return Err(protocol_error(
                "session/attach",
                format!("Pedelec thread {pedelec_thread_id} is already attached to {existing}"),
            ));
        }

        self.transport
            .register_protocol_log(pedelec_thread_id, &self.provider_label, &config.cwd);
        let cwd = config.cwd.to_string_lossy();
        let (method, params) = match persisted_provider_session_id {
            Some(session_id) => {
                if !self.supports_load() {
                    return Err(protocol_error(
                        "session/load",
                        format!("{} does not advertise loadSession", self.provider_label),
                    ));
                }
                (
                    "session/load",
                    json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
                )
            }
            None => ("session/new", json!({ "cwd": cwd, "mcpServers": [] })),
        };
        let response = self
            .transport
            .request_scoped(pedelec_thread_id, method, params, self.control_timeout)
            .map_err(|error| request_error(method, error))?;
        let provider_session_id = match persisted_provider_session_id {
            Some(session_id) => {
                if !response.is_object() {
                    self.retire_for_protocol_error();
                    return Err(protocol_error(
                        method,
                        "session/load response must be an object",
                    ));
                }
                if response.get("sessionId").is_some() {
                    let Some(returned_id) = response.get("sessionId").and_then(Value::as_str)
                    else {
                        self.retire_for_protocol_error();
                        return Err(protocol_error(
                            method,
                            "session/load response sessionId is not a string",
                        ));
                    };
                    if returned_id.trim().is_empty() || returned_id != session_id {
                        self.retire_for_protocol_error();
                        return Err(protocol_error(
                            method,
                            "session/load response sessionId does not match the requested id",
                        ));
                    }
                }
                session_id.to_string()
            }
            None => {
                let Some(session_id) = response
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                else {
                    self.retire_for_protocol_error();
                    return Err(protocol_error(method, "response is missing sessionId"));
                };
                session_id.to_string()
            }
        };
        self.register_session(pedelec_thread_id, &provider_session_id)?;
        self.mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .session_config_options
            .insert(
                provider_session_id.clone(),
                response
                    .get("configOptions")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            );
        {
            let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
            mappings.session_modes.insert(
                provider_session_id.clone(),
                response.get("modes").cloned().unwrap_or_else(|| json!({})),
            );
            mappings.session_origins.insert(
                provider_session_id.clone(),
                if persisted_provider_session_id.is_some() {
                    AcpSessionOrigin::Loaded
                } else {
                    AcpSessionOrigin::New
                },
            );
        }
        for option in &config.config_options {
            if let Err(error) = self.transport.request(
                "session/set_config_option",
                json!({
                    "sessionId": provider_session_id,
                    "configId": option.config_id,
                    "value": option.value,
                }),
                self.control_timeout,
            ) {
                self.unregister_session(pedelec_thread_id, &provider_session_id);
                return Err(request_error("session/set_config_option", error));
            }
        }
        let _ = self.event_sender().send(AcpRuntimeEvent::SessionReady {
            pedelec_thread_id: pedelec_thread_id.to_string(),
            provider_session_id: provider_session_id.clone(),
            config_options: response.get("configOptions").cloned(),
        });
        Ok(provider_session_id)
    }

    /// Admits a long-lived prompt without waiting for its terminal response.
    pub fn start_turn(
        self: &Arc<Self>,
        pedelec_thread_id: &str,
        provider_session_id: &str,
        local_turn_id: &str,
        prompt: &str,
    ) -> Result<(), AcpRuntimeError> {
        if !self.is_healthy() {
            return Err(AcpRuntimeError::RuntimeDisconnected {
                operation: "session/prompt".to_string(),
                message: format!("{} ACP runtime is not healthy", self.provider_label),
            });
        }
        {
            let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
            if mappings
                .pedelec_to_provider
                .get(pedelec_thread_id)
                .map(String::as_str)
                != Some(provider_session_id)
            {
                return Err(protocol_error(
                    "session/prompt",
                    "provider session is not attached to this Pedelec thread",
                ));
            }
            if mappings.pending_turns.contains_key(provider_session_id) {
                return Err(protocol_error(
                    "session/prompt",
                    "provider session already has an active prompt",
                ));
            }
            mappings.pending_turns.insert(
                provider_session_id.to_string(),
                AcpPendingTurn {
                    local_turn_id: local_turn_id.to_string(),
                    messages: Vec::new(),
                    update_revision: 0,
                    response_received: false,
                    complete_messages: HashSet::new(),
                },
            );
        }
        let controller = Arc::clone(self);
        let pedelec_thread_id = pedelec_thread_id.to_string();
        let provider_session_id = provider_session_id.to_string();
        let cleanup_session_id = provider_session_id.clone();
        let local_turn_id = local_turn_id.to_string();
        let prompt = prompt.to_string();
        let prompt_timeout = self.prompt_timeout;
        thread::Builder::new()
            .name(format!("pedelec-acp-prompt-{local_turn_id}"))
            .spawn(move || {
                let response = controller.transport.request(
                    "session/prompt",
                    json!({
                        "sessionId": provider_session_id,
                        "prompt": [{ "type": "text", "text": prompt }],
                    }),
                    // Prompt requests are intentionally long-lived and run
                    // outside Core locks, but remain bounded by launch policy.
                    prompt_timeout,
                );
                controller.finish_prompt(
                    &pedelec_thread_id,
                    &provider_session_id,
                    &local_turn_id,
                    response,
                );
            })
            .map_err(|error| {
                self.mappings
                    .lock()
                    .expect("ACP mappings mutex poisoned")
                    .pending_turns
                    .remove(&cleanup_session_id);
                start_error("session/prompt-worker", error)
            })?;
        Ok(())
    }

    pub fn cancel_turn(&self, provider_session_id: &str) -> Result<(), AcpRuntimeError> {
        let should_cancel = self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .pending_turns
            .get(provider_session_id)
            .is_some_and(|turn| !turn.response_received);
        if !should_cancel {
            return Ok(());
        }
        self.transport
            .notify(
                "session/cancel",
                json!({ "sessionId": provider_session_id }),
            )
            .map_err(|error| request_error("session/cancel", error))
    }

    /// Cancels an active prompt when possible and always removes the local
    /// attachment. Ending a Pedelec thread must not leave an ACP turn or
    /// workspace policy reachable by late provider callbacks.
    pub fn cancel_and_detach_session(
        &self,
        pedelec_thread_id: &str,
        provider_session_id: &str,
    ) -> Result<(), AcpRuntimeError> {
        let pending_turn_id = self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .pending_turns
            .get(provider_session_id)
            .map(|turn| (turn.local_turn_id.clone(), !turn.response_received));
        // Detach first so a concurrent permission/update callback is treated
        // as stale while the cancellation notification is being written. Keep
        // the pending turn briefly so a provider terminal response can close
        // the in-flight request before Core finalizes the ended thread.
        self.detach_session_attachment(pedelec_thread_id, provider_session_id);
        let cancel_result = if pending_turn_id
            .as_ref()
            .is_some_and(|(_, should_cancel)| *should_cancel)
        {
            self.transport
                .notify(
                    "session/cancel",
                    json!({ "sessionId": provider_session_id }),
                )
                .map_err(|error| request_error("session/cancel", error))
        } else {
            Ok(())
        };
        if let Err(error) = cancel_result {
            self.remove_pending_turn(
                provider_session_id,
                pending_turn_id.as_ref().map(|turn| turn.0.as_str()),
            );
            return Err(error);
        }
        if let Some((local_turn_id, _)) = pending_turn_id {
            self.wait_for_cancelled_turn(provider_session_id, &local_turn_id);
        }
        Ok(())
    }

    /// Drops a local attachment without attempting provider I/O. This is used
    /// after a transport has already disconnected, when waiting for another
    /// cancellation write would only prolong thread teardown.
    pub fn forget_session(&self, pedelec_thread_id: &str) {
        let provider_session_id = self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .cloned();
        if let Some(provider_session_id) = provider_session_id {
            self.unregister_session(pedelec_thread_id, &provider_session_id);
        }
    }

    pub fn set_config_option(
        &self,
        provider_session_id: &str,
        option: &AcpConfigOptionUpdate,
    ) -> Result<Value, AcpRuntimeError> {
        if !self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .provider_to_pedelec
            .contains_key(provider_session_id)
        {
            return Err(protocol_error(
                "session/set_config_option",
                "provider session is not attached",
            ));
        }
        let response = self
            .transport
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": provider_session_id,
                    "configId": option.config_id,
                    "value": option.value,
                }),
                self.control_timeout,
            )
            .map_err(|error| request_error("session/set_config_option", error))?;
        if let Some(config_options) = response.get("configOptions").cloned() {
            self.mappings
                .lock()
                .expect("ACP mappings mutex poisoned")
                .session_config_options
                .insert(provider_session_id.to_string(), config_options);
        }
        Ok(response)
    }

    pub fn session_config_options(&self, provider_session_id: &str) -> Option<Value> {
        self.mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .session_config_options
            .get(provider_session_id)
            .cloned()
    }

    pub fn session_modes(&self, provider_session_id: &str) -> Option<Value> {
        self.mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .session_modes
            .get(provider_session_id)
            .cloned()
    }

    pub fn set_mode(
        &self,
        provider_session_id: &str,
        mode_id: &str,
    ) -> Result<Value, AcpRuntimeError> {
        if !self
            .mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .provider_to_pedelec
            .contains_key(provider_session_id)
        {
            return Err(protocol_error(
                "session/set_mode",
                "provider session is not attached",
            ));
        }
        let response = self
            .transport
            .request(
                "session/set_mode",
                json!({ "sessionId": provider_session_id, "modeId": mode_id }),
                self.control_timeout,
            )
            .map_err(|error| request_error("session/set_mode", error))?;
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        let modes = mappings
            .session_modes
            .entry(provider_session_id.to_string())
            .or_insert_with(|| json!({}));
        if let Some(object) = modes.as_object_mut() {
            object.insert("currentModeId".into(), Value::String(mode_id.to_string()));
        }
        Ok(response)
    }

    pub fn session_needs_bootstrap(&self, provider_session_id: &str) -> bool {
        self.mappings
            .lock()
            .expect("ACP mappings mutex poisoned")
            .session_origins
            .get(provider_session_id)
            == Some(&AcpSessionOrigin::New)
    }

    pub fn mark_session_bootstrapped(&self, provider_session_id: &str) {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        if mappings.session_origins.get(provider_session_id) == Some(&AcpSessionOrigin::New) {
            mappings
                .session_origins
                .insert(provider_session_id.to_string(), AcpSessionOrigin::Loaded);
        }
    }

    /// Locally detaches an idle session. ACP v1 close support is deliberately
    /// not required by this foundation.
    pub fn detach_session(&self, pedelec_thread_id: &str) -> Result<(), AcpRuntimeError> {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        let Some(provider_id) = mappings.pedelec_to_provider.get(pedelec_thread_id).cloned() else {
            return Ok(());
        };
        if mappings.pending_turns.contains_key(&provider_id) {
            return Err(protocol_error(
                "session/detach",
                "cannot detach a session with an active prompt",
            ));
        }
        mappings.pedelec_to_provider.remove(pedelec_thread_id);
        mappings.provider_to_pedelec.remove(&provider_id);
        mappings.session_config_options.remove(&provider_id);
        mappings.session_modes.remove(&provider_id);
        mappings.session_origins.remove(&provider_id);
        Ok(())
    }

    pub fn recv_event(&self) -> Result<AcpRuntimeEvent, mpsc::RecvError> {
        self.events
            .lock()
            .expect("ACP events mutex poisoned")
            .recv()
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AcpRuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("ACP events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn shutdown(&self) -> Result<(), AcpRuntimeError> {
        self.healthy.store(false, Ordering::Release);
        self.transport
            .shutdown_with_grace(Duration::from_secs(1))
            .map(|_| ())
            .map_err(|error| request_error("shutdown", error))
    }

    fn register_session(
        &self,
        pedelec_thread_id: &str,
        provider_session_id: &str,
    ) -> Result<(), AcpRuntimeError> {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        if let Some(existing) = mappings.provider_to_pedelec.get(provider_session_id) {
            if existing != pedelec_thread_id {
                return Err(protocol_error(
                    "session/attach",
                    format!(
                        "provider session {provider_session_id} is already attached to {existing}"
                    ),
                ));
            }
        }
        mappings.pedelec_to_provider.insert(
            pedelec_thread_id.to_string(),
            provider_session_id.to_string(),
        );
        mappings.provider_to_pedelec.insert(
            provider_session_id.to_string(),
            pedelec_thread_id.to_string(),
        );
        Ok(())
    }

    fn unregister_session(&self, pedelec_thread_id: &str, provider_session_id: &str) {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        if mappings
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .map(String::as_str)
            != Some(provider_session_id)
        {
            return;
        }
        mappings.pedelec_to_provider.remove(pedelec_thread_id);
        mappings.provider_to_pedelec.remove(provider_session_id);
        mappings.session_config_options.remove(provider_session_id);
        mappings.session_modes.remove(provider_session_id);
        mappings.session_origins.remove(provider_session_id);
        mappings.pending_turns.remove(provider_session_id);
    }

    fn detach_session_attachment(&self, pedelec_thread_id: &str, provider_session_id: &str) {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        if mappings
            .pedelec_to_provider
            .get(pedelec_thread_id)
            .map(String::as_str)
            != Some(provider_session_id)
        {
            return;
        }
        mappings.pedelec_to_provider.remove(pedelec_thread_id);
        mappings.provider_to_pedelec.remove(provider_session_id);
        mappings.session_config_options.remove(provider_session_id);
        mappings.session_modes.remove(provider_session_id);
        mappings.session_origins.remove(provider_session_id);
    }

    fn remove_pending_turn(&self, provider_session_id: &str, local_turn_id: Option<&str>) {
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        if local_turn_id.is_none_or(|local_turn_id| {
            mappings
                .pending_turns
                .get(provider_session_id)
                .is_some_and(|turn| turn.local_turn_id == local_turn_id)
        }) {
            mappings.pending_turns.remove(provider_session_id);
            mappings.turn_changed.notify_all();
        }
    }

    fn wait_for_cancelled_turn(&self, provider_session_id: &str, local_turn_id: &str) {
        let deadline = Instant::now() + ACP_CANCEL_DRAIN_TIMEOUT;
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        while Instant::now() < deadline {
            let Some(turn) = mappings.pending_turns.get(provider_session_id) else {
                return;
            };
            if turn.local_turn_id != local_turn_id {
                return;
            }
            let changed = Arc::clone(&mappings.turn_changed);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, _) = changed
                .wait_timeout(mappings, remaining.min(Duration::from_millis(20)))
                .expect("ACP mappings mutex poisoned while draining cancelled turn");
            mappings = next;
        }
        if mappings
            .pending_turns
            .get(provider_session_id)
            .is_some_and(|turn| turn.local_turn_id == local_turn_id)
        {
            mappings.pending_turns.remove(provider_session_id);
            mappings.turn_changed.notify_all();
        }
    }

    fn finish_prompt(
        &self,
        pedelec_thread_id: &str,
        provider_session_id: &str,
        local_turn_id: &str,
        response: Result<Value, RuntimeControllerError>,
    ) {
        // Mark the request terminal before waiting for already-read updates.
        // This closes the prompt-response/cancel race: a cancel arriving after
        // the provider response is observed is harmless and does not rewrite a
        // successful turn as an interruption.
        {
            let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
            let Some(turn) = mappings.pending_turns.get_mut(provider_session_id) else {
                // Disconnect, detach, or a previous terminal reducer already
                // removed this request. Its response is intentionally stale.
                return;
            };
            if turn.local_turn_id != local_turn_id {
                return;
            }
            turn.response_received = true;
        }

        // The RPC reader enqueues notifications before resolving the prompt
        // response, but the normalization worker is independent. Give it a
        // bounded quiescence window so final aggregation cannot overtake
        // already-read `session/update` frames.
        // Some ACP implementations put the complete assistant message on the
        // prompt result instead of sending a session/update. Fold that value
        // into the same aggregate before the quiescence wait so both forms
        // produce one final assistant event.
        if let Ok(value) = response.as_ref() {
            if let Some((message_id, text)) = extract_complete_assistant(value) {
                let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
                if let Some(turn) = mappings.pending_turns.get_mut(provider_session_id) {
                    if turn.local_turn_id == local_turn_id {
                        turn.set_complete(message_id.as_deref(), &text);
                        turn.update_revision = turn.update_revision.wrapping_add(1);
                        mappings.turn_changed.notify_all();
                    }
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
        let mut revision = mappings
            .pending_turns
            .get(provider_session_id)
            .map(|turn| turn.update_revision)
            .unwrap_or_default();
        while Instant::now() < deadline {
            let changed = Arc::clone(&mappings.turn_changed);
            let (next, wait) = changed
                .wait_timeout(mappings, Duration::from_millis(20))
                .expect("ACP mappings mutex poisoned while awaiting final updates");
            mappings = next;
            let next_revision = mappings
                .pending_turns
                .get(provider_session_id)
                .map(|turn| turn.update_revision)
                .unwrap_or(revision);
            if wait.timed_out() && next_revision == revision {
                break;
            }
            revision = next_revision;
        }
        let Some(turn) = mappings.pending_turns.remove(provider_session_id) else {
            // A disconnect or end operation won the race while we were
            // waiting for final updates. Do not emit a second terminal event.
            return;
        };
        mappings.turn_changed.notify_all();
        if turn.local_turn_id != local_turn_id {
            return;
        }
        // A Pedelec user turn has one assistant-message terminal event. Keep
        // provider message ids only for aggregation/replacement; do not leak
        // provider segmentation as duplicate final chat messages.
        let final_text = turn
            .messages
            .into_iter()
            .map(|(_, text)| text)
            .collect::<String>();
        let sender = self.event_sender();
        if !final_text.is_empty() {
            let _ = sender.send(AcpRuntimeEvent::AssistantMessage {
                pedelec_thread_id: pedelec_thread_id.to_string(),
                provider_session_id: provider_session_id.to_string(),
                local_turn_id: local_turn_id.to_string(),
                text: final_text,
            });
        }
        match response {
            Ok(value) => {
                let Some(stop_reason) = value.get("stopReason").and_then(Value::as_str) else {
                    self.retire_for_protocol_error();
                    let _ = sender.send(AcpRuntimeEvent::ProtocolError {
                        pedelec_thread_id: Some(pedelec_thread_id.to_string()),
                        provider_session_id: Some(provider_session_id.to_string()),
                        operation: "session/prompt".to_string(),
                        message: "response is missing stopReason".to_string(),
                    });
                    return;
                };
                let status = match stop_reason {
                    "end_turn" => AcpTurnStatus::Completed,
                    "cancelled" => AcpTurnStatus::Interrupted,
                    _ => AcpTurnStatus::Failed,
                };
                let _ = sender.send(AcpRuntimeEvent::TurnCompleted {
                    pedelec_thread_id: pedelec_thread_id.to_string(),
                    provider_session_id: provider_session_id.to_string(),
                    local_turn_id: local_turn_id.to_string(),
                    status,
                    stop_reason: stop_reason.to_string(),
                    error: None,
                });
            }
            Err(error) => {
                // A transport disconnect is reported by the runtime event
                // worker. Do not race it with a request-failed terminal event;
                // otherwise Core could settle the active turn as a generic
                // request error before it receives the authoritative runtime
                // disconnected event. Timeouts and provider rejections remain
                // ordinary request failures and settle this turn exactly once.
                if is_transport_disconnect(&error) {
                    let mut mappings = self.mappings.lock().expect("ACP mappings mutex poisoned");
                    mappings.pending_turns.remove(provider_session_id);
                    return;
                }
                let _ = sender.send(AcpRuntimeEvent::TurnCompleted {
                    pedelec_thread_id: pedelec_thread_id.to_string(),
                    provider_session_id: provider_session_id.to_string(),
                    local_turn_id: local_turn_id.to_string(),
                    status: AcpTurnStatus::Failed,
                    stop_reason: request_stop_reason(&error).to_string(),
                    error: Some(runtime_error_value(&error)),
                });
            }
        }
    }

    fn event_sender(&self) -> mpsc::Sender<AcpRuntimeEvent> {
        self.event_tx.clone()
    }
}

impl Drop for AcpController {
    fn drop(&mut self) {
        self.healthy.store(false, Ordering::Release);
        self.transport.retire();
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("ACP event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl ProviderRuntimeController for AcpController {
    fn shutdown(&self) -> Result<(), String> {
        AcpController::shutdown(self).map_err(|error| error.to_string())
    }

    fn is_healthy(&self) -> bool {
        AcpController::is_healthy(self)
    }
}

fn run_event_worker(
    transport: Arc<PersistentRuntimeController>,
    mappings: Arc<Mutex<AcpMappings>>,
    healthy: Arc<AtomicBool>,
    permission_resolver: Arc<dyn AcpPermissionResolver>,
    extension_handler: Option<Arc<dyn AcpExtensionRequestHandler>>,
    events: mpsc::Sender<AcpRuntimeEvent>,
) {
    while let Ok(event) = transport.recv_event() {
        match event {
            RuntimeEvent::Rpc(RpcEvent::Notification { method, params }) => {
                if method == "session/update" {
                    if handle_session_update(&mappings, &events, params) {
                        healthy.store(false, Ordering::Release);
                        transport.retire();
                    }
                } else {
                    let _ = events.send(AcpRuntimeEvent::Notification {
                        method,
                        params,
                        pedelec_thread_id: None,
                        provider_session_id: None,
                        local_turn_id: None,
                    });
                }
            }
            RuntimeEvent::Rpc(RpcEvent::ServerRequest(request)) => {
                handle_server_request(
                    &transport,
                    &mappings,
                    &events,
                    &healthy,
                    permission_resolver.as_ref(),
                    extension_handler.as_deref(),
                    request,
                );
            }
            RuntimeEvent::Rpc(RpcEvent::Stderr { text }) => {
                let _ = events.send(AcpRuntimeEvent::Stderr { text });
            }
            RuntimeEvent::Rpc(RpcEvent::UnmatchedResponse { id, response }) => {
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "$/unmatched_response".to_string(),
                    params: json!({ "id": format!("{id:?}"), "response": response }),
                    pedelec_thread_id: None,
                    provider_session_id: None,
                    local_turn_id: None,
                });
            }
            RuntimeEvent::Rpc(RpcEvent::Disconnected { reason }) => {
                healthy.store(false, Ordering::Release);
                let attachments = clear_mappings(&mappings);
                let _ = events.send(AcpRuntimeEvent::Disconnected {
                    generation: transport.generation(),
                    pid: transport.process_id(),
                    attachments,
                    reason,
                });
                break;
            }
            RuntimeEvent::ProcessExit(_) => {
                // The RPC reader supplies the authoritative disconnect reason.
            }
        }
    }
}

fn handle_session_update(
    mappings: &Arc<Mutex<AcpMappings>>,
    events: &mpsc::Sender<AcpRuntimeEvent>,
    params: Value,
) -> bool {
    let Some(session_id) = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        let _ = events.send(AcpRuntimeEvent::ProtocolError {
            pedelec_thread_id: None,
            provider_session_id: None,
            operation: "session/update".to_string(),
            message: "notification is missing sessionId".to_string(),
        });
        return true;
    };
    let Some(update) = params.get("update").and_then(Value::as_object) else {
        let _ = events.send(AcpRuntimeEvent::ProtocolError {
            pedelec_thread_id: None,
            provider_session_id: Some(session_id.clone()),
            operation: "session/update".to_string(),
            message: "notification is missing update object".to_string(),
        });
        return true;
    };
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        let _ = events.send(AcpRuntimeEvent::ProtocolError {
            pedelec_thread_id: None,
            provider_session_id: Some(session_id.to_string()),
            operation: "session/update".to_string(),
            message: "update is missing sessionUpdate discriminator".to_string(),
        });
        return true;
    };
    let mut mappings = mappings.lock().expect("ACP mappings mutex poisoned");
    let pedelec_id = mappings.provider_to_pedelec.get(&session_id).cloned();
    let local_turn_id = mappings
        .pending_turns
        .get(&session_id)
        .map(|turn| turn.local_turn_id.clone());
    match kind {
        "agent_message_chunk" | "assistant_message_chunk" => {
            let Some(text) = extract_update_text(update) else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: pedelec_id,
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id,
                });
                return false;
            };
            let Some(pedelec_thread_id) = pedelec_id else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: None,
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id: None,
                });
                return false;
            };
            let Some(turn) = mappings.pending_turns.get_mut(&session_id) else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: Some(pedelec_thread_id),
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id: None,
                });
                return false;
            };
            turn.append(update.get("messageId").and_then(Value::as_str), &text);
            turn.update_revision = turn.update_revision.wrapping_add(1);
            let local_turn_id = turn.local_turn_id.clone();
            mappings.turn_changed.notify_all();
            drop(mappings);
            if !text.is_empty() {
                let _ = events.send(AcpRuntimeEvent::AssistantDelta {
                    pedelec_thread_id,
                    provider_session_id: session_id.clone(),
                    local_turn_id,
                    text,
                });
            }
        }
        "agent_message"
        | "assistant_message"
        | "agent_message_complete"
        | "assistant_message_complete" => {
            let Some(text) = extract_update_text(update) else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: pedelec_id,
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id,
                });
                return false;
            };
            let Some(pedelec_thread_id) = pedelec_id else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: None,
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id: None,
                });
                return false;
            };
            let Some(turn) = mappings.pending_turns.get_mut(&session_id) else {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::Notification {
                    method: "session/update".to_string(),
                    params,
                    pedelec_thread_id: Some(pedelec_thread_id),
                    provider_session_id: Some(session_id.clone()),
                    local_turn_id: None,
                });
                return false;
            };
            let delta = turn.set_complete(update.get("messageId").and_then(Value::as_str), &text);
            turn.update_revision = turn.update_revision.wrapping_add(1);
            let local_turn_id = turn.local_turn_id.clone();
            mappings.turn_changed.notify_all();
            drop(mappings);
            if let Some(text) = delta.filter(|text| !text.is_empty()) {
                let _ = events.send(AcpRuntimeEvent::AssistantDelta {
                    pedelec_thread_id,
                    provider_session_id: session_id.clone(),
                    local_turn_id,
                    text,
                });
            }
        }
        "usage_update" => {
            if !update.get("used").is_some_and(Value::is_number)
                || !update.get("size").is_some_and(Value::is_number)
            {
                drop(mappings);
                let _ = events.send(AcpRuntimeEvent::ProtocolError {
                    pedelec_thread_id: pedelec_id,
                    provider_session_id: Some(session_id),
                    operation: "session/update".to_string(),
                    message: "usage_update is missing numeric used or size".to_string(),
                });
                return true;
            }
            let Some(pedelec_thread_id) = pedelec_id else {
                drop(mappings);
                return false;
            };
            let usage = Value::Object(update.clone());
            drop(mappings);
            let _ = events.send(AcpRuntimeEvent::UsageUpdated {
                pedelec_thread_id,
                provider_session_id: session_id.clone(),
                local_turn_id,
                usage,
            });
        }
        _ => {
            drop(mappings);
            let _ = events.send(AcpRuntimeEvent::Notification {
                method: "session/update".to_string(),
                params,
                pedelec_thread_id: pedelec_id,
                provider_session_id: Some(session_id),
                local_turn_id,
            });
        }
    }
    false
}

fn extract_update_text(update: &serde_json::Map<String, Value>) -> Option<String> {
    update
        .get("content")
        .and_then(extract_text_content)
        .or_else(|| update.get("message").and_then(extract_assistant_text))
}

fn extract_text_content(value: &Value) -> Option<String> {
    let mut text = String::new();
    if collect_text_content(value, &mut text) {
        Some(text)
    } else {
        None
    }
}

fn collect_text_content(value: &Value, output: &mut String) -> bool {
    match value {
        Value::String(text) => {
            output.push_str(text);
            true
        }
        Value::Object(content) if content.get("type").and_then(Value::as_str) == Some("text") => {
            let Some(text) = content.get("text").and_then(Value::as_str) else {
                return false;
            };
            output.push_str(text);
            true
        }
        Value::Array(values) => values.iter().fold(false, |found, value| {
            collect_text_content(value, output) || found
        }),
        _ => false,
    }
}

fn extract_assistant_text(value: &Value) -> Option<String> {
    extract_text_content(value).or_else(|| value.get("content").and_then(extract_text_content))
}

fn extract_complete_assistant(value: &Value) -> Option<(Option<String>, String)> {
    for key in ["assistantMessage", "message", "content"] {
        let Some(candidate) = value.get(key) else {
            continue;
        };
        if let Some(text) = extract_assistant_text(candidate) {
            let message_id = value
                .get("messageId")
                .and_then(Value::as_str)
                .or_else(|| candidate.get("messageId").and_then(Value::as_str))
                .map(ToOwned::to_owned);
            return Some((message_id, text));
        }
    }
    None
}

fn handle_server_request(
    transport: &PersistentRuntimeController,
    mappings: &Arc<Mutex<AcpMappings>>,
    events: &mpsc::Sender<AcpRuntimeEvent>,
    healthy: &Arc<AtomicBool>,
    resolver: &dyn AcpPermissionResolver,
    extension_handler: Option<&dyn AcpExtensionRequestHandler>,
    request: RpcServerRequest,
) {
    if request.method != "session/request_permission" {
        if let Some(result) = extension_handler.and_then(|handler| handler.handle(&request)) {
            let method = request.method.clone();
            let session_id = request
                .params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let (pedelec_thread_id, provider_session_id, local_turn_id) = session_id
                .as_deref()
                .and_then(|session_id| {
                    let mappings = mappings.lock().ok()?;
                    let thread = mappings.provider_to_pedelec.get(session_id).cloned()?;
                    let local_turn = mappings
                        .pending_turns
                        .get(session_id)
                        .map(|turn| turn.local_turn_id.clone());
                    Some((Some(thread), Some(session_id.to_string()), local_turn))
                })
                .unwrap_or((None, session_id, None));
            match transport.respond_success(request.id, result) {
                Ok(()) => {
                    let _ = events.send(AcpRuntimeEvent::Notification {
                        method,
                        params: request.params,
                        pedelec_thread_id,
                        provider_session_id,
                        local_turn_id,
                    });
                }
                Err(error) => response_write_failed(
                    transport,
                    mappings,
                    healthy,
                    events,
                    Some("extension callback response"),
                    error.to_string(),
                    None,
                    None,
                ),
            }
            return;
        }
        if let Err(error) = transport.respond_error(
            request.id,
            json!(-32601),
            "client method is not implemented",
            Some(json!({ "method": request.method })),
        ) {
            response_write_failed(
                transport,
                mappings,
                healthy,
                events,
                Some("unsupported server request"),
                error.to_string(),
                None,
                None,
            );
        }
        return;
    }
    let Some(session_id) = request.params.get("sessionId").and_then(Value::as_str) else {
        respond_permission_error(
            transport,
            mappings,
            healthy,
            events,
            request.id,
            "missing sessionId",
            None,
            None,
        );
        return;
    };
    let Some(options) = request.params.get("options").and_then(Value::as_array) else {
        respond_permission_error(
            transport,
            mappings,
            healthy,
            events,
            request.id,
            "missing options",
            None,
            Some(session_id),
        );
        return;
    };
    let Some(tool_call) = request.params.get("toolCall") else {
        respond_permission_error(
            transport,
            mappings,
            healthy,
            events,
            request.id,
            "missing toolCall",
            None,
            Some(session_id),
        );
        return;
    };
    let pedelec_thread_id = mappings
        .lock()
        .expect("ACP mappings mutex poisoned")
        .provider_to_pedelec
        .get(session_id)
        .cloned();
    let Some(pedelec_thread_id) = pedelec_thread_id else {
        respond_permission_error(
            transport,
            mappings,
            healthy,
            events,
            request.id,
            "unknown sessionId",
            None,
            Some(session_id),
        );
        return;
    };
    let permission = AcpPermissionRequest {
        pedelec_thread_id: pedelec_thread_id.clone(),
        provider_session_id: session_id.to_string(),
        tool_call: tool_call.clone(),
        options: Value::Array(options.clone()),
    };
    let requested = resolver.resolve(&permission);
    let desired_kind = match requested {
        AcpPermissionDecision::AllowOnce => "allow_once",
        AcpPermissionDecision::RejectOnce => "reject_once",
    };
    // Never escalate to allow_always. If allow_once is unavailable, safely
    // fall back to a one-shot rejection if the provider offered one.
    let selection = find_option(options, desired_kind).or_else(|| {
        (requested == AcpPermissionDecision::AllowOnce)
            .then(|| find_option(options, "reject_once"))
            .flatten()
    });
    let Some((option_id, actual_decision)) = selection else {
        match transport.respond_error(
            request.id,
            json!(-32602),
            "permission options cannot be mapped safely",
            None,
        ) {
            Ok(()) => {
                healthy.store(false, Ordering::Release);
                transport.retire();
                let _ = events.send(AcpRuntimeEvent::ProtocolError {
                    pedelec_thread_id: Some(pedelec_thread_id),
                    provider_session_id: Some(session_id.to_string()),
                    operation: "session/request_permission".to_string(),
                    message: "no safe allow_once or reject_once option was provided".to_string(),
                });
            }
            Err(error) => response_write_failed(
                transport,
                mappings,
                healthy,
                events,
                Some("permission rejection response"),
                error.to_string(),
                Some(pedelec_thread_id),
                Some(session_id),
            ),
        }
        return;
    };
    match transport.respond_success(
        request.id,
        json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
    ) {
        Ok(()) => {
            let _ = events.send(AcpRuntimeEvent::PermissionResolved {
                pedelec_thread_id,
                provider_session_id: session_id.to_string(),
                option_id: option_id.to_string(),
                decision: actual_decision,
            });
        }
        Err(error) => response_write_failed(
            transport,
            mappings,
            healthy,
            events,
            Some("permission response"),
            error.to_string(),
            Some(pedelec_thread_id),
            Some(session_id),
        ),
    }
}

fn respond_permission_error(
    transport: &PersistentRuntimeController,
    mappings: &Arc<Mutex<AcpMappings>>,
    healthy: &Arc<AtomicBool>,
    events: &mpsc::Sender<AcpRuntimeEvent>,
    id: crate::RpcId,
    message: &str,
    pedelec_thread_id: Option<String>,
    provider_session_id: Option<&str>,
) {
    if let Err(error) = transport.respond_error(id, json!(-32602), message, None) {
        response_write_failed(
            transport,
            mappings,
            healthy,
            events,
            Some("permission validation response"),
            error.to_string(),
            pedelec_thread_id,
            provider_session_id,
        );
    }
}

fn response_write_failed(
    transport: &PersistentRuntimeController,
    mappings: &Arc<Mutex<AcpMappings>>,
    healthy: &Arc<AtomicBool>,
    events: &mpsc::Sender<AcpRuntimeEvent>,
    operation: Option<&str>,
    error: String,
    pedelec_thread_id: Option<String>,
    provider_session_id: Option<&str>,
) {
    healthy.store(false, Ordering::Release);
    transport.retire();
    clear_mappings(mappings);
    let _ = events.send(AcpRuntimeEvent::ProtocolError {
        pedelec_thread_id,
        provider_session_id: provider_session_id.map(ToOwned::to_owned),
        operation: operation.unwrap_or("server response").to_string(),
        message: format!("ACP response could not be written: {error}"),
    });
}

fn find_option<'a>(options: &'a [Value], kind: &str) -> Option<(&'a str, AcpPermissionDecision)> {
    options.iter().find_map(|option| {
        (option.get("kind").and_then(Value::as_str) == Some(kind))
            .then(|| option.get("optionId").and_then(Value::as_str))
            .flatten()
            .filter(|id| !id.trim().is_empty())
            .map(|id| {
                (
                    id,
                    if kind == "allow_once" {
                        AcpPermissionDecision::AllowOnce
                    } else {
                        AcpPermissionDecision::RejectOnce
                    },
                )
            })
    })
}

fn is_transport_disconnect(error: &RuntimeControllerError) -> bool {
    matches!(
        error,
        RuntimeControllerError::Process(_)
            | RuntimeControllerError::JsonLine(_)
            | RuntimeControllerError::Rpc(RpcError::Disconnected { .. } | RpcError::Write { .. })
            | RuntimeControllerError::CommandChannelClosed
            | RuntimeControllerError::WorkerChannelClosed
            | RuntimeControllerError::Shutdown(_)
    )
}

fn request_stop_reason(error: &RuntimeControllerError) -> &'static str {
    match error {
        RuntimeControllerError::Rpc(RpcError::RequestTimeout { .. }) => "timeout",
        RuntimeControllerError::Rpc(RpcError::Remote { .. }) => "provider_rejected",
        RuntimeControllerError::Rpc(RpcError::InvalidMessage { .. }) => "invalid_response",
        _ => "request_error",
    }
}

/// Keep provider error payloads useful for diagnostics without allowing an
/// untrusted provider error/data field to grow the event stream indefinitely.
fn runtime_error_value(error: &RuntimeControllerError) -> Value {
    match error {
        RuntimeControllerError::Rpc(RpcError::Remote { code, message, .. }) => json!({
            "kind": "remote",
            "code": code,
            "message": bounded_text(message),
        }),
        RuntimeControllerError::Rpc(RpcError::RequestTimeout { method, .. }) => {
            json!({ "kind": "timeout", "operation": method })
        }
        RuntimeControllerError::Rpc(RpcError::InvalidMessage { message }) => json!({
            "kind": "invalid_response",
            "message": bounded_text(message),
        }),
        other => json!({ "kind": "runtime", "message": bounded_text(&other.to_string()) }),
    }
}

fn bounded_text(text: &str) -> String {
    const MAX_BYTES: usize = 4096;
    if text.len() <= MAX_BYTES {
        return text.to_string();
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn clear_mappings(mappings: &Mutex<AcpMappings>) -> Vec<AcpSessionAttachment> {
    let mut mappings = mappings.lock().expect("ACP mappings mutex poisoned");
    let attachments = mappings
        .pedelec_to_provider
        .iter()
        .map(|(pedelec, provider)| AcpSessionAttachment {
            pedelec_thread_id: pedelec.clone(),
            provider_session_id: provider.clone(),
        })
        .collect();
    *mappings = AcpMappings::default();
    attachments
}

fn collect_tool_paths(value: &Value, paths: &mut Vec<PathBuf>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "path" | "cwd" | "filePath") {
                    if let Some(path) = value.as_str() {
                        paths.push(PathBuf::from(path));
                    }
                } else {
                    collect_tool_paths(value, paths);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_tool_paths(value, paths);
            }
        }
        _ => {}
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolves existing symlinks and, for a not-yet-created target, resolves the
/// nearest existing ancestor before restoring the missing suffix.
fn canonicalize_for_policy(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return normalize_path(&canonical);
    }
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while let Some(name) = ancestor.file_name() {
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
        if let Ok(mut canonical) = std::fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return normalize_path(&canonical);
        }
    }
    normalize_path(path)
}

fn path_is_within(path: &Path, workspace: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let workspace = workspace
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        path == workspace
            || path
                .strip_prefix(&workspace)
                .is_some_and(|tail| tail.starts_with('\\'))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(workspace)
    }
}

fn start_error(operation: impl Into<String>, error: impl fmt::Display) -> AcpRuntimeError {
    AcpRuntimeError::RuntimeStart {
        operation: operation.into(),
        message: bounded_text(&error.to_string()),
    }
}

fn protocol_error(operation: impl Into<String>, message: impl Into<String>) -> AcpRuntimeError {
    AcpRuntimeError::Protocol {
        operation: operation.into(),
        message: bounded_text(&message.into()),
    }
}

fn request_error(operation: impl Into<String>, error: RuntimeControllerError) -> AcpRuntimeError {
    let operation = operation.into();
    let message = bounded_text(&error.to_string());
    if is_transport_disconnect(&error) {
        AcpRuntimeError::RuntimeDisconnected { operation, message }
    } else {
        AcpRuntimeError::Request {
            operation,
            message,
            details: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn permission_selection_uses_semantic_kind_not_provider_option_id() {
        let options = vec![
            json!({ "optionId": "persistent", "kind": "allow_always" }),
            json!({ "optionId": "provider-specific-yes", "kind": "allow_once" }),
            json!({ "optionId": "provider-specific-no", "kind": "reject_once" }),
        ];
        assert_eq!(
            find_option(&options, "allow_once"),
            Some(("provider-specific-yes", AcpPermissionDecision::AllowOnce))
        );
        assert_eq!(
            find_option(&options, "reject_once"),
            Some(("provider-specific-no", AcpPermissionDecision::RejectOnce))
        );
    }

    #[test]
    fn malformed_permission_option_ids_are_not_selectable() {
        let options = vec![
            json!({ "optionId": "", "kind": "allow_once" }),
            json!({ "optionId": "   ", "kind": "reject_once" }),
        ];

        assert_eq!(find_option(&options, "allow_once"), None);
        assert_eq!(find_option(&options, "reject_once"), None);
    }

    #[test]
    fn workspace_policy_rejects_unknown_or_outside_operations() {
        let policy = AcpWorkspacePermissionPolicy::new(PathBuf::from("C:/workspace/project"));
        let request = |tool_call| AcpPermissionRequest {
            pedelec_thread_id: "thread".into(),
            provider_session_id: "session".into(),
            tool_call,
            options: json!([]),
        };
        assert_eq!(
            policy.resolve(&request(
                json!({ "path": "C:/workspace/project/src/lib.rs" })
            )),
            AcpPermissionDecision::AllowOnce
        );
        assert_eq!(
            policy.resolve(&request(json!({ "path": "src/lib.rs" }))),
            AcpPermissionDecision::AllowOnce
        );
        assert_eq!(
            policy.resolve(&request(json!({ "path": "../../outside.rs" }))),
            AcpPermissionDecision::RejectOnce
        );
        assert_eq!(
            policy.resolve(&request(json!({ "path": "C:/outside/file.rs" }))),
            AcpPermissionDecision::RejectOnce
        );
        assert_eq!(
            policy.resolve(&request(json!({ "command": "unknown" }))),
            AcpPermissionDecision::RejectOnce
        );
    }

    struct FakeAcpAgent {
        directory: tempfile::TempDir,
        log: PathBuf,
        load_session: bool,
        load_response_session_id: Option<String>,
    }

    impl FakeAcpAgent {
        fn new(load_session: bool) -> Self {
            let directory = tempdir().unwrap();
            Self {
                log: directory.path().join("frames.jsonl"),
                directory,
                load_session,
                load_response_session_id: None,
            }
        }

        fn with_load_response_session_id(mut self, session_id: &str) -> Self {
            self.load_response_session_id = Some(session_id.to_string());
            self
        }

        fn launch(&self) -> AcpLaunchConfig {
            #[cfg(windows)]
            {
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_acp_agent.ps1");
                let config = AcpLaunchConfig::new("fake", "powershell.exe", self.directory.path())
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                    ])
                    .arg(script.as_os_str())
                    .with_env("FAKE_ACP_LOG", self.log.to_string_lossy().into_owned())
                    .with_env(
                        "FAKE_ACP_WORKSPACE",
                        self.directory.path().to_string_lossy().into_owned(),
                    )
                    .with_env(
                        "FAKE_ACP_LOAD",
                        if self.load_session { "true" } else { "false" },
                    );
                let config = config.with_env(
                    "FAKE_ACP_LOAD_SESSION_ID",
                    self.load_response_session_id.clone().unwrap_or_default(),
                );
                return config;
            }
            #[cfg(not(windows))]
            {
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_acp_agent.sh");
                AcpLaunchConfig::new("fake", "sh", self.directory.path())
                    .arg(script.as_os_str())
                    .with_env("FAKE_ACP_LOG", self.log.to_string_lossy().into_owned())
                    .with_env(
                        "FAKE_ACP_WORKSPACE",
                        self.directory.path().to_string_lossy().into_owned(),
                    )
                    .with_env(
                        "FAKE_ACP_LOAD",
                        if self.load_session { "true" } else { "false" },
                    )
                    .with_env(
                        "FAKE_ACP_LOAD_SESSION_ID",
                        self.load_response_session_id.clone().unwrap_or_default(),
                    )
            }
        }

        fn frames(&self) -> Vec<Value> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
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
    fn fake_agent_covers_initialize_new_prompt_updates_permission_and_usage() {
        let fixture = FakeAcpAgent::new(true);
        let workspace = fixture.directory.path().to_path_buf();
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(AcpWorkspacePermissionPolicy::new(&workspace)),
        )
        .unwrap();
        assert!(controller.supports_load());
        let session_id = controller
            .ensure_session("thread-1", None, &AcpSessionConfig::new(&workspace))
            .unwrap();
        assert_eq!(session_id, "acp-session-1");
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap(),
            AcpRuntimeEvent::SessionReady { .. }
        ));
        controller
            .set_config_option(
                &session_id,
                &AcpConfigOptionUpdate {
                    config_id: "provider-model".into(),
                    value: json!("fake/selected"),
                },
            )
            .unwrap();
        controller.set_mode(&session_id, "agent-mode").unwrap();
        controller
            .start_turn("thread-1", &session_id, "local_1", "hello")
            .unwrap();

        let mut deltas = String::new();
        let mut final_message = None;
        let mut permission = None;
        let mut saw_usage = false;
        let mut saw_stderr = false;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(5))
                .unwrap()
            {
                AcpRuntimeEvent::AssistantDelta { text, .. } => deltas.push_str(&text),
                AcpRuntimeEvent::AssistantMessage { text, .. } => final_message = Some(text),
                AcpRuntimeEvent::PermissionResolved {
                    option_id,
                    decision,
                    ..
                } => permission = Some((option_id, decision)),
                AcpRuntimeEvent::UsageUpdated { usage, .. } => {
                    saw_usage = usage["used"] == 4 && usage["size"] == 100
                }
                AcpRuntimeEvent::Stderr { text } => {
                    saw_stderr |= text.contains("fake ACP diagnostic")
                }
                AcpRuntimeEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, AcpTurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(deltas, "hello world");
        assert_eq!(final_message.as_deref(), Some("hello world"));
        assert_eq!(
            permission,
            Some((
                "provider-allow".to_string(),
                AcpPermissionDecision::AllowOnce
            ))
        );
        assert!(saw_usage);
        if !saw_stderr {
            if let Ok(AcpRuntimeEvent::Stderr { text }) =
                controller.recv_event_timeout(Duration::from_secs(1))
            {
                saw_stderr = text.contains("fake ACP diagnostic");
            }
        }
        assert!(saw_stderr);
        assert!(fixture
            .frames()
            .iter()
            .filter(|frame| frame.get("method").is_some())
            .all(|frame| frame["jsonrpc"] == "2.0"));
        let records = protocol_records(&workspace, "thread-1");
        assert_protocol_request_has_response(&records, "session/new");
        assert_protocol_request_has_response(&records, "session/set_config_option");
        assert_protocol_request_has_response(&records, "session/set_mode");
        assert_protocol_request_has_response(&records, "session/prompt");
        assert!(records.iter().any(|record| {
            record["direction"] == "provider_to_client"
                && record["kind"] == "notification"
                && record["message"]["method"] == "session/update"
        }));

        controller
            .start_turn("thread-1", &session_id, "local_2", "cancel me")
            .unwrap();
        controller.cancel_turn(&session_id).unwrap();
        for _ in 0..10 {
            if fixture
                .frames()
                .iter()
                .any(|frame| frame["method"] == "session/cancel")
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(fixture
            .frames()
            .iter()
            .any(|frame| frame["method"] == "session/cancel"));
        controller.shutdown().unwrap();
    }

    #[test]
    fn persisted_session_without_load_capability_fails_without_creating_one() {
        let fixture = FakeAcpAgent::new(false);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let error = controller
            .ensure_session(
                "thread-1",
                Some("persisted-session"),
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(error, AcpRuntimeError::Protocol { .. }));
        assert!(!fixture.frames().iter().any(|frame| {
            matches!(
                frame["method"].as_str(),
                Some("session/new" | "session/load")
            )
        }));
        controller.shutdown().unwrap();
    }

    #[test]
    fn persisted_session_loads_exact_provider_id() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        assert_eq!(
            controller
                .ensure_session(
                    "thread-1",
                    Some("persisted-session"),
                    &AcpSessionConfig::new(fixture.directory.path()),
                )
                .unwrap(),
            "persisted-session"
        );
        assert!(fixture.frames().iter().any(|frame| {
            frame["method"] == "session/load" && frame["params"]["sessionId"] == "persisted-session"
        }));
        controller.shutdown().unwrap();
    }

    #[test]
    fn attached_loaded_session_is_reused_without_reloading() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let config = AcpSessionConfig::new(fixture.directory.path());
        assert_eq!(
            controller
                .ensure_session("thread-1", Some("persisted-session"), &config)
                .unwrap(),
            "persisted-session"
        );
        assert_eq!(
            controller
                .ensure_session("thread-1", Some("persisted-session"), &config)
                .unwrap(),
            "persisted-session"
        );
        assert_eq!(
            fixture
                .frames()
                .iter()
                .filter(|frame| frame["method"] == "session/load")
                .count(),
            1
        );
        assert!(!fixture
            .frames()
            .iter()
            .any(|frame| frame["method"] == "session/new"));
        controller.shutdown().unwrap();
    }

    #[test]
    fn idle_detach_removes_attachment_without_cancelling_or_stopping_shared_runtime() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller.detach_session("thread-1").unwrap();
        assert!(controller.is_healthy());
        assert!(!fixture
            .frames()
            .iter()
            .any(|frame| frame["method"] == "session/cancel"));
        assert!(matches!(
            controller.start_turn("thread-1", &session, "late", "must reject"),
            Err(AcpRuntimeError::Protocol { .. })
        ));
        controller.shutdown().unwrap();
    }

    #[test]
    fn cancel_and_detach_emits_one_cancel_and_one_interrupted_terminal() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller
            .start_turn("thread-1", &session, "cancel-turn", "wait-for-cancel")
            .unwrap();
        assert!(matches!(
            controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap(),
            AcpRuntimeEvent::AssistantDelta { ref text, .. } if text == "before cancel"
        ));
        controller
            .cancel_and_detach_session("thread-1", &session)
            .unwrap();

        let mut interrupted = 0;
        let mut assistant_messages = 0;
        while let Ok(event) = controller.recv_event_timeout(Duration::from_millis(100)) {
            match event {
                AcpRuntimeEvent::AssistantMessage { text, .. } => {
                    assistant_messages += 1;
                    assert_eq!(text, "before cancel");
                }
                AcpRuntimeEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, AcpTurnStatus::Interrupted);
                    interrupted += 1;
                }
                _ => {}
            }
        }
        assert_eq!(assistant_messages, 1);
        assert_eq!(interrupted, 1);
        assert_eq!(
            fixture
                .frames()
                .iter()
                .filter(|frame| frame["method"] == "session/cancel")
                .count(),
            1
        );
        controller.cancel_turn(&session).unwrap();
        assert_eq!(
            fixture
                .frames()
                .iter()
                .filter(|frame| frame["method"] == "session/cancel")
                .count(),
            1
        );
        controller.shutdown().unwrap();
    }

    #[test]
    fn successful_completion_before_cancel_is_not_rewritten_or_cancelled() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller
            .start_turn("thread-1", &session, "complete-first", "empty-success")
            .unwrap();
        loop {
            if let AcpRuntimeEvent::TurnCompleted { status, .. } = controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap()
            {
                assert_eq!(status, AcpTurnStatus::Completed);
                break;
            }
        }
        controller.cancel_turn(&session).unwrap();
        assert!(!fixture
            .frames()
            .iter()
            .any(|frame| frame["method"] == "session/cancel"));
        assert!(controller
            .recv_event_timeout(Duration::from_millis(100))
            .is_err());
        controller.shutdown().unwrap();
    }

    #[test]
    fn empty_success_emits_no_fake_assistant_message_and_one_completion() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller
            .start_turn("thread-1", &session, "empty-turn", "empty-success")
            .unwrap();
        let mut messages = 0;
        let mut completions = 0;
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(2))
                .unwrap()
            {
                AcpRuntimeEvent::AssistantMessage { .. } => messages += 1,
                AcpRuntimeEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, AcpTurnStatus::Completed);
                    completions += 1;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(messages, 0);
        assert_eq!(completions, 1);
        assert!(controller
            .recv_event_timeout(Duration::from_millis(100))
            .is_err());
        controller.shutdown().unwrap();
    }

    #[test]
    fn empty_persisted_session_id_is_rejected_without_rpc_session_creation() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let error = controller
            .ensure_session(
                "thread-1",
                Some("  "),
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(error, AcpRuntimeError::Protocol { .. }));
        assert!(!fixture.frames().iter().any(|frame| {
            matches!(
                frame["method"].as_str(),
                Some("session/new" | "session/load")
            )
        }));
        controller.shutdown().unwrap();
    }

    #[test]
    fn loaded_session_rejects_a_provider_identity_mismatch() {
        let fixture = FakeAcpAgent::new(true).with_load_response_session_id("different-session");
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let error = controller
            .ensure_session(
                "thread-1",
                Some("persisted-session"),
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap_err();
        assert!(matches!(error, AcpRuntimeError::Protocol { .. }));
        assert!(fixture.frames().iter().any(|frame| {
            frame["method"] == "session/load" && frame["params"]["sessionId"] == "persisted-session"
        }));
        controller.shutdown().unwrap();
    }

    #[test]
    fn complete_and_chunked_assistant_updates_share_one_utf8_final_message() {
        let mappings = Arc::new(Mutex::new(AcpMappings::default()));
        mappings
            .lock()
            .unwrap()
            .provider_to_pedelec
            .insert("session".into(), "thread".into());
        mappings
            .lock()
            .unwrap()
            .pedelec_to_provider
            .insert("thread".into(), "session".into());
        mappings.lock().unwrap().pending_turns.insert(
            "session".into(),
            AcpPendingTurn {
                local_turn_id: "local".into(),
                messages: Vec::new(),
                update_revision: 0,
                response_received: false,
                complete_messages: HashSet::new(),
            },
        );
        let (events_tx, events_rx) = mpsc::channel();

        handle_session_update(
            &mappings,
            &events_tx,
            json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "message",
                    "content": {"type": "text", "text": "你"}
                }
            }),
        );
        handle_session_update(
            &mappings,
            &events_tx,
            json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "agent_message",
                    "messageId": "message",
                    "content": {"type": "text", "text": "你好"}
                }
            }),
        );
        handle_session_update(
            &mappings,
            &events_tx,
            json!({
                "sessionId": "session",
                "update": {
                    "sessionUpdate": "tool_call",
                    "content": {"type": "text", "text": "do not render"}
                }
            }),
        );

        let first = events_rx.recv().unwrap();
        assert!(matches!(first, AcpRuntimeEvent::AssistantDelta { text, .. } if text == "你"));
        let second = events_rx.recv().unwrap();
        assert!(matches!(second, AcpRuntimeEvent::AssistantDelta { text, .. } if text == "好"));
        assert!(matches!(
            events_rx.recv().unwrap(),
            AcpRuntimeEvent::Notification { .. }
        ));
        let mappings_guard = mappings.lock().unwrap();
        let turn = mappings_guard.pending_turns.get("session").unwrap();
        assert_eq!(turn.messages, vec![("message".into(), "你好".into())]);
    }

    #[test]
    fn complete_assistant_message_with_a_new_id_does_not_duplicate_anonymous_chunks() {
        let mut turn = AcpPendingTurn {
            local_turn_id: "local".into(),
            messages: Vec::new(),
            update_revision: 0,
            response_received: false,
            complete_messages: HashSet::new(),
        };

        turn.append(None, "hello ");
        assert_eq!(
            turn.set_complete(Some("provider-message"), "hello world"),
            Some("world".into())
        );
        assert_eq!(
            turn.messages,
            vec![("__unidentified__".into(), "hello world".into())]
        );
        assert!(turn.complete_messages.contains("provider-message"));
        assert!(turn.complete_messages.contains("__unidentified__"));
    }

    #[test]
    fn denied_permission_selects_the_provider_reject_once_option() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller
            .start_turn("thread-1", &session, "local_1", "deny")
            .unwrap();
        loop {
            match controller
                .recv_event_timeout(Duration::from_secs(5))
                .unwrap()
            {
                AcpRuntimeEvent::PermissionResolved {
                    option_id,
                    decision,
                    ..
                } => {
                    assert_eq!(option_id, "provider-reject");
                    assert_eq!(decision, AcpPermissionDecision::RejectOnce);
                    break;
                }
                _ => {}
            }
        }
        controller.shutdown().unwrap();
    }

    #[test]
    fn concurrent_sessions_share_process_without_cross_routing_turns() {
        let fixture = FakeAcpAgent::new(true);
        let workspace = fixture.directory.path().to_path_buf();
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(AcpWorkspacePermissionPolicy::new(&workspace)),
        )
        .unwrap();
        let first = controller
            .ensure_session("thread-1", None, &AcpSessionConfig::new(&workspace))
            .unwrap();
        let second = controller
            .ensure_session("thread-2", None, &AcpSessionConfig::new(&workspace))
            .unwrap();
        assert_ne!(first, second);
        for _ in 0..2 {
            assert!(matches!(
                controller
                    .recv_event_timeout(Duration::from_secs(2))
                    .unwrap(),
                AcpRuntimeEvent::SessionReady { .. }
            ));
        }
        controller
            .start_turn("thread-1", &first, "local_1", "concurrent-first")
            .unwrap();
        controller
            .start_turn("thread-2", &second, "local_2", "concurrent-second")
            .unwrap();

        let mut completed = HashMap::new();
        while completed.len() < 2 {
            match controller
                .recv_event_timeout(Duration::from_secs(5))
                .unwrap()
            {
                AcpRuntimeEvent::AssistantDelta {
                    pedelec_thread_id,
                    provider_session_id,
                    local_turn_id,
                    ..
                } => {
                    if pedelec_thread_id == "thread-1" {
                        assert_eq!(provider_session_id, first);
                        assert_eq!(local_turn_id, "local_1");
                    } else {
                        assert_eq!(pedelec_thread_id, "thread-2");
                        assert_eq!(provider_session_id, second);
                        assert_eq!(local_turn_id, "local_2");
                    }
                }
                AcpRuntimeEvent::TurnCompleted {
                    pedelec_thread_id,
                    local_turn_id,
                    ..
                } => {
                    completed.insert(pedelec_thread_id, local_turn_id);
                }
                _ => {}
            }
        }
        assert_eq!(
            completed.get("thread-1").map(String::as_str),
            Some("local_1")
        );
        assert_eq!(
            completed.get("thread-2").map(String::as_str),
            Some("local_2")
        );
        let first_records = protocol_records(&workspace, "thread-1");
        let second_records = protocol_records(&workspace, "thread-2");
        assert_protocol_request_has_response(&first_records, "session/prompt");
        assert_protocol_request_has_response(&second_records, "session/prompt");
        let first_log = first_records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let second_log = second_records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first_log.contains("concurrent-first"));
        assert!(!first_log.contains("concurrent-second"));
        assert!(second_log.contains("concurrent-second"));
        assert!(!second_log.contains("concurrent-first"));
        controller.shutdown().unwrap();
    }

    #[test]
    fn malformed_agent_frame_disconnects_with_current_attachments() {
        let fixture = FakeAcpAgent::new(true);
        let controller = AcpController::spawn(
            fixture.launch(),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();
        let session = controller
            .ensure_session(
                "thread-1",
                None,
                &AcpSessionConfig::new(fixture.directory.path()),
            )
            .unwrap();
        let _ = controller.recv_event_timeout(Duration::from_secs(2));
        controller
            .start_turn("thread-1", &session, "local_1", "malformed")
            .unwrap();
        loop {
            if let AcpRuntimeEvent::Disconnected {
                attachments,
                reason,
                ..
            } = controller
                .recv_event_timeout(Duration::from_secs(5))
                .unwrap()
            {
                assert!(matches!(reason, RpcDisconnectReason::MalformedFrame(_)));
                assert_eq!(
                    attachments,
                    vec![AcpSessionAttachment {
                        pedelec_thread_id: "thread-1".to_string(),
                        provider_session_id: session,
                    }]
                );
                break;
            }
        }
        assert!(!controller.is_healthy());
        let mappings = controller.mappings.lock().unwrap();
        assert!(mappings.pedelec_to_provider.is_empty());
        assert!(mappings.provider_to_pedelec.is_empty());
        assert!(mappings.pending_turns.is_empty());
    }
}
