mod cli;
mod protocol;
mod registry;

pub use cli::{parse_cli, CliAction, ServeCommand};
pub use protocol::{PROTOCOL_VERSION, SERVER_NAME};

use super::backend::{InferenceBackend, ModelCapabilities};
use super::config::{AgentSessionConfig, PedelecAgentServerConfig};
use super::error::AgentError;
use super::events::{AgentTurnSink, TurnToolResult};
use super::ollama::OllamaBackend;
use super::sandbox::canonicalize_workspace_path;
use super::session::AgentSession;
use protocol::{
    assistant_delta_params, assistant_message_params, completed_failed_params,
    completed_success_params, decode_params, invalid_params, method_not_found, notification,
    parse_request_line, rpc_error, rpc_result, tool_call_params, tool_result_params, turn_identity,
    usage_params, InitializeParams, JsonRpcRequest, ProtocolWriter, SessionCloseParams,
    SessionOpenParams, TurnStartParams,
};
use registry::{
    AttachmentState, RegisterError, RegistryLookup, SessionAttachment, SessionRegistry,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct ModelCapabilityCache {
    inner: Mutex<HashMap<String, ModelCapabilities>>,
}

impl ModelCapabilityCache {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn inspect(
        &self,
        backend: &dyn InferenceBackend,
        model: &str,
    ) -> Result<ModelCapabilities, AgentError> {
        if let Some(cached) = lock_mutex(&self.inner).get(model).cloned() {
            return Ok(cached);
        }
        let capabilities = backend.inspect_model(model)?;
        lock_mutex(&self.inner).insert(model.to_string(), capabilities);
        Ok(capabilities)
    }
}

pub struct PedelecAgentServer {
    config: PedelecAgentServerConfig,
    backend: Arc<dyn InferenceBackend>,
    capabilities: ModelCapabilityCache,
    registry: SessionRegistry,
    writer: ProtocolWriter,
    initialized: Mutex<bool>,
    shutting_down: AtomicBool,
    fatal: Arc<AtomicBool>,
    exit_process_on_shutdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeOutcome {
    Shutdown,
    StdinClosed,
    Fatal,
}

pub struct ServeOptions {
    pub exit_process_on_shutdown: bool,
    pub exit_process_on_writer_fatal: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            exit_process_on_shutdown: true,
            exit_process_on_writer_fatal: true,
        }
    }
}

impl PedelecAgentServer {
    pub fn new(
        config: PedelecAgentServerConfig,
        backend: Arc<dyn InferenceBackend>,
        stdout: Box<dyn Write + Send>,
        options: ServeOptions,
    ) -> Arc<Self> {
        let fatal = Arc::new(AtomicBool::new(false));
        let exit_on_writer = options.exit_process_on_writer_fatal;
        let on_fatal = {
            let fatal = Arc::clone(&fatal);
            Arc::new(move || {
                fatal.store(true, Ordering::SeqCst);
                if exit_on_writer {
                    std::process::exit(1);
                }
            }) as Arc<dyn Fn() + Send + Sync>
        };
        Arc::new(Self {
            config,
            backend,
            capabilities: ModelCapabilityCache::new(),
            registry: SessionRegistry::new(),
            writer: ProtocolWriter::new(stdout, on_fatal),
            initialized: Mutex::new(false),
            shutting_down: AtomicBool::new(false),
            fatal,
            exit_process_on_shutdown: options.exit_process_on_shutdown,
        })
    }

    pub fn serve(self: &Arc<Self>, reader: impl BufRead) -> ServeOutcome {
        for line in reader.lines() {
            if self.fatal.load(Ordering::SeqCst) {
                return ServeOutcome::Fatal;
            }
            if self.shutting_down.load(Ordering::SeqCst) {
                return ServeOutcome::Shutdown;
            }
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    eprintln!("pedelec-agent stdin read failed: {err}");
                    return ServeOutcome::Fatal;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let server = Arc::clone(self);
            let _ = thread::Builder::new()
                .name("pedelec-agent-rpc".into())
                .spawn(move || server.handle_line(&line));
        }
        if self.fatal.load(Ordering::SeqCst) {
            ServeOutcome::Fatal
        } else if self.shutting_down.load(Ordering::SeqCst) {
            ServeOutcome::Shutdown
        } else {
            ServeOutcome::StdinClosed
        }
    }

    fn handle_line(self: &Arc<Self>, line: &str) {
        match parse_request_line(line) {
            Ok(request) => self.dispatch(request),
            Err(frame) => self.writer.write_value(&frame),
        }
    }

    fn dispatch(self: &Arc<Self>, request: JsonRpcRequest) {
        if self.fatal.load(Ordering::SeqCst) {
            return;
        }
        let method = request.method.as_str();
        if method != "initialize" && method != "shutdown" && !*lock_mutex(&self.initialized) {
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::new("NOT_INITIALIZED", "Server has not been initialized."),
            ));
            return;
        }
        if method != "shutdown" && self.shutting_down.load(Ordering::SeqCst) {
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::new("SERVER_SHUTTING_DOWN", "Server is shutting down."),
            ));
            return;
        }
        match method {
            "initialize" => self.handle_initialize(request),
            "session/open" => self.handle_session_open(request),
            "turn/start" => self.handle_turn_start(request),
            "session/close" => self.handle_session_close(request),
            "shutdown" => self.handle_shutdown(request),
            other => self
                .writer
                .write_value(&method_not_found(request.id, other)),
        }
    }

    fn handle_initialize(&self, request: JsonRpcRequest) {
        let params: InitializeParams = match decode_params(&request.id, request.params) {
            Ok(params) => params,
            Err(frame) => {
                self.writer.write_value(&frame);
                return;
            }
        };
        if params.protocol_version != PROTOCOL_VERSION {
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::with_details(
                    "PROTOCOL_VERSION_UNSUPPORTED",
                    "Unsupported protocol version.",
                    json!({
                        "protocolVersion": params.protocol_version,
                        "supported": PROTOCOL_VERSION
                    }),
                ),
            ));
            return;
        }
        let _ = params.client_info;
        *lock_mutex(&self.initialized) = true;
        self.writer.write_value(&rpc_result(
            request.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                },
                "provider": self.config.provider.as_str(),
                "capabilities": {
                    "multipleSessions": true,
                    "assistantDelta": true,
                    "usage": true
                }
            }),
        ));
    }

    fn handle_session_open(&self, request: JsonRpcRequest) {
        let params: SessionOpenParams = match decode_params(&request.id, request.params) {
            Ok(params) => params,
            Err(frame) => {
                self.writer.write_value(&frame);
                return;
            }
        };
        if let Err(error) = validate_session_open(&params) {
            self.writer.write_value(&rpc_error(request.id, error));
            return;
        }
        let requested_session_id = normalize_optional_id(params.session_id.clone());
        let (thread_lock, session_lock) = self
            .registry
            .lock_open(&params.thread_id, requested_session_id.as_deref());
        let _thread_guard = thread_lock.lock();
        let _session_guard = session_lock.as_ref().map(|lock| lock.lock());

        if self.shutting_down.load(Ordering::SeqCst) {
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::new("SERVER_SHUTTING_DOWN", "Server is shutting down."),
            ));
            return;
        }

        if let Some(existing) = self.registry.get_by_thread(&params.thread_id) {
            match requested_session_id.as_deref() {
                Some(session_id) if session_id == existing.session_id => {
                    self.write_already_attached(request.id, &existing, &params);
                    return;
                }
                _ => {
                    self.writer.write_value(&rpc_error(
                        request.id,
                        attachment_conflict(
                            &params.thread_id,
                            &existing.session_id,
                            requested_session_id.as_deref().unwrap_or("<new>"),
                        ),
                    ));
                    return;
                }
            }
        }
        if let Some(session_id) = requested_session_id.as_deref() {
            if let Some(existing) = self.registry.get_by_session(session_id) {
                self.writer.write_value(&rpc_error(
                    request.id,
                    AgentError::with_details(
                        "ATTACHMENT_CONFLICT",
                        "Session is already attached to another thread.",
                        json!({
                            "sessionId": session_id,
                            "attachedThreadId": existing.thread_id,
                            "requestedThreadId": params.thread_id
                        }),
                    ),
                ));
                return;
            }
        }

        let capabilities = match self
            .capabilities
            .inspect(self.backend.as_ref(), &params.model)
        {
            Ok(capabilities) => capabilities,
            Err(error) => {
                self.writer.write_value(&rpc_error(request.id, error));
                return;
            }
        };
        if !capabilities.tools {
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::with_details(
                    "MODEL_TOOLS_UNSUPPORTED",
                    "The selected model does not support tool calling.",
                    json!({ "model": params.model, "capabilities": capabilities }),
                ),
            ));
            return;
        }

        let session = match AgentSession::open(
            Arc::clone(&self.backend),
            &self.config,
            AgentSessionConfig {
                requested_session_id: requested_session_id.clone(),
                model: params.model.clone(),
                workspace_path: PathBuf::from(&params.workspace_path),
                host_instructions: params.host_instructions.clone(),
            },
        ) {
            Ok(session) => session,
            Err(error) => {
                self.writer.write_value(&rpc_error(request.id, error));
                return;
            }
        };
        let resumed = session.resumed();
        let session_id = session.session_id().to_string();
        let capabilities = session.capabilities();
        let attachment = SessionAttachment::new(params.thread_id.clone(), session);
        match self.registry.register(Arc::clone(&attachment)) {
            Ok(_) => self.writer.write_value(&rpc_result(
                request.id,
                json!({
                    "sessionId": session_id,
                    "resumed": resumed,
                    "alreadyAttached": false,
                    "modelCapabilities": capabilities
                }),
            )),
            Err(error) => self
                .writer
                .write_value(&rpc_error(request.id, register_error(error))),
        }
    }

    fn write_already_attached(
        &self,
        id: Value,
        attachment: &SessionAttachment,
        params: &SessionOpenParams,
    ) {
        if let Err(error) = validate_attached_identity(attachment, params) {
            self.writer.write_value(&rpc_error(id, error));
            return;
        }
        if let Err(error) = refresh_host_instructions(attachment, params.host_instructions.clone())
        {
            self.writer.write_value(&rpc_error(id, error));
            return;
        }
        self.writer.write_value(&rpc_result(
            id,
            json!({
                "sessionId": attachment.session_id,
                "resumed": attachment.resumed,
                "alreadyAttached": true,
                "modelCapabilities": attachment.capabilities
            }),
        ));
    }

    fn handle_turn_start(self: &Arc<Self>, request: JsonRpcRequest) {
        let params: TurnStartParams = match decode_params(&request.id, request.params) {
            Ok(params) => params,
            Err(frame) => {
                self.writer.write_value(&frame);
                return;
            }
        };
        if let Err(error) = validate_turn_start(&params) {
            self.writer.write_value(&rpc_error(request.id, error));
            return;
        }
        let attachment = match self.lookup_attachment(&params.thread_id, &params.session_id) {
            Ok(attachment) => attachment,
            Err(error) => {
                self.writer.write_value(&rpc_error(request.id, error));
                return;
            }
        };

        let generation = attachment.current_generation();
        {
            let mut control = lock_mutex(&attachment.control);
            if control.committed_turn_ids.contains(&params.turn_id) {
                self.writer.write_value(&rpc_error(
                    request.id,
                    AgentError::with_details(
                        "TURN_ALREADY_COMPLETED",
                        "This turn was already committed and cannot be replayed.",
                        json!({ "turnId": params.turn_id }),
                    ),
                ));
                return;
            }
            match &control.state {
                AttachmentState::Closed => {
                    self.writer.write_value(&rpc_error(
                        request.id,
                        AgentError::new("ATTACHMENT_NOT_FOUND", "Session is not attached."),
                    ));
                    return;
                }
                AttachmentState::Running { turn_id } if turn_id == &params.turn_id => {
                    drop(control);
                    self.writer.write_value(&rpc_result(
                        request.id,
                        json!({
                            "turnId": params.turn_id,
                            "accepted": true,
                            "alreadyStarted": true
                        }),
                    ));
                    return;
                }
                AttachmentState::Running { turn_id } => {
                    let busy = AgentError::with_details(
                        "SESSION_BUSY",
                        "Session already has an active turn.",
                        json!({ "turnId": turn_id }),
                    );
                    drop(control);
                    self.writer.write_value(&rpc_error(request.id, busy));
                    return;
                }
                AttachmentState::Ready => {}
            }
            control.state = AttachmentState::Running {
                turn_id: params.turn_id.clone(),
            };
        }

        let response = rpc_result(
            request.id.clone(),
            json!({
                "turnId": params.turn_id,
                "accepted": true,
                "alreadyStarted": false
            }),
        );
        let started = notification(
            "turn/started",
            turn_identity(&params.thread_id, &params.session_id, &params.turn_id),
        );
        if !attachment.emit_all(&self.writer, generation, &[response, started]) {
            unreserve(&attachment, &params.turn_id);
            self.writer.write_value(&rpc_error(
                request.id,
                AgentError::new("SESSION_CLOSED", "Session is closed."),
            ));
            return;
        }

        let server = Arc::clone(self);
        let _ = thread::Builder::new()
            .name(format!("pedelec-agent-turn-{}", params.turn_id))
            .spawn(move || server.run_turn_worker(attachment, params, generation));
    }

    fn run_turn_worker(
        &self,
        attachment: Arc<SessionAttachment>,
        params: TurnStartParams,
        generation: u64,
    ) {
        let mut sink = ProtocolTurnSink {
            writer: &self.writer,
            attachment: &attachment,
            generation,
            thread_id: &params.thread_id,
            session_id: &params.session_id,
            turn_id: &params.turn_id,
        };
        let result = {
            let mut session = lock_mutex(&attachment.session);
            session.run_turn(&params.turn_id, &params.message, &mut sink)
        };
        if !attachment.gate.is_live(generation) {
            finish_turn(&attachment, &params.turn_id);
            return;
        }
        match result {
            Ok(result) => {
                {
                    let mut control = lock_mutex(&attachment.control);
                    control.committed_turn_ids.insert(params.turn_id.clone());
                    if matches!(&control.state, AttachmentState::Running { turn_id } if turn_id == &params.turn_id)
                    {
                        control.state = AttachmentState::Ready;
                    }
                }
                attachment.emit(
                    &self.writer,
                    generation,
                    notification(
                        "turn/assistant_message",
                        assistant_message_params(
                            &params.thread_id,
                            &params.session_id,
                            &params.turn_id,
                            &result.text,
                        ),
                    ),
                );
                attachment.emit(
                    &self.writer,
                    generation,
                    notification(
                        "turn/completed",
                        completed_success_params(
                            &params.thread_id,
                            &params.session_id,
                            &params.turn_id,
                        ),
                    ),
                );
            }
            Err(error) => {
                attachment.emit(
                    &self.writer,
                    generation,
                    notification(
                        "turn/completed",
                        completed_failed_params(
                            &params.thread_id,
                            &params.session_id,
                            &params.turn_id,
                            &error,
                        ),
                    ),
                );
                finish_turn(&attachment, &params.turn_id);
            }
        }
    }

    fn handle_session_close(&self, request: JsonRpcRequest) {
        let params: SessionCloseParams = match decode_params(&request.id, request.params) {
            Ok(params) => params,
            Err(frame) => {
                self.writer.write_value(&frame);
                return;
            }
        };
        if params.thread_id.trim().is_empty() || params.session_id.trim().is_empty() {
            self.writer.write_value(&invalid_params(
                request.id,
                "threadId and sessionId are required.",
            ));
            return;
        }
        let (thread_lock, session_lock) = self
            .registry
            .lock_open(&params.thread_id, Some(params.session_id.as_str()));
        let _thread_guard = thread_lock.lock();
        let _session_guard = session_lock.as_ref().map(|lock| lock.lock());
        let attachment = match self
            .registry
            .resolve_close(&params.thread_id, &params.session_id)
        {
            Ok(Some(attachment)) => attachment,
            Ok(None) => {
                self.writer
                    .write_value(&rpc_result(request.id, json!({ "closed": true })));
                return;
            }
            Err(error) => {
                self.writer
                    .write_value(&rpc_error(request.id, lookup_error(error)));
                return;
            }
        };
        let _ = params.active_turn_id;
        attachment.close_lifecycle();
        attachment.invalidate();
        self.registry.detach(&attachment);
        self.writer
            .write_value(&rpc_result(request.id, json!({ "closed": true })));
    }

    fn handle_shutdown(&self, request: JsonRpcRequest) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for attachment in self.registry.snapshot() {
            attachment.close_lifecycle();
            attachment.invalidate();
            self.registry.detach(&attachment);
        }
        self.writer
            .write_value(&rpc_result(request.id, json!({ "ok": true })));
        if self.exit_process_on_shutdown {
            std::process::exit(0);
        }
    }

    fn lookup_attachment(
        &self,
        thread_id: &str,
        session_id: &str,
    ) -> Result<Arc<SessionAttachment>, AgentError> {
        match self.registry.lookup(thread_id, session_id) {
            Ok(attachment) => Ok(attachment),
            Err(error) => Err(lookup_error(error)),
        }
    }

    #[cfg(test)]
    fn set_commit_admission_hook(&self, thread_id: &str, hook: Arc<dyn Fn() + Send + Sync>) {
        let attachment = self
            .registry
            .get_by_thread(thread_id)
            .expect("attachment must exist before installing a commit hook");
        attachment.gate.set_commit_admission_hook(Some(hook));
    }

    #[cfg(test)]
    fn set_tool_start_hook(&self, thread_id: &str, hook: Arc<dyn Fn() + Send + Sync>) {
        let attachment = self
            .registry
            .get_by_thread(thread_id)
            .expect("attachment must exist before installing a tool hook");
        lock_mutex(&attachment.session).set_tool_start_hook(Some(hook));
    }

    #[cfg(test)]
    fn inflight_lock_counts(&self) -> (usize, usize) {
        self.registry.inflight_lock_counts()
    }
}

struct ProtocolTurnSink<'a> {
    writer: &'a ProtocolWriter,
    attachment: &'a SessionAttachment,
    generation: u64,
    thread_id: &'a str,
    session_id: &'a str,
    turn_id: &'a str,
}

impl AgentTurnSink for ProtocolTurnSink<'_> {
    fn assistant_delta(&mut self, text: &str) {
        self.attachment.emit(
            self.writer,
            self.generation,
            notification(
                "turn/assistant_delta",
                assistant_delta_params(self.thread_id, self.session_id, self.turn_id, text),
            ),
        );
    }

    fn usage_updated(&mut self, usage: &super::backend::InferenceUsage) {
        self.attachment.emit(
            self.writer,
            self.generation,
            notification(
                "turn/usage",
                usage_params(self.thread_id, self.session_id, self.turn_id, usage),
            ),
        );
    }

    fn tool_call(&mut self, call: &super::conversation::NormalizedToolCall) {
        self.attachment.emit(
            self.writer,
            self.generation,
            notification(
                "turn/tool_call",
                tool_call_params(self.thread_id, self.session_id, self.turn_id, call),
            ),
        );
    }

    fn tool_result(&mut self, result: &TurnToolResult) {
        self.attachment.emit(
            self.writer,
            self.generation,
            notification(
                "turn/tool_result",
                tool_result_params(self.thread_id, self.session_id, self.turn_id, result),
            ),
        );
    }
}

pub fn serve_stdio(config: PedelecAgentServerConfig) -> Result<ServeOutcome, AgentError> {
    let backend = Arc::new(OllamaBackend::new(&config)?);
    let server = PedelecAgentServer::new(
        config,
        backend,
        Box::new(std::io::stdout()),
        ServeOptions::default(),
    );
    Ok(server.serve(std::io::BufReader::new(std::io::stdin())))
}

fn validate_attached_identity(
    attachment: &SessionAttachment,
    params: &SessionOpenParams,
) -> Result<(), AgentError> {
    if attachment.model != params.model {
        return Err(identity_conflict("model", &attachment.model, &params.model));
    }
    let requested_workspace = requested_workspace_identity(&params.workspace_path)?;
    if attachment.workspace_path != requested_workspace {
        return Err(identity_conflict(
            "workspacePath",
            &attachment.workspace_path.to_string_lossy(),
            &requested_workspace.to_string_lossy(),
        ));
    }
    Ok(())
}

fn requested_workspace_identity(workspace_path: &str) -> Result<PathBuf, AgentError> {
    canonicalize_workspace_path(Path::new(workspace_path))
}

fn refresh_host_instructions(
    attachment: &SessionAttachment,
    host_instructions: Option<String>,
) -> Result<(), AgentError> {
    if attachment.current_host_instructions() == host_instructions {
        return Ok(());
    }
    {
        let control = lock_mutex(&attachment.control);
        match &control.state {
            AttachmentState::Closed => {
                return Err(AgentError::new("SESSION_CLOSED", "Session is closed."));
            }
            AttachmentState::Running { turn_id } => {
                return Err(AgentError::with_details(
                    "SESSION_BUSY",
                    "Session already has an active turn.",
                    json!({ "turnId": turn_id }),
                ));
            }
            AttachmentState::Ready => {}
        }
    }
    let mut session = attachment.session.try_lock().map_err(|_| {
        AgentError::new(
            "SESSION_BUSY",
            "Session is busy and cannot refresh host instructions.",
        )
    })?;
    if session.is_closed() {
        return Err(AgentError::new("SESSION_CLOSED", "Session is closed."));
    }
    session.set_host_instructions(host_instructions.clone());
    drop(session);
    attachment.set_cached_host_instructions(host_instructions);
    Ok(())
}

fn identity_conflict(field: &str, existing: &str, requested: &str) -> AgentError {
    AgentError::with_details(
        "INVALID_ARGUMENT",
        "Session resume argument conflicts with existing session",
        json!({ "field": field, "existing": existing, "requested": requested }),
    )
}

fn validate_session_open(params: &SessionOpenParams) -> Result<(), AgentError> {
    if params.thread_id.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "threadId is required."));
    }
    if params.model.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "model is required."));
    }
    if params.workspace_path.trim().is_empty() {
        return Err(AgentError::new(
            "INVALID_PARAMS",
            "workspacePath is required.",
        ));
    }
    Ok(())
}

fn validate_turn_start(params: &TurnStartParams) -> Result<(), AgentError> {
    if params.thread_id.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "threadId is required."));
    }
    if params.session_id.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "sessionId is required."));
    }
    if params.turn_id.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "turnId is required."));
    }
    if params.message.trim().is_empty() {
        return Err(AgentError::new("INVALID_PARAMS", "message is required."));
    }
    Ok(())
}

fn normalize_optional_id(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn attachment_conflict(thread_id: &str, attached: &str, requested: &str) -> AgentError {
    AgentError::with_details(
        "ATTACHMENT_CONFLICT",
        "Thread is already attached to a different session.",
        json!({
            "threadId": thread_id,
            "attachedSessionId": attached,
            "requestedSessionId": requested
        }),
    )
}

fn register_error(error: RegisterError) -> AgentError {
    match error {
        RegisterError::ThreadBound {
            thread_id,
            attached_session_id,
        } => attachment_conflict(&thread_id, &attached_session_id, "<new>"),
        RegisterError::SessionBound {
            session_id,
            attached_thread_id,
        } => AgentError::with_details(
            "ATTACHMENT_CONFLICT",
            "Session is already attached to another thread.",
            json!({
                "sessionId": session_id,
                "attachedThreadId": attached_thread_id
            }),
        ),
    }
}

fn lookup_error(error: RegistryLookup) -> AgentError {
    match error {
        RegistryLookup::Missing => {
            AgentError::new("ATTACHMENT_NOT_FOUND", "Session is not attached.")
        }
        RegistryLookup::Conflict {
            thread_id,
            attached_session_id,
            requested_session_id,
        } => attachment_conflict(&thread_id, &attached_session_id, &requested_session_id),
    }
}

fn unreserve(attachment: &SessionAttachment, turn_id: &str) {
    finish_turn(attachment, turn_id);
}

fn finish_turn(attachment: &SessionAttachment, turn_id: &str) {
    let mut control = lock_mutex(&attachment.control);
    if matches!(&control.state, AttachmentState::Running { turn_id: current } if current == turn_id)
    {
        control.state = AttachmentState::Ready;
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests;
