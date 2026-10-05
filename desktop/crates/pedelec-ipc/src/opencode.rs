use crate::{record_protocol_traffic, PersistentRuntimeDispatcher};
use pedelec_core::{
    build_persistent_user_prompt_with_bootstrap, error_codes, PedelecError,
    PersistentProviderSessionIntent, PersistentRuntimeOperation, ProviderArtifactInput,
    ProviderArtifactKind, ProviderArtifactPayload, ProviderArtifactSource, ProviderCode,
    ProviderRuntimeDiagnostic, ProviderRuntimeEvent, SharedCoreRuntime,
};
use pedelec_runtime::{
    AcpArtifactSource, AcpAuthentication, AcpConfigOptionUpdate, AcpController,
    AcpExtensionRequestHandler, AcpPermissionDecision, AcpPermissionRequest, AcpRuntimeError,
    AcpRuntimeEvent, AcpSessionConfig, AcpTurnStatus, AcpWorkspacePermissionPolicy,
    ProviderRuntimeController, ProviderRuntimeOwner, RpcServerRequest, RuntimeRegistryError,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;

pub const OPENCODE_RUNTIME_KEY: &str = "opencode-acp";
pub const CURSOR_RUNTIME_KEY: &str = "cursor-acp";
const OPENCODE_INSTRUCTION_PATH: &str = ".pedelec-runtime/opencode-session-instructions.md";
const OPENCODE_CONFIG_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const OPENCODE_PERMISSION: &str = "OPENCODE_PERMISSION";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpProviderKind {
    OpenCode,
    Cursor,
}

impl AcpProviderKind {
    fn provider_code(self) -> ProviderCode {
        match self {
            Self::OpenCode => ProviderCode::OpenCode,
            Self::Cursor => ProviderCode::Cursor,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
        }
    }

    fn runtime_key(self) -> &'static str {
        match self {
            Self::OpenCode => OPENCODE_RUNTIME_KEY,
            Self::Cursor => CURSOR_RUNTIME_KEY,
        }
    }

    fn is_cursor(self) -> bool {
        matches!(self, Self::Cursor)
    }
}

#[derive(Debug, Clone)]
pub struct AcpRuntimeDispatcher {
    provider: AcpProviderKind,
    owner: ProviderRuntimeOwner,
    core_runtime: SharedCoreRuntime,
    program_override: Option<PathBuf>,
    process_cwd: PathBuf,
    typed_controller: Arc<Mutex<Option<Arc<AcpController>>>>,
    selection: Option<pedelec_core::ProviderRuntimeSelection>,
    registry_key: Option<String>,
    router: Arc<crate::selection::SelectionRouter<AcpRuntimeDispatcher>>,
    members: Arc<Mutex<HashSet<String>>>,
    workspaces: Arc<Mutex<HashMap<String, PathBuf>>>,
    event_pumps: Arc<Mutex<HashSet<u64>>>,
    traffic_pumps: Arc<Mutex<HashSet<u64>>>,
    stopped_generations: Arc<Mutex<HashSet<u64>>>,
    launch_env: Vec<(String, String)>,
}

impl AcpRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self::new_for_provider(AcpProviderKind::OpenCode, owner, core_runtime)
    }

    pub fn new_for_provider(
        provider: AcpProviderKind,
        owner: ProviderRuntimeOwner,
        core_runtime: SharedCoreRuntime,
    ) -> Self {
        Self {
            provider,
            owner,
            core_runtime,
            program_override: None,
            process_cwd: env::temp_dir(),
            typed_controller: Arc::new(Mutex::new(None)),
            selection: None,
            registry_key: None,
            router: Arc::new(crate::selection::SelectionRouter::default()),
            members: Arc::new(Mutex::new(HashSet::new())),
            workspaces: Arc::new(Mutex::new(HashMap::new())),
            event_pumps: Arc::new(Mutex::new(HashSet::new())),
            traffic_pumps: Arc::new(Mutex::new(HashSet::new())),
            stopped_generations: Arc::new(Mutex::new(HashSet::new())),
            launch_env: Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn with_program_for_test(mut self, program: impl Into<PathBuf>) -> Self {
        self.program_override = Some(program.into());
        self
    }

    #[doc(hidden)]
    pub fn with_process_cwd_for_test(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.process_cwd = cwd.into();
        self
    }

    #[doc(hidden)]
    pub fn with_env_for_test(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.launch_env.push((key.into(), value.into()));
        self
    }

    fn controller_for(
        &self,
        session: &PersistentProviderSessionIntent,
        provider_turn_id: Option<&str>,
    ) -> Result<Arc<AcpController>, PedelecError> {
        self.fail_unhealthy_controller(Some(&session.thread_id));
        self.workspaces
            .lock()
            .map_err(|_| mutex_error(&format!("{} workspace policy", self.provider.label())))?
            .entry(session.thread_id.clone())
            .or_insert_with(|| session.workspace_path.clone());

        // This child owns one captured selection. The outer router chooses
        // the child before this reuse check, including for newly admitted threads.
        if let Some(controller) = self
            .typed_controller
            .lock()
            .map_err(|_| mutex_error(&format!("{} controller", self.provider.label())))?
            .clone()
            .filter(|controller| controller.is_healthy())
        {
            return Ok(controller);
        }

        let program_override = self.program_override.clone();
        let runtime = Arc::clone(&self.core_runtime);
        let process_cwd = self.process_cwd.clone();
        let runtime_file = session.core_ipc_runtime_file_path.clone();
        let typed = Arc::clone(&self.typed_controller);
        let workspaces = Arc::clone(&self.workspaces);
        let launch_env = self.launch_env.clone();
        let provider = self.provider;
        let cursor_pre_authenticated = provider.is_cursor() && cursor_auth_available(&launch_env);
        let program = match program_override {
            Some(program) => program,
            None => match runtime
                .lock()
                .map_err(|_| mutex_error(&format!("{} runtime", provider.label())))
                .and_then(|runtime| runtime.provider_executable_path(&provider.provider_code()))
            {
                Ok(program) => program,
                Err(error) => {
                    return Err(runtime_context_error(
                        error,
                        provider,
                        session,
                        provider_turn_id,
                        "startup",
                    ));
                }
            },
        };
        self.owner
            .get_or_init(
                self.registry_key
                    .clone()
                    .unwrap_or_else(|| provider.runtime_key().to_string()),
                move || {
                    let mut launch = pedelec_runtime::AcpLaunchConfig::new(
                        provider.label(),
                        program,
                        process_cwd,
                    )
                    .arg("acp")
                    .with_env("PEDELEC_PROVIDER", provider.code())
                    .with_env(
                        "PEDELEC_CORE_IPC_RUNTIME_FILE",
                        runtime_file.to_string_lossy().into_owned(),
                    );
                    if provider == AcpProviderKind::OpenCode {
                        let config_content = opencode_acp_config_content()
                            .map_err(|error| RuntimeRegistryError::Initialization(error.message))?;
                        launch = launch
                            .with_env(OPENCODE_CONFIG_CONTENT, config_content)
                            .with_env(OPENCODE_PERMISSION, opencode_permission_overlay());
                    } else {
                        launch = launch.with_client_capabilities(json!({
                            "_meta": { "parameterizedModelPicker": true }
                        }));
                        if !cursor_pre_authenticated {
                            launch = launch.with_authentication(AcpAuthentication::MethodId(
                                "cursor_login".to_string(),
                            ));
                        }
                    }
                    let path = pedelec_shared::paths::path_value_with_default_pedelec_dir()
                        .map_err(|error| RuntimeRegistryError::Initialization(error.message))?;
                    launch = launch.with_env("PATH", path);
                    for (key, value) in launch_env {
                        launch = launch.with_env(key, value);
                    }
                    let resolver = Arc::new(move |request: &AcpPermissionRequest| {
                        resolve_workspace_permission(&workspaces, request)
                    });
                    let extension_handler = provider.is_cursor().then(|| {
                        Arc::new(CursorExtensionRequestHandler)
                            as Arc<dyn AcpExtensionRequestHandler>
                    });
                    let controller = AcpController::spawn_with_extension_handler(
                        launch,
                        resolver,
                        extension_handler,
                    )
                    .map_err(|error| RuntimeRegistryError::Initialization(error.to_string()))?;
                    *typed.lock().map_err(|_| {
                        RuntimeRegistryError::Initialization(format!(
                            "{} controller mutex was poisoned",
                            provider.label()
                        ))
                    })? = Some(Arc::clone(&controller));
                    Ok(controller as Arc<dyn ProviderRuntimeController>)
                },
            )
            .map_err(|error| registry_error(error, self.provider, session, provider_turn_id))?;
        self.typed_controller
            .lock()
            .map_err(|_| mutex_error(&format!("{} controller", self.provider.label())))?
            .clone()
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_START_FAILED,
                    format!("{} ACP controller was not installed", self.provider.label()),
                    json!({
                        "provider": self.provider.code(),
                        "operation": "runtime",
                        "stage": "startup",
                        "threadId": session.thread_id,
                        "providerSessionId": session.provider_session_id,
                        "providerTurnId": provider_turn_id,
                        "runtimeGeneration": null,
                        "processId": null,
                    }),
                )
            })
    }

    fn ensure_session(
        &self,
        controller: &Arc<AcpController>,
        session: &PersistentProviderSessionIntent,
    ) -> Result<String, PedelecError> {
        if self.provider == AcpProviderKind::OpenCode {
            write_session_instruction(session)?;
        }
        let resumed = session.provider_session_id.is_some();
        let provider_id = controller
            .ensure_session(
                &session.thread_id,
                session.provider_session_id.as_deref(),
                &AcpSessionConfig::new(&session.workspace_path),
            )
            .map_err(|error| {
                acp_error(
                    error,
                    Some(controller),
                    Some(&session.thread_id),
                    session.provider_session_id.as_deref(),
                    None,
                    "admission",
                    self.provider,
                )
            })?;

        if self.provider.is_cursor() {
            configure_cursor_mode(controller, &provider_id, &session.thread_id)?;
        }

        if self.provider.is_cursor() {
            configure_cursor_session_settings(controller, session, &provider_id)?;
        } else if let Some(model) = session.model.as_deref() {
            let options = controller
                .session_config_options(&provider_id)
                .unwrap_or_else(|| json!([]));
            let option = find_config_option(&options, "model").ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_PROTOCOL_ERROR,
                    format!(
                        "{} did not advertise a model session config option",
                        self.provider.label()
                    ),
                    json!({
                        "provider": self.provider.code(),
                        "operation": "session/config",
                        "stage": "model",
                        "threadId": session.thread_id,
                        "providerSessionId": provider_id,
                        "model": model,
                        "runtimeGeneration": controller.generation(),
                        "processId": controller.process_id(),
                    }),
                )
            })?;
            validate_select_value_for_provider(option, model, self.provider).map_err(
                |mut error| {
                    error.details = Some(json!({
                        "provider": self.provider.code(),
                        "operation": "session/set_config_option",
                        "stage": "model",
                        "threadId": session.thread_id,
                        "providerSessionId": provider_id,
                        "model": model,
                        "runtimeGeneration": controller.generation(),
                        "processId": controller.process_id(),
                    }));
                    error
                },
            )?;
            controller
                .set_config_option(
                    &provider_id,
                    &AcpConfigOptionUpdate {
                        config_id: option["id"].as_str().unwrap().to_string(),
                        value: Value::String(model.to_string()),
                    },
                )
                .map_err(|error| {
                    acp_error(
                        error,
                        Some(controller),
                        Some(&session.thread_id),
                        Some(&provider_id),
                        None,
                        "model",
                        self.provider,
                    )
                })?;
        }
        self.reduce_session_ready_if_current(
            controller,
            session,
            &provider_id,
            resumed,
            None,
            "admission",
        )?;
        Ok(provider_id)
    }

    fn start_rpc_traffic_pump(&self, controller: Arc<AcpController>) {
        let generation = controller.generation();
        if !self
            .traffic_pumps
            .lock()
            .map(|mut pumps| pumps.insert(generation))
            .unwrap_or(false)
        {
            return;
        }
        let runtime = Arc::clone(&self.core_runtime);
        let typed = Arc::clone(&self.typed_controller);
        let traffic_pumps = Arc::clone(&self.traffic_pumps);
        let provider = self.provider;
        let process_id = controller.process_id();
        thread::spawn(move || {
            loop {
                match controller.recv_protocol_traffic_timeout(std::time::Duration::from_millis(50))
                {
                    Ok(record) => {
                        let current = match typed.lock() {
                            Ok(current) => current,
                            Err(_) => break,
                        };
                        if !current
                            .as_ref()
                            .is_some_and(|active| active.generation() == generation)
                        {
                            break;
                        }
                        record_protocol_traffic(
                            &runtime,
                            provider.provider_code(),
                            generation,
                            process_id,
                            record,
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !controller.is_healthy() {
                            break;
                        }
                        let is_current = typed
                            .lock()
                            .map(|current| {
                                current
                                    .as_ref()
                                    .is_some_and(|active| active.generation() == generation)
                            })
                            .unwrap_or(false);
                        if !is_current {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            if let Ok(mut pumps) = traffic_pumps.lock() {
                pumps.remove(&generation);
            }
        });
    }

    fn start_event_pump(&self, controller: Arc<AcpController>) {
        self.start_rpc_traffic_pump(Arc::clone(&controller));
        let generation = controller.generation();
        if !self
            .event_pumps
            .lock()
            .map(|mut pumps| pumps.insert(generation))
            .unwrap_or(false)
        {
            return;
        }
        record(
            &self.core_runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                provider: self.provider.provider_code(),
                runtime_generation: generation,
                process_id: controller.process_id(),
                selected_executable_path: self
                    .selection
                    .as_ref()
                    .map(|s| s.executable_path.clone()),
                selected_version: self.selection.as_ref().map(|s| s.version.clone()),
            },
        );
        let runtime = Arc::clone(&self.core_runtime);
        let typed = Arc::clone(&self.typed_controller);
        let event_pumps = Arc::clone(&self.event_pumps);
        let stopped = Arc::clone(&self.stopped_generations);
        let members = Arc::clone(&self.members);
        let provider = self.provider;
        thread::spawn(move || {
            while let Ok(event) = controller.recv_event() {
                // Hold the current-generation lock while reducing the event.
                // Checking it and then releasing the lock leaves a small but
                // real window in which a replacement can be installed before
                // an old callback mutates Core.
                let current = match typed.lock() {
                    Ok(current) => current,
                    Err(_) => break,
                };
                if !current
                    .as_ref()
                    .is_some_and(|active| active.generation() == generation)
                {
                    break;
                }
                handle_event_scoped(&runtime, &controller, provider, event, Some(&members));
            }
            if let Ok(mut pumps) = event_pumps.lock() {
                pumps.remove(&generation);
            }
            mark_stopped(
                &runtime,
                &stopped,
                &controller,
                provider,
                "runtime event pump ended",
            );
        });
    }

    fn any_controller(&self) -> Option<Arc<AcpController>> {
        self.typed_controller
            .lock()
            .ok()
            .and_then(|item| item.clone())
    }

    fn remove_workspace(&self, thread_id: &str) {
        if let Ok(mut workspaces) = self.workspaces.lock() {
            workspaces.remove(thread_id);
        }
    }

    fn reduce_session_ready_if_current(
        &self,
        controller: &AcpController,
        session: &PersistentProviderSessionIntent,
        provider_session_id: &str,
        resumed: bool,
        provider_turn_id: Option<&str>,
        stage: &str,
    ) -> Result<(), PedelecError> {
        let current = self
            .typed_controller
            .lock()
            .map_err(|_| mutex_error(&format!("{} controller", self.provider.label())))?;
        if current
            .as_ref()
            .is_some_and(|candidate| candidate.generation() == controller.generation())
            && controller.is_healthy()
        {
            let result =
                reduce_session_ready(&self.core_runtime, &session.thread_id, provider_session_id);
            if result.is_ok() {
                record(
                    &self.core_runtime,
                    ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                        provider: self.provider.provider_code(),
                        runtime_generation: controller.generation(),
                        process_id: controller.process_id(),
                        thread_id: session.thread_id.clone(),
                        provider_thread_id: provider_session_id.to_string(),
                        resumed,
                    },
                );
            }
            return result;
        }
        Err(acp_error(
            AcpRuntimeError::RuntimeDisconnected {
                operation: "session/attach".to_string(),
                message: format!(
                    "{} ACP runtime generation was replaced or disconnected",
                    self.provider.label()
                ),
            },
            Some(controller),
            Some(&session.thread_id),
            Some(provider_session_id),
            provider_turn_id,
            stage,
            self.provider,
        ))
    }

    fn fail_unhealthy_controller(&self, excluded: Option<&str>) {
        let unhealthy = self
            .typed_controller
            .lock()
            .ok()
            .and_then(|item| item.clone())
            .filter(|item| !item.is_healthy());
        if let Some(controller) = unhealthy {
            if let Ok(mut core) = self.core_runtime.lock() {
                crate::selection::fail_bound_runtime(
                    &mut core,
                    &self.members,
                    excluded,
                    PedelecError::with_details(
                        error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                        format!("{} ACP runtime disconnected", self.provider.label()),
                        json!({"provider":self.provider.code(),"runtimeGeneration":controller.generation(),"processId":controller.process_id()}),
                    ),
                );
            }
        }
    }

    pub fn record_shutdown_diagnostic(&self) {
        self.router
            .for_each(|child| child.record_shutdown_diagnostic());
        if let Some(controller) = self
            .typed_controller
            .lock()
            .ok()
            .and_then(|item| item.clone())
        {
            mark_stopped(
                &self.core_runtime,
                &self.stopped_generations,
                &controller,
                self.provider,
                "desktop shutdown",
            );
        }
    }
}

impl PersistentRuntimeDispatcher for AcpRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        if operation.provider() != &self.provider.provider_code() {
            return Err(PedelecError::new(
                error_codes::PROVIDER_UNSUPPORTED,
                "provider runtime received an operation for another provider",
            ));
        }
        // Legacy protocol fixtures can bypass discovery; production always
        // captures Core's completed scan before entering a generation.
        if self.selection.is_none() && self.program_override.is_none() {
            return self.router.dispatch(
                &self.core_runtime,
                &self.owner,
                self.provider.provider_code(),
                operation,
                |selection, key| {
                    let mut child = Self::new_for_provider(
                        self.provider,
                        self.owner.clone(),
                        self.core_runtime.clone(),
                    );
                    child.program_override = Some(selection.executable_path.clone());
                    child.process_cwd = self.process_cwd.clone();
                    child.launch_env = self.launch_env.clone();
                    child.selection = Some(selection);
                    child.registry_key = Some(key);
                    child
                },
                |child| {
                    child
                        .typed_controller
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_none_or(|c| c.is_healthy())
                },
                |child, operation| {
                    let id = operation.thread_id().to_owned();
                    let ending = matches!(operation, PersistentRuntimeOperation::EndSession { .. });
                    let already_bound = child.members.lock().unwrap().contains(&id);
                    let disconnected = child
                        .typed_controller
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|c| !c.is_healthy());
                    if !ending && already_bound && disconnected {
                        child.fail_unhealthy_controller(None);
                        return Err(PedelecError::new(
                            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                            "bound provider runtime disconnected",
                        ));
                    }
                    child.members.lock().unwrap().insert(id.clone());
                    let result = child.dispatch_generation(operation);
                    if ending || (!already_bound && result.is_err()) {
                        child.members.lock().unwrap().remove(&id);
                    }
                    result
                },
            );
        }
        let id = operation.thread_id().to_owned();
        let ending = matches!(operation, PersistentRuntimeOperation::EndSession { .. });
        self.members.lock().unwrap().insert(id.clone());
        let result = self.dispatch_generation(operation);
        if ending {
            self.members.lock().unwrap().remove(&id);
        }
        result
    }
}

impl AcpRuntimeDispatcher {
    fn dispatch_generation(
        &self,
        operation: PersistentRuntimeOperation,
    ) -> Result<(), PedelecError> {
        let expected_provider = self.provider.provider_code();
        if operation.provider() != &expected_provider {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_UNSUPPORTED,
                format!(
                    "{} ACP runtime received an operation for another provider",
                    self.provider.label()
                ),
                json!({
                    "provider": self.provider.code(),
                    "operationProvider": operation.provider(),
                }),
            ));
        }
        if let PersistentRuntimeOperation::EndSession { session } = &operation {
            let Some(controller) = self.any_controller() else {
                self.remove_workspace(&session.thread_id);
                return if session.active_provider_turn_id.is_some() {
                    Err(runtime_end_error(
                        session,
                        self.provider,
                        "active turn has no healthy ACP runtime",
                        None,
                    ))
                } else {
                    Ok(())
                };
            };
            if !controller.is_healthy() {
                if session.active_provider_turn_id.is_some() {
                    if let Some(provider_id) = session.provider_session_id.as_deref() {
                        let _ =
                            controller.cancel_and_detach_session(&session.thread_id, provider_id);
                    } else {
                        controller.forget_session(&session.thread_id);
                    }
                } else {
                    controller.forget_session(&session.thread_id);
                }
                self.remove_workspace(&session.thread_id);
                return if session.active_provider_turn_id.is_some() {
                    Err(runtime_end_error(
                        session,
                        self.provider,
                        "active turn has no healthy ACP runtime",
                        Some(&controller),
                    ))
                } else {
                    Ok(())
                };
            }
            let result = if session.active_provider_turn_id.is_some() {
                let Some(provider_id) = session.provider_session_id.as_deref() else {
                    self.remove_workspace(&session.thread_id);
                    return Err(runtime_end_error(
                        session,
                        self.provider,
                        "active turn has no provider session id",
                        Some(&controller),
                    ));
                };
                controller
                    .cancel_and_detach_session(&session.thread_id, provider_id)
                    .map_err(|error| {
                        acp_error(
                            error,
                            Some(&controller),
                            Some(&session.thread_id),
                            Some(provider_id),
                            session.active_provider_turn_id.as_deref(),
                            "end",
                            self.provider,
                        )
                    })
            } else {
                controller
                    .detach_session(&session.thread_id)
                    .map_err(|error| {
                        acp_error(
                            error,
                            Some(&controller),
                            Some(&session.thread_id),
                            session.provider_session_id.as_deref(),
                            None,
                            "end",
                            self.provider,
                        )
                    })
            };
            self.remove_workspace(&session.thread_id);
            result
        } else {
            let session = match &operation {
                PersistentRuntimeOperation::EnsureSession { session } => session,
                PersistentRuntimeOperation::StartTurn { turn } => &turn.session,
                PersistentRuntimeOperation::EndSession { .. } => unreachable!(),
            };
            let provider_turn_id = match &operation {
                PersistentRuntimeOperation::StartTurn { turn } => Some(turn.local_turn_id.as_str()),
                PersistentRuntimeOperation::EnsureSession { .. }
                | PersistentRuntimeOperation::EndSession { .. } => None,
            };
            let controller = self.controller_for(session, provider_turn_id)?;
            self.start_event_pump(Arc::clone(&controller));
            match operation {
                PersistentRuntimeOperation::EnsureSession { session } => {
                    self.ensure_session(&controller, &session).map(|_| ())
                }
                PersistentRuntimeOperation::StartTurn { turn } => {
                    let provider_id = self.ensure_session(&controller, &turn.session)?;
                    let use_bootstrap = self.provider.is_cursor()
                        && controller.session_needs_bootstrap(&provider_id);
                    let prompt = if use_bootstrap {
                        build_persistent_user_prompt_with_bootstrap(
                            &turn.session.host_instructions,
                            &turn.message,
                        )
                    } else {
                        turn.message.clone()
                    };
                    let prompt = shape_user_prompt_for_provider(self.provider, &prompt);
                    controller
                        .start_turn(&turn.thread_id, &provider_id, &turn.local_turn_id, &prompt)
                        .map_err(|error| {
                            acp_error(
                                error,
                                Some(&controller),
                                Some(&turn.thread_id),
                                Some(&provider_id),
                                Some(&turn.local_turn_id),
                                "admission",
                                self.provider,
                            )
                        })?;
                    if use_bootstrap {
                        controller.mark_session_bootstrapped(&provider_id);
                    }
                    record(
                        &self.core_runtime,
                        ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                            provider: self.provider.provider_code(),
                            runtime_generation: controller.generation(),
                            process_id: controller.process_id(),
                            thread_id: turn.thread_id,
                            provider_thread_id: provider_id,
                            provider_turn_id: turn.local_turn_id,
                        },
                    );
                    Ok(())
                }
                PersistentRuntimeOperation::EndSession { .. } => unreachable!(),
            }
        }
    }
}

fn shape_user_prompt_for_provider(provider: AcpProviderKind, prompt: &str) -> String {
    if provider == AcpProviderKind::OpenCode && prompt.trim_start().starts_with('/') {
        format!("- {prompt}")
    } else {
        prompt.to_owned()
    }
}

/// Compatibility wrapper for the phase-1 OpenCode adapter. The session,
/// prompt, cancellation, permission, and event lifecycle lives in
/// [`AcpRuntimeDispatcher`] and is shared with Cursor.
#[derive(Debug, Clone)]
pub struct OpenCodeRuntimeDispatcher {
    inner: AcpRuntimeDispatcher,
}

impl OpenCodeRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self {
            inner: AcpRuntimeDispatcher::new_for_provider(
                AcpProviderKind::OpenCode,
                owner,
                core_runtime,
            ),
        }
    }

    #[doc(hidden)]
    pub fn with_program_for_test(mut self, program: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.with_program_for_test(program);
        self
    }

    #[doc(hidden)]
    pub fn with_process_cwd_for_test(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.with_process_cwd_for_test(cwd);
        self
    }

    #[doc(hidden)]
    pub fn with_env_for_test(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner = self.inner.with_env_for_test(key, value);
        self
    }

    pub fn record_shutdown_diagnostic(&self) {
        self.inner.record_shutdown_diagnostic();
    }
}

impl PersistentRuntimeDispatcher for OpenCodeRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        self.inner.dispatch(operation)
    }
}

#[derive(Debug, Clone)]
pub struct CursorRuntimeDispatcher {
    inner: AcpRuntimeDispatcher,
}

impl CursorRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self {
            inner: AcpRuntimeDispatcher::new_for_provider(
                AcpProviderKind::Cursor,
                owner,
                core_runtime,
            ),
        }
    }

    #[doc(hidden)]
    pub fn with_program_for_test(mut self, program: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.with_program_for_test(program);
        self
    }

    #[doc(hidden)]
    pub fn with_process_cwd_for_test(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.with_process_cwd_for_test(cwd);
        self
    }

    #[doc(hidden)]
    pub fn with_env_for_test(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner = self.inner.with_env_for_test(key, value);
        self
    }

    pub fn record_shutdown_diagnostic(&self) {
        self.inner.record_shutdown_diagnostic();
    }
}

impl PersistentRuntimeDispatcher for CursorRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        self.inner.dispatch(operation)
    }
}

fn configure_cursor_session_settings(
    controller: &AcpController,
    session: &PersistentProviderSessionIntent,
    provider_session_id: &str,
) -> Result<(), PedelecError> {
    if let Some(model) = session.model.as_deref() {
        let options = controller
            .session_config_options(provider_session_id)
            .unwrap_or_else(|| json!([]));
        apply_cursor_config_value(
            controller,
            session,
            provider_session_id,
            &options,
            "model",
            None,
            model,
        )?;
    }

    if let Some(effort) = session
        .cursor_settings
        .as_ref()
        .and_then(|settings| settings.effort.as_deref())
    {
        // Model selection can replace the dependent option list. Read the
        // controller cache again after each successful set_config_option.
        let options = controller
            .session_config_options(provider_session_id)
            .unwrap_or_else(|| json!([]));
        apply_cursor_config_value(
            controller,
            session,
            provider_session_id,
            &options,
            "thought_level",
            None,
            effort,
        )?;
    }

    if let Some(fast) = session
        .cursor_settings
        .as_ref()
        .and_then(|settings| settings.fast)
    {
        let options = controller
            .session_config_options(provider_session_id)
            .unwrap_or_else(|| json!([]));
        apply_cursor_config_value(
            controller,
            session,
            provider_session_id,
            &options,
            "model_config",
            Some("fast"),
            if fast { "true" } else { "false" },
        )?;
    }
    Ok(())
}

fn apply_cursor_config_value(
    controller: &AcpController,
    session: &PersistentProviderSessionIntent,
    provider_session_id: &str,
    options: &Value,
    category: &str,
    required_id: Option<&str>,
    requested: &str,
) -> Result<(), PedelecError> {
    let stage = match category {
        "thought_level" => "effort",
        "model_config" => "fast",
        _ => "model",
    };
    let option = options.as_array().and_then(|options| {
        options.iter().find(|option| {
            option.get("category").and_then(Value::as_str) == Some(category)
                && required_id.is_none_or(|id| option.get("id").and_then(Value::as_str) == Some(id))
                && option
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
        })
    });
    let Some(option) = option else {
        return Err(cursor_config_error(
            controller,
            session,
            provider_session_id,
            stage,
            requested,
            error_codes::PROVIDER_PROTOCOL_ERROR,
            match stage {
                "effort" => "Cursor did not advertise a thought-level session config option",
                "fast" => "Cursor did not advertise a compatible Fast session config option",
                _ => "Cursor did not advertise a model session config option",
            },
        ));
    };

    let advertised = option
        .get("options")
        .and_then(Value::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| config_value_id(value) == Some(requested))
        });
    if !advertised {
        return Err(cursor_config_error(
            controller,
            session,
            provider_session_id,
            stage,
            requested,
            error_codes::PROVIDER_REQUEST_FAILED,
            match stage {
                "effort" => "the selected Cursor effort is not advertised for this model",
                "fast" => "the selected Cursor Fast value is not advertised for this model",
                _ => "the selected Cursor model is not advertised by this session",
            },
        ));
    }

    let config_id = option["id"].as_str().expect("validated config option id");
    controller
        .set_config_option(
            provider_session_id,
            &AcpConfigOptionUpdate {
                config_id: config_id.to_string(),
                value: Value::String(requested.to_string()),
            },
        )
        .map_err(|error| {
            let mut error = acp_error(
                error,
                Some(controller),
                Some(&session.thread_id),
                Some(provider_session_id),
                None,
                stage,
                AcpProviderKind::Cursor,
            );
            if let Some(details) = error.details.as_mut() {
                details["requestedValue"] = Value::String(requested.to_string());
                details[stage] = Value::String(requested.to_string());
            }
            error
        })?;
    Ok(())
}

fn cursor_config_error(
    controller: &AcpController,
    session: &PersistentProviderSessionIntent,
    provider_session_id: &str,
    stage: &str,
    requested: &str,
    code: &str,
    message: &str,
) -> PedelecError {
    let mut details = json!({
        "provider": "cursor",
        "operation": "session/set_config_option",
        "stage": stage,
        "threadId": session.thread_id,
        "providerSessionId": provider_session_id,
        "requestedValue": requested,
        "runtimeGeneration": controller.generation(),
        "processId": controller.process_id(),
    });
    details[stage] = Value::String(requested.to_string());
    PedelecError::with_details(code, message, details)
}

fn find_config_option<'a>(options: &'a Value, category: &str) -> Option<&'a Value> {
    options.as_array()?.iter().find(|option| {
        option.get("category").and_then(Value::as_str) == Some(category)
            && option.get("id").and_then(Value::as_str).is_some()
    })
}

fn validate_select_value_for_provider(
    option: &Value,
    selected: &str,
    provider: AcpProviderKind,
) -> Result<(), PedelecError> {
    let accepted = option
        .get("options")
        .and_then(Value::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| config_value_id(value) == Some(selected))
        });
    if accepted {
        return Ok(());
    }
    Err(PedelecError::with_details(
        error_codes::PROVIDER_REQUEST_FAILED,
        format!(
            "the selected {} model is not advertised by this session",
            provider.label()
        ),
        json!({"provider":provider.code(),"operation":"session/set_config_option","model":selected}),
    ))
}

fn config_value_id(value: &Value) -> Option<&str> {
    value
        .get("value")
        .and_then(Value::as_str)
        .or_else(|| value.get("valueId").and_then(Value::as_str))
        .or_else(|| value.as_str())
}

fn configure_cursor_mode(
    controller: &AcpController,
    provider_session_id: &str,
    thread_id: &str,
) -> Result<(), PedelecError> {
    if let Some(modes) = controller.session_modes(provider_session_id) {
        if let Some(mode_id) = choose_agent_mode(&modes) {
            controller
                .set_mode(provider_session_id, &mode_id)
                .map_err(|error| {
                    acp_error(
                        error,
                        Some(controller),
                        Some(thread_id),
                        Some(provider_session_id),
                        None,
                        "mode",
                        AcpProviderKind::Cursor,
                    )
                })?;
            return Ok(());
        }
    }

    let options = controller
        .session_config_options(provider_session_id)
        .unwrap_or_else(|| json!([]));
    if let Some(option) = find_config_option(&options, "mode") {
        let mode_id = option
            .get("options")
            .and_then(Value::as_array)
            .and_then(|values| {
                values.iter().find_map(|value| {
                    let id = config_value_id(value)?;
                    is_agent_mode_label(value).then(|| id.to_string())
                })
            })
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_PROTOCOL_ERROR,
                    "Cursor did not advertise a full agent mode",
                    json!({
                        "provider": "cursor",
                        "operation": "session/mode",
                        "stage": "mode",
                        "threadId": thread_id,
                        "providerSessionId": provider_session_id,
                        "runtimeGeneration": controller.generation(),
                        "processId": controller.process_id(),
                    }),
                )
            })?;
        controller
            .set_config_option(
                provider_session_id,
                &AcpConfigOptionUpdate {
                    config_id: option["id"].as_str().unwrap().to_string(),
                    value: Value::String(mode_id),
                },
            )
            .map_err(|error| {
                acp_error(
                    error,
                    Some(controller),
                    Some(thread_id),
                    Some(provider_session_id),
                    None,
                    "mode",
                    AcpProviderKind::Cursor,
                )
            })?;
        return Ok(());
    }

    Err(PedelecError::with_details(
        error_codes::PROVIDER_PROTOCOL_ERROR,
        "Cursor did not advertise ACP mode selection",
        json!({
            "provider": "cursor",
            "operation": "session/mode",
            "stage": "mode",
            "threadId": thread_id,
            "providerSessionId": provider_session_id,
            "runtimeGeneration": controller.generation(),
            "processId": controller.process_id(),
        }),
    ))
}

fn choose_agent_mode(modes: &Value) -> Option<String> {
    let available = modes.get("availableModes").and_then(Value::as_array)?;
    available
        .iter()
        .filter_map(|mode| {
            let id = mode
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| mode.get("modeId").and_then(Value::as_str))
                .or_else(|| mode.as_str())?;
            let label = format!(
                "{} {} {}",
                id,
                mode.get("name").and_then(Value::as_str).unwrap_or_default(),
                mode.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )
            .to_ascii_lowercase();
            if label.contains("ask")
                || label.contains("plan")
                || label.contains("read-only")
                || label.contains("readonly")
                || label.contains("read only")
            {
                return None;
            }
            let score = if label == "agent" || label.starts_with("agent ") {
                100
            } else if label.contains("agent") {
                80
            } else if label.contains("full") || label.contains("tool") {
                60
            } else {
                0
            };
            (score > 0).then(|| (score, id.to_string()))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, id)| id)
}

fn is_agent_mode_label(value: &Value) -> bool {
    let label = format!(
        "{} {} {}",
        config_value_id(value).unwrap_or_default(),
        value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_ascii_lowercase();
    (label.contains("agent") || label.contains("full") || label.contains("tool"))
        && !label.contains("ask")
        && !label.contains("plan")
        && !label.contains("read-only")
        && !label.contains("readonly")
        && !label.contains("read only")
}

fn cursor_auth_available(launch_env: &[(String, String)]) -> bool {
    ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"].iter().any(|key| {
        launch_env
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| !value.trim().is_empty())
            .or_else(|| env::var(key).ok().map(|value| !value.trim().is_empty()))
            .unwrap_or(false)
    })
}

#[derive(Debug, Clone, Copy)]
struct CursorExtensionRequestHandler;

impl AcpExtensionRequestHandler for CursorExtensionRequestHandler {
    fn handle(&self, request: &RpcServerRequest) -> Option<Value> {
        match request.method.as_str() {
            // Pedelec deliberately has no Cursor-specific interactive UI in
            // this phase. Return the provider-shaped negative result so the
            // agent can continue without waiting for a client decision.
            "cursor/ask_question" => Some(json!({
                "outcome": {
                    "outcome": "skipped",
                    "reason": "Pedelec does not provide an interactive Cursor question UI"
                }
            })),
            "cursor/create_plan" => Some(json!({
                "outcome": {
                    "outcome": "rejected",
                    "reason": "Pedelec does not provide a Cursor plan approval UI"
                }
            })),
            _ => None,
        }
    }
}

fn write_session_instruction(
    session: &PersistentProviderSessionIntent,
) -> Result<(), PedelecError> {
    let path = session.workspace_path.join(OPENCODE_INSTRUCTION_PATH);
    let parent = path.parent().expect("instruction path has a parent");
    fs::create_dir_all(parent).map_err(|error| instruction_error(&path, error))?;
    fs::write(&path, &session.host_instructions).map_err(|error| instruction_error(&path, error))
}

fn instruction_error(path: &Path, error: std::io::Error) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_BOOTSTRAP_ASSET_FAILED,
        "failed to install OpenCode session instructions",
        json!({"provider":"opencode","path":path,"error":error.to_string()}),
    )
}

fn opencode_acp_config_content() -> Result<String, PedelecError> {
    merge_opencode_acp_config(env::var(OPENCODE_CONFIG_CONTENT).ok().as_deref())
}

// OpenCode-specific privileged instruction contract (not an ACP standard
// method): current OpenCode loads config.instructions in
// packages/opencode/src/session/instruction.ts (Instruction.system), and the
// session prompt pipeline includes those loaded instructions in its system
// context. Keep the Pedelec asset workspace-scoped so a provider process can
// serve multiple sessions without introducing app-global thread state.
fn merge_opencode_acp_config(existing: Option<&str>) -> Result<String, PedelecError> {
    let mut root = match existing {
        Some(value) => serde_json::from_str::<Value>(value).map_err(|error| {
            PedelecError::with_details(
                error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
                "OPENCODE_CONFIG_CONTENT must be valid JSON",
                json!({"provider":"opencode","error":error.to_string()}),
            )
        })?,
        None => json!({}),
    };
    let object = root.as_object_mut().ok_or_else(|| {
        PedelecError::new(
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
            "OPENCODE_CONFIG_CONTENT must contain a JSON object",
        )
    })?;
    let instructions = object
        .entry("instructions")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
                "OPENCODE_CONFIG_CONTENT.instructions must be an array",
            )
        })?;
    if !instructions
        .iter()
        .any(|value| value.as_str() == Some(OPENCODE_INSTRUCTION_PATH))
    {
        instructions.push(Value::String(OPENCODE_INSTRUCTION_PATH.to_string()));
    }
    serde_json::to_string(&root).map_err(|error| {
        PedelecError::new(
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
            error.to_string(),
        )
    })
}

fn resolve_workspace_permission(
    workspaces: &Mutex<HashMap<String, PathBuf>>,
    request: &AcpPermissionRequest,
) -> AcpPermissionDecision {
    workspaces
        .lock()
        .ok()
        .and_then(|paths| paths.get(&request.pedelec_thread_id).cloned())
        .map(|workspace| {
            if is_pedelec_helper_execution(&request.tool_call) {
                return AcpPermissionDecision::AllowOnce;
            }
            pedelec_runtime::AcpPermissionResolver::resolve(
                &AcpWorkspacePermissionPolicy::new(workspace),
                request,
            )
        })
        .unwrap_or(AcpPermissionDecision::RejectOnce)
}

fn is_pedelec_helper_execution(tool_call: &Value) -> bool {
    // ACP providers may omit rawInput.command from permission requests. Cursor
    // keeps the command in title and sometimes in nested content text instead.
    if tool_call
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "execute")
    {
        return false;
    }

    let mut descriptions = Vec::new();
    collect_command_descriptions(tool_call, &mut descriptions);
    descriptions.into_iter().any(|description| {
        ["pedelec-deno", "pedelec-cli"]
            .iter()
            .any(|helper| contains_helper_name(description, helper))
    })
}

fn collect_command_descriptions<'a>(value: &'a Value, descriptions: &mut Vec<&'a str>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "command" | "title" | "text") {
                    if let Some(description) = value.as_str() {
                        descriptions.push(description);
                    }
                } else if matches!(key.as_str(), "rawInput" | "content") {
                    collect_command_descriptions(value, descriptions);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_command_descriptions(value, descriptions);
            }
        }
        _ => {}
    }
}

fn contains_helper_name(description: &str, helper: &str) -> bool {
    description.match_indices(helper).any(|(start, _)| {
        let before = description[..start].chars().next_back();
        let after = description[start + helper.len()..].chars().next();
        let name_char =
            |character: char| character.is_ascii_alphanumeric() || matches!(character, '_' | '-');
        before.is_none_or(|character| !name_char(character))
            && after.is_none_or(|character| !name_char(character))
    })
}

fn opencode_permission_overlay() -> String {
    let existing = env::var(OPENCODE_PERMISSION).ok();
    let Some(mut value) = existing
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()
        .ok()
        .flatten()
        .and_then(|value| value.as_object().cloned())
        .or_else(|| existing.is_none().then(serde_json::Map::new))
    else {
        return existing.unwrap_or_default();
    };
    value.insert("skill".into(), Value::String("deny".into()));
    Value::Object(value).to_string()
}

fn acp_artifact_invalid(
    provider: AcpProviderKind,
    thread_id: &str,
    turn_id: &str,
    provider_artifact_id: Option<&str>,
    tool_call_id: Option<&str>,
    stage: &str,
    message: &str,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_ARTIFACT_INVALID,
        message,
        json!({
            "provider": provider.code(),
            "threadId": thread_id,
            "providerTurnId": turn_id,
            "providerArtifactId": provider_artifact_id,
            "toolCallId": tool_call_id,
            "stage": stage,
        }),
    )
}

fn artifact_kind_from_mime(mime_type: &str) -> ProviderArtifactKind {
    let mime = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime.starts_with("image/") {
        ProviderArtifactKind::Image
    } else if mime.starts_with("audio/") {
        ProviderArtifactKind::Audio
    } else if mime.starts_with("video/") {
        ProviderArtifactKind::Video
    } else {
        ProviderArtifactKind::File
    }
}

fn materialize_acp_artifact(
    runtime: &SharedCoreRuntime,
    provider: AcpProviderKind,
    thread_id: &str,
    turn_id: &str,
    provider_artifact_id: Option<String>,
    tool_call_id: Option<String>,
    source: AcpArtifactSource,
    suggested_filename: &str,
    mime_type: Option<&str>,
    payload: Result<Vec<u8>, String>,
) -> Result<(), PedelecError> {
    let mime_type = mime_type
        .filter(|mime| !mime.trim().is_empty() && !mime.contains(['\r', '\n']))
        .ok_or_else(|| {
            acp_artifact_invalid(
                provider,
                thread_id,
                turn_id,
                provider_artifact_id.as_deref(),
                tool_call_id.as_deref(),
                "metadata",
                "ACP artifact MIME type is missing or invalid",
            )
        })?;
    let bytes = payload.map_err(|reason| {
        acp_artifact_invalid(
            provider,
            thread_id,
            turn_id,
            provider_artifact_id.as_deref(),
            tool_call_id.as_deref(),
            "decode",
            &reason,
        )
    })?;
    let core_source = match source {
        AcpArtifactSource::ProviderArtifact => ProviderArtifactSource::ProviderArtifact,
        AcpArtifactSource::ToolResult => ProviderArtifactSource::ToolResult,
    };
    runtime
        .lock()
        .map_err(|_| mutex_error("ACP artifact Core runtime"))?
        .materialize_provider_artifact(ProviderArtifactInput {
            thread_id,
            provider: provider.provider_code(),
            provider_turn_id: Some(turn_id),
            kind: artifact_kind_from_mime(mime_type),
            source: core_source,
            provider_artifact_id,
            tool_call_id,
            suggested_filename,
            mime_type,
            payload: ProviderArtifactPayload::Bytes(&bytes),
        })?;
    Ok(())
}

fn cursor_image_file_details(path: &Path) -> (String, String) {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    };
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty() && *name != "." && *name != "..")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let extension = match mime_type {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => "bin",
            };
            format!("generated-image.{extension}")
        });
    (filename, mime_type.to_string())
}

fn materialize_cursor_generated_image(
    runtime: &SharedCoreRuntime,
    thread_id: &str,
    turn_id: &str,
    params: &Value,
) -> Result<(), PedelecError> {
    let Some(file_path) = params.get("filePath") else {
        return Ok(());
    };
    let Some(file_path) = file_path.as_str() else {
        return Err(acp_artifact_invalid(
            AcpProviderKind::Cursor,
            thread_id,
            turn_id,
            params.get("toolCallId").and_then(Value::as_str),
            params.get("toolCallId").and_then(Value::as_str),
            "source",
            "Cursor generated image filePath must be a string",
        ));
    };
    if file_path.trim().is_empty() {
        return Ok(());
    }
    let tool_call_id = params
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            acp_artifact_invalid(
                AcpProviderKind::Cursor,
                thread_id,
                turn_id,
                None,
                None,
                "metadata",
                "Cursor generated image toolCallId is missing",
            )
        })?;
    let path = PathBuf::from(file_path);
    if !path.is_absolute() {
        return Err(acp_artifact_invalid(
            AcpProviderKind::Cursor,
            thread_id,
            turn_id,
            Some(tool_call_id),
            Some(tool_call_id),
            "source",
            "Cursor generated image filePath must be absolute",
        ));
    }
    let (suggested_filename, mime_type) = cursor_image_file_details(&path);
    runtime
        .lock()
        .map_err(|_| mutex_error("Cursor artifact Core runtime"))?
        .materialize_provider_artifact(ProviderArtifactInput {
            thread_id,
            provider: ProviderCode::Cursor,
            provider_turn_id: Some(turn_id),
            kind: ProviderArtifactKind::Image,
            source: ProviderArtifactSource::ImageGeneration,
            provider_artifact_id: Some(tool_call_id.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            suggested_filename: &suggested_filename,
            mime_type: &mime_type,
            payload: ProviderArtifactPayload::File(&path),
        })?;
    Ok(())
}

fn fail_acp_artifact_operation(
    runtime: &SharedCoreRuntime,
    provider: AcpProviderKind,
    thread_id: &str,
    turn_id: &str,
    error: PedelecError,
) {
    if let Ok(mut core) = runtime.lock() {
        let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::ProviderError {
            thread_id: thread_id.to_string(),
            provider_turn_id: Some(turn_id.to_string()),
            error: error.clone(),
        });
    }
    record(
        runtime,
        ProviderRuntimeDiagnostic::ProviderRuntimeError {
            provider: provider.provider_code(),
            runtime_generation: None,
            process_id: None,
            thread_id: Some(thread_id.to_string()),
            provider_thread_id: None,
            provider_turn_id: Some(turn_id.to_string()),
            code: error.code.clone(),
            message: error.message.clone(),
            details: error.details.clone(),
        },
    );
}

fn handle_provider_notification(
    runtime: &SharedCoreRuntime,
    provider: AcpProviderKind,
    method: &str,
    params: &Value,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
) {
    if provider != AcpProviderKind::Cursor || method != "cursor/generate_image" {
        return;
    }
    let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
        return;
    };
    if let Err(error) = materialize_cursor_generated_image(runtime, thread_id, turn_id, params) {
        fail_acp_artifact_operation(runtime, provider, thread_id, turn_id, error);
    }
}

#[cfg(test)]
fn handle_event(
    runtime: &SharedCoreRuntime,
    controller: &AcpController,
    provider: AcpProviderKind,
    event: AcpRuntimeEvent,
) {
    handle_event_scoped(runtime, controller, provider, event, None);
}

fn handle_event_scoped(
    runtime: &SharedCoreRuntime,
    controller: &AcpController,
    provider: AcpProviderKind,
    event: AcpRuntimeEvent,
    members: Option<&Mutex<HashSet<String>>>,
) {
    match event {
        AcpRuntimeEvent::SessionReady { .. } => {}
        AcpRuntimeEvent::AssistantDelta {
            pedelec_thread_id,
            local_turn_id,
            text,
            ..
        } => reduce(
            runtime,
            ProviderRuntimeEvent::AssistantDelta {
                thread_id: pedelec_thread_id,
                provider_turn_id: Some(local_turn_id),
                text,
            },
        ),
        AcpRuntimeEvent::AssistantMessage {
            pedelec_thread_id,
            local_turn_id,
            text,
            ..
        } => reduce(
            runtime,
            ProviderRuntimeEvent::AssistantMessage {
                thread_id: pedelec_thread_id,
                provider_turn_id: Some(local_turn_id),
                text,
            },
        ),
        AcpRuntimeEvent::ProviderArtifact {
            pedelec_thread_id,
            local_turn_id,
            provider_artifact_id,
            tool_call_id,
            source,
            suggested_filename,
            mime_type,
            payload,
            ..
        } => {
            if let Err(error) = materialize_acp_artifact(
                runtime,
                provider,
                &pedelec_thread_id,
                &local_turn_id,
                provider_artifact_id,
                tool_call_id,
                source,
                &suggested_filename,
                mime_type.as_deref(),
                payload,
            ) {
                fail_acp_artifact_operation(
                    runtime,
                    provider,
                    &pedelec_thread_id,
                    &local_turn_id,
                    error,
                );
            }
        }
        AcpRuntimeEvent::UsageUpdated {
            pedelec_thread_id,
            local_turn_id,
            usage,
            ..
        } => reduce(
            runtime,
            ProviderRuntimeEvent::UsageUpdated {
                thread_id: pedelec_thread_id,
                provider_turn_id: local_turn_id,
                usage,
            },
        ),
        AcpRuntimeEvent::PromptUsageUpdated {
            pedelec_thread_id,
            usage,
            ..
        } => {
            if provider == AcpProviderKind::OpenCode {
                record_opencode_prompt_usage(runtime, &pedelec_thread_id, &usage);
            }
        }
        AcpRuntimeEvent::TurnCompleted {
            pedelec_thread_id,
            provider_session_id,
            local_turn_id,
            status,
            stop_reason,
            error,
        } => {
            record(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                    provider: provider.provider_code(),
                    runtime_generation: controller.generation(),
                    process_id: controller.process_id(),
                    thread_id: pedelec_thread_id.clone(),
                    provider_thread_id: provider_session_id.clone(),
                    provider_turn_id: Some(local_turn_id.clone()),
                    status: stop_reason.clone(),
                },
            );
            let success = status == AcpTurnStatus::Completed;
            let mapped = (!success).then(|| {
                let mut details = json!({
                    "provider": provider.code(),
                    "operation": "session/prompt",
                    "stage": "completion",
                    "threadId": pedelec_thread_id.clone(),
                    "providerSessionId": provider_session_id.clone(),
                    "providerTurnId": local_turn_id.clone(),
                    "stopReason": stop_reason.clone(),
                    "runtimeGeneration": controller.generation(),
                    "processId": controller.process_id(),
                });
                if let Some(error) = error {
                    details["error"] = error;
                }
                PedelecError::with_details(
                    error_codes::PROVIDER_REQUEST_FAILED,
                    format!("{} turn {stop_reason}", provider.label()),
                    details,
                )
            });
            reduce(
                runtime,
                ProviderRuntimeEvent::TurnCompleted {
                    thread_id: pedelec_thread_id,
                    provider_turn_id: Some(local_turn_id),
                    success,
                    error: mapped,
                },
            );
        }
        AcpRuntimeEvent::Notification {
            method,
            params,
            pedelec_thread_id,
            local_turn_id,
            ..
        } => handle_provider_notification(
            runtime,
            provider,
            &method,
            &params,
            pedelec_thread_id.as_deref(),
            local_turn_id.as_deref(),
        ),
        AcpRuntimeEvent::PermissionResolved { .. } => {}
        AcpRuntimeEvent::Stderr { text } => record(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                provider: provider.provider_code(),
                runtime_generation: controller.generation(),
                process_id: controller.process_id(),
                text,
            },
        ),
        AcpRuntimeEvent::ProtocolError {
            pedelec_thread_id,
            provider_session_id,
            operation,
            message,
        } => {
            controller.retire_for_protocol_error();
            let message = bounded_text(&message);
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROTOCOL_ERROR,
                &message,
                json!({
                    "provider": provider.code(),
                    "operation": operation,
                    "stage": "stream",
                    "threadId": pedelec_thread_id.clone(),
                    "providerSessionId": provider_session_id.clone(),
                    "runtimeGeneration": controller.generation(),
                    "processId": controller.process_id(),
                }),
            );
            record(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeError {
                    provider: provider.provider_code(),
                    runtime_generation: Some(controller.generation()),
                    process_id: Some(controller.process_id()),
                    thread_id: pedelec_thread_id,
                    provider_thread_id: provider_session_id,
                    provider_turn_id: None,
                    code: error.code.clone(),
                    message,
                    details: error.details.clone(),
                },
            );
            if let Ok(mut core) = runtime.lock() {
                if let Some(members) = members {
                    crate::selection::fail_bound_runtime(&mut core, members, None, error);
                } else {
                    core.fail_persistent_runtime(provider.provider_code(), error);
                }
            }
        }
        AcpRuntimeEvent::Disconnected {
            generation,
            pid,
            attachments,
            reason,
        } => {
            let reason_text = bounded_text(&format!("{reason:?}"));
            let attachment_details = attachments
                .iter()
                .map(|attachment| {
                    json!({
                        "threadId": attachment.pedelec_thread_id,
                        "providerSessionId": attachment.provider_session_id,
                    })
                })
                .collect::<Vec<_>>();
            record(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                    provider: provider.provider_code(),
                    runtime_generation: generation,
                    process_id: pid,
                    thread_id: None,
                    provider_thread_id: None,
                    reason: reason_text.clone(),
                },
            );
            if let Ok(mut core) = runtime.lock() {
                let error = PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    format!("{} ACP runtime disconnected", provider.label()),
                    json!({
                        "provider": provider.code(),
                        "operation": "runtime",
                        "stage": "disconnect",
                        "runtimeGeneration": generation,
                        "processId": pid,
                        "reason": reason_text.clone(),
                        "attachments": attachment_details,
                    }),
                );
                if let Some(members) = members {
                    crate::selection::fail_bound_runtime(&mut core, members, None, error);
                } else {
                    core.fail_persistent_runtime(provider.provider_code(), error);
                }
            }
            for attachment in attachments {
                record(
                    runtime,
                    ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                        provider: provider.provider_code(),
                        runtime_generation: generation,
                        process_id: pid,
                        thread_id: Some(attachment.pedelec_thread_id),
                        provider_thread_id: Some(attachment.provider_session_id),
                        reason: reason_text.clone(),
                    },
                );
            }
        }
    }
}

fn reduce(runtime: &SharedCoreRuntime, event: ProviderRuntimeEvent) {
    if let Ok(mut core) = runtime.lock() {
        let _ = core.reduce_provider_runtime_event(event);
    }
}

fn opencode_prompt_total_tokens(usage: &Value) -> Option<u64> {
    usage.get("totalTokens").and_then(Value::as_u64)
}

fn record_opencode_prompt_usage(runtime: &SharedCoreRuntime, thread_id: &str, usage: &Value) {
    let Some(total_tokens) = opencode_prompt_total_tokens(usage) else {
        return;
    };
    if let Ok(mut core) = runtime.lock() {
        if let Some(operation_id) = core.current_operation_id(thread_id) {
            // OpenCode's terminal prompt total is a per-prompt contribution.
            // Core makes the operation update idempotent if a terminal frame
            // is replayed, while ACP usage_update remains diagnostic-only.
            let _ = core.add_session_token_delta_once(thread_id, &operation_id, total_tokens);
        }
    }
}

fn record(runtime: &SharedCoreRuntime, event: ProviderRuntimeDiagnostic) {
    if let Ok(mut core) = runtime.lock() {
        core.record_provider_runtime_diagnostic(event);
    }
}
fn reduce_session_ready(
    runtime: &SharedCoreRuntime,
    thread: &str,
    provider: &str,
) -> Result<(), PedelecError> {
    runtime
        .lock()
        .map_err(|_| mutex_error("Core runtime"))?
        .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
            thread_id: thread.into(),
            provider_session_id: provider.into(),
        })
}
fn bounded_text(text: &str) -> String {
    super::truncate_diagnostic_text(text)
}
fn mutex_error(name: &str) -> PedelecError {
    PedelecError::new(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        format!("{name} mutex was poisoned"),
    )
}
fn registry_error(
    error: RuntimeRegistryError,
    provider: AcpProviderKind,
    session: &PersistentProviderSessionIntent,
    provider_turn_id: Option<&str>,
) -> PedelecError {
    let error_text = bounded_text(&error.to_string());
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_START_FAILED,
        format!("{} ACP runtime could not be started", provider.label()),
        json!({
            "provider": provider.code(),
            "operation": acp_operation_from_error(&error_text),
            "stage": "startup",
            "threadId": session.thread_id,
            "providerSessionId": session.provider_session_id,
            "providerTurnId": provider_turn_id,
            "runtimeGeneration": null,
            "processId": null,
            "error": error_text
        }),
    )
}

fn runtime_context_error(
    mut error: PedelecError,
    provider: AcpProviderKind,
    session: &PersistentProviderSessionIntent,
    provider_turn_id: Option<&str>,
    stage: &str,
) -> PedelecError {
    let mut details = error
        .details
        .take()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    details["provider"] = json!(provider.code());
    details["operation"] = json!("runtime");
    details["stage"] = json!(stage);
    details["threadId"] = json!(session.thread_id);
    details["providerSessionId"] = json!(session.provider_session_id);
    details["providerTurnId"] = json!(provider_turn_id);
    details["runtimeGeneration"] = Value::Null;
    details["processId"] = Value::Null;
    error.details = Some(details);
    error
}

fn acp_operation_from_error(error: &str) -> &'static str {
    [
        "authenticate",
        "initialize",
        "session/load",
        "session/new",
        "spawn",
        "event-worker",
    ]
    .into_iter()
    .find(|operation| error.contains(operation))
    .unwrap_or("runtime")
}
fn acp_error(
    error: AcpRuntimeError,
    controller: Option<&AcpController>,
    thread: Option<&str>,
    provider_session_id: Option<&str>,
    provider_turn_id: Option<&str>,
    stage: &str,
    provider: AcpProviderKind,
) -> PedelecError {
    let operation = match &error {
        AcpRuntimeError::RuntimeStart { operation, .. }
        | AcpRuntimeError::RuntimeDisconnected { operation, .. }
        | AcpRuntimeError::Protocol { operation, .. }
        | AcpRuntimeError::Request { operation, .. } => operation.clone(),
    };
    let code = match error {
        AcpRuntimeError::RuntimeStart { .. } => error_codes::PROVIDER_RUNTIME_START_FAILED,
        AcpRuntimeError::RuntimeDisconnected { .. } => error_codes::PROVIDER_RUNTIME_DISCONNECTED,
        AcpRuntimeError::Protocol { .. } => error_codes::PROVIDER_PROTOCOL_ERROR,
        AcpRuntimeError::Request { .. } => error_codes::PROVIDER_REQUEST_FAILED,
    };
    let message = bounded_text(&error.to_string());
    let request_details = match &error {
        AcpRuntimeError::Request { details, .. } => details.clone(),
        _ => None,
    };
    let mut details = json!({
        "provider": provider.code(),
        "operation": operation,
        "threadId": thread,
        "providerSessionId": provider_session_id,
        "providerTurnId": provider_turn_id,
        "stage": stage,
        "runtimeGeneration": controller.map(AcpController::generation),
        "processId": controller.map(AcpController::process_id),
    });
    if let Some(request_details) = request_details {
        details["request"] = request_details;
    }
    PedelecError::with_details(code, message, details)
}

fn runtime_end_error(
    session: &pedelec_core::PersistentProviderEndIntent,
    provider: AcpProviderKind,
    reason: &str,
    controller: Option<&AcpController>,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_DISCONNECTED,
        format!(
            "{} ACP runtime could not complete thread end",
            provider.label()
        ),
        json!({
            "provider": provider.code(),
            "operation": "end",
            "stage": "end",
            "threadId": session.thread_id,
            "providerSessionId": session.provider_session_id,
            "providerTurnId": session.active_provider_turn_id,
            "reason": bounded_text(reason),
            "runtimeGeneration": controller.map(AcpController::generation),
            "processId": controller.map(AcpController::process_id),
        }),
    )
}
fn mark_stopped(
    runtime: &SharedCoreRuntime,
    stopped: &Mutex<HashSet<u64>>,
    controller: &AcpController,
    provider: AcpProviderKind,
    reason: &str,
) {
    if stopped
        .lock()
        .map(|mut values| values.insert(controller.generation()))
        .unwrap_or(true)
    {
        record(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStopped {
                provider: provider.provider_code(),
                runtime_generation: controller.generation(),
                process_id: controller.process_id(),
                reason: reason.into(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use chrono::Utc;
    use pedelec_core::{
        EffortLevel, PendingProviderOperation, PendingProviderOperationKind,
        PersistentApprovalPolicy, PersistentProviderEndIntent, PersistentProviderTurnIntent,
        PersistentSandboxPolicy, ProviderSessionState, ThreadState, ThreadStatus, WorkspaceKind,
    };
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    const TEST_PNG_BASE64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jFZ0AAAAASUVORK5CYII=";

    fn test_png_bytes() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(TEST_PNG_BASE64)
            .unwrap()
    }

    #[test]
    fn opencode_usage_parser_accepts_only_native_prompt_total() {
        assert_eq!(
            opencode_prompt_total_tokens(&json!({
                "inputTokens": 4,
                "outputTokens": 3,
                "thoughtTokens": 1,
                "cachedReadTokens": 2,
                "cachedWriteTokens": 0,
                "totalTokens": 10,
            })),
            Some(10)
        );
        assert_eq!(
            opencode_prompt_total_tokens(&json!({ "used": 4, "size": 100 })),
            None
        );
        assert_eq!(
            opencode_prompt_total_tokens(&json!({ "totalTokens": "10" })),
            None
        );
    }

    #[test]
    fn opencode_prompt_usage_is_idempotent_for_one_operation() {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let thread_id = "thread-open";
        let workspace = tempdir().unwrap().path().to_path_buf();
        let now = Utc::now();
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-thread-open";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::OpenCode,
                effort_level: Some(EffortLevel::Default),
                effort_args: Vec::new(),
                skills: Vec::new(),
                status: ThreadStatus::Running,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: Some("acp-session-1".into()),
                active_provider_turn_id: Some("local-turn-1".into()),
            },
        );
        drop(runtime_guard);
        runtime.lock().unwrap().pending_provider_operations.insert(
            thread_id.into(),
            PendingProviderOperation {
                operation_id: "operation-1".into(),
                kind: PendingProviderOperationKind::UserTurn,
                started_at: now,
            },
        );

        let usage = json!({ "totalTokens": 10 });
        record_opencode_prompt_usage(&runtime, thread_id, &usage);
        record_opencode_prompt_usage(&runtime, thread_id, &usage);

        assert_eq!(
            runtime.lock().unwrap().session_total_tokens(thread_id),
            Some(10)
        );
    }

    #[test]
    fn config_preserves_existing_instructions_and_adds_session_entry() {
        let root: Value = serde_json::from_str(
            &merge_opencode_acp_config(Some(r#"{"instructions":["AGENTS.md"]}"#)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            root["instructions"],
            json!(["AGENTS.md", OPENCODE_INSTRUCTION_PATH])
        );
    }

    #[test]
    fn config_deduplicates_the_pedelec_instruction_entry() {
        let root: Value = serde_json::from_str(
            &merge_opencode_acp_config(Some(
                r#"{"instructions":[".pedelec-runtime/opencode-session-instructions.md","AGENTS.md"]}"#,
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            root["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| value.as_str() == Some(OPENCODE_INSTRUCTION_PATH))
                .count(),
            1
        );
        assert_eq!(root["instructions"][1], "AGENTS.md");
    }

    #[test]
    fn leading_slash_prompt_shaping_is_opencode_only_and_preserves_raw_text() {
        for (message, expected) in [
            ("/foo", "- /foo"),
            ("/upl_123.jpg 裡有什麼?", "- /upl_123.jpg 裡有什麼?"),
            ("   /foo", "-    /foo"),
            ("\t/foo", "- \t/foo"),
            ("\n/foo", "- \n/foo"),
        ] {
            assert_eq!(
                shape_user_prompt_for_provider(AcpProviderKind::OpenCode, message),
                expected
            );
        }

        for message in [
            "foo",
            "foo /bar",
            "- /foo",
            "> /foo",
            "https://example.com/foo",
        ] {
            assert_eq!(
                shape_user_prompt_for_provider(AcpProviderKind::OpenCode, message),
                message
            );
        }
        assert_eq!(
            shape_user_prompt_for_provider(AcpProviderKind::Cursor, "/foo"),
            "/foo"
        );
    }

    #[test]
    fn workspace_permission_resolution_is_session_scoped_and_rejects_detached_threads() {
        let temp = tempdir().unwrap();
        let workspace_a = temp.path().join("workspace-a");
        let workspace_b = temp.path().join("workspace-b");
        fs::create_dir_all(&workspace_a).unwrap();
        fs::create_dir_all(&workspace_b).unwrap();
        let workspaces = Mutex::new(HashMap::from([
            ("thread-a".to_string(), workspace_a.clone()),
            ("thread-b".to_string(), workspace_b.clone()),
        ]));
        let request = |thread: &str, path: Value| AcpPermissionRequest {
            pedelec_thread_id: thread.into(),
            provider_session_id: format!("session-{thread}"),
            tool_call: json!({ "path": path }),
            options: json!([]),
        };

        assert_eq!(
            resolve_workspace_permission(
                &workspaces,
                &request("thread-a", json!(workspace_a.join("src/lib.rs")))
            ),
            AcpPermissionDecision::AllowOnce
        );
        assert_eq!(
            resolve_workspace_permission(
                &workspaces,
                &request("thread-a", json!(workspace_b.join("src/lib.rs")))
            ),
            AcpPermissionDecision::RejectOnce
        );
        assert_eq!(
            resolve_workspace_permission(&workspaces, &request("thread-b", json!("src/lib.rs"))),
            AcpPermissionDecision::AllowOnce
        );
        assert_eq!(
            resolve_workspace_permission(&workspaces, &request("thread-a", json!("../../escape"))),
            AcpPermissionDecision::RejectOnce
        );
        assert_eq!(
            resolve_workspace_permission(&workspaces, &request("detached", json!("src/lib.rs"))),
            AcpPermissionDecision::RejectOnce
        );
    }

    #[test]
    fn workspace_permission_allows_pedelec_helpers_from_provider_tool_calls() {
        let workspace = tempdir().unwrap();
        let workspaces = Mutex::new(HashMap::from([(
            "thread-a".to_string(),
            workspace.path().to_path_buf(),
        )]));
        let request = |thread: &str, tool_call: Value| AcpPermissionRequest {
            pedelec_thread_id: thread.into(),
            provider_session_id: "session-a".into(),
            tool_call,
            options: json!([]),
        };

        let cursor_stdin = json!({
            "kind": "execute",
            "status": "pending",
            "title": "`@'\nimport { writeNote } from \"memory-manager\";\nawait writeNote(\"hello\");\n'@ | pedelec-deno --thread-id t000004 run -`",
            "content": [{
                "type": "content",
                "content": {
                    "type": "text",
                    "text": "Not in allowlist: @' ... '@, pedelec-deno"
                }
            }]
        });
        assert_eq!(
            resolve_workspace_permission(&workspaces, &request("thread-a", cursor_stdin)),
            AcpPermissionDecision::AllowOnce
        );

        for tool_call in [
            json!({ "kind": "execute", "rawInput": { "command": "pedelec-deno --thread-id t000004 run scripts/task.ts" } }),
            json!({ "kind": "execute", "command": "pedelec-deno --thread-id t000004 run -" }),
            json!({ "kind": "execute", "title": "pedelec-cli --thread-id t000004 tool-spec ask_user" }),
            json!({ "kind": "execute", "command": "pedelec-cli --thread-id t000004 tool-call ask_user '{}'" }),
            json!({ "kind": "execute", "content": [{ "content": { "text": "Not in allowlist: pedelec-cli" } }] }),
        ] {
            assert_eq!(
                resolve_workspace_permission(&workspaces, &request("thread-a", tool_call)),
                AcpPermissionDecision::AllowOnce
            );
        }

        for tool_call in [
            json!({ "kind": "execute", "command": "some-random-command --foo" }),
            json!({ "kind": "execute", "command": "other-pedelec-deno --foo" }),
            json!({ "kind": "execute", "command": "pedelec-deno-extra --foo" }),
            json!({ "kind": "read", "title": "pedelec-deno" }),
            json!({ "kind": "execute", "message": "pedelec-deno" }),
        ] {
            assert_eq!(
                resolve_workspace_permission(&workspaces, &request("thread-a", tool_call)),
                AcpPermissionDecision::RejectOnce
            );
        }
        assert_eq!(
            resolve_workspace_permission(
                &workspaces,
                &request(
                    "detached",
                    json!({ "kind": "execute", "command": "pedelec-deno run -" })
                )
            ),
            AcpPermissionDecision::RejectOnce
        );
    }

    #[test]
    fn provider_disconnect_only_fails_threads_for_that_acp_provider() {
        for failed_provider in [AcpProviderKind::OpenCode, AcpProviderKind::Cursor] {
            let temp = tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
            {
                let mut core = runtime.lock().unwrap();
                for provider in [
                    ProviderCode::OpenCode,
                    ProviderCode::Cursor,
                    ProviderCode::Codex,
                ] {
                    let id = format!("thread-{}", format!("{provider:?}").to_lowercase());
                    let workspace_id = format!("workspace-{id}");
                    core.register_workspace_for_test(
                        &workspace_id,
                        workspace.join(&id),
                        WorkspaceKind::Custom,
                    )
                    .unwrap();
                    core.thread_manager.insert_thread(
                        ThreadState {
                            thread_id: id.clone(),
                            workspace_id,
                            provider: provider.clone(),
                            effort_level: Some(EffortLevel::Default),
                            effort_args: vec![],
                            skills: vec![],
                            status: ThreadStatus::Running,
                            created_at: Utc::now(),
                            updated_at: Utc::now(),
                            sdk_origin: None,
                        },
                        ProviderSessionState {
                            provider_session_id: Some(format!("session-{id}")),
                            active_provider_turn_id: Some(format!("turn-{id}")),
                        },
                    );
                    core.pending_provider_operations.insert(
                        id,
                        PendingProviderOperation {
                            operation_id: "test-operation".into(),
                            kind: PendingProviderOperationKind::UserTurn,
                            started_at: Utc::now(),
                        },
                    );
                }
            }
            let log = temp.path().join("crash-scope.jsonl");
            let launch = pedelec_runtime::AcpLaunchConfig::new(
                "scope-test",
                fake_opencode_program(temp.path()),
                temp.path(),
            )
            .with_env("FAKE_ACP_LOG", log.to_string_lossy().into_owned())
            .with_env(
                "FAKE_ACP_WORKSPACE",
                workspace.to_string_lossy().into_owned(),
            )
            .with_env("FAKE_ACP_LOAD", "true");
            let controller = AcpController::spawn(
                launch,
                Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
            )
            .unwrap();
            handle_event(
                &runtime,
                &controller,
                failed_provider,
                AcpRuntimeEvent::Disconnected {
                    generation: controller.generation(),
                    pid: controller.process_id(),
                    attachments: vec![],
                    reason: pedelec_runtime::RpcDisconnectReason::UncleanEof,
                },
            );
            let failed_code = failed_provider.provider_code();
            let core = runtime.lock().unwrap();
            for provider in [
                ProviderCode::OpenCode,
                ProviderCode::Cursor,
                ProviderCode::Codex,
            ] {
                let id = format!("thread-{}", format!("{provider:?}").to_lowercase());
                let expected = if provider == failed_code {
                    ThreadStatus::Error
                } else {
                    ThreadStatus::Running
                };
                assert_eq!(
                    core.thread_status(&id),
                    Some(expected),
                    "provider={provider:?}"
                );
            }
            drop(core);
            let _ = controller.shutdown();
        }
    }

    #[test]
    fn model_mapping_uses_category_instead_of_assuming_id() {
        let options = json!([{"id":"provider-model-picker","category":"model","options":[{"value":"openai/gpt-5","name":"GPT-5"}]}]);
        let option = find_config_option(&options, "model").unwrap();
        assert_eq!(option["id"], "provider-model-picker");
        assert!(validate_select_value_for_provider(
            option,
            "openai/gpt-5",
            AcpProviderKind::OpenCode
        )
        .is_ok());
        assert!(validate_select_value_for_provider(
            option,
            "missing/model",
            AcpProviderKind::OpenCode
        )
        .is_err());
    }

    fn run_cursor_config_admission(
        model: &str,
        settings: pedelec_core::CursorSessionSettings,
        config_variant: Option<&str>,
    ) -> (Result<(), PedelecError>, Vec<Value>, bool) {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("cursor-config-frames.jsonl");
        let resume = config_variant == Some("resume");
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        {
            let mut core = runtime.lock().unwrap();
            core.register_workspace_for_test(
                "workspace-cursor-config",
                &workspace,
                WorkspaceKind::Custom,
            )
            .unwrap();
            core.thread_manager.insert_thread(
                ThreadState {
                    thread_id: "thread-cursor-config".into(),
                    workspace_id: "workspace-cursor-config".into(),
                    provider: ProviderCode::Cursor,
                    effort_level: Some(EffortLevel::Default),
                    effort_args: vec!["--model".into(), model.into()],
                    skills: vec![],
                    status: ThreadStatus::Running,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    sdk_origin: None,
                },
                ProviderSessionState {
                    provider_session_id: None,
                    active_provider_turn_id: None,
                },
            );
            core.pending_provider_operations.insert(
                "thread-cursor-config".into(),
                PendingProviderOperation {
                    operation_id: "cursor-config-test".into(),
                    kind: PendingProviderOperationKind::Prepare,
                    started_at: Utc::now(),
                },
            );
        }
        let owner = ProviderRuntimeOwner::new();
        let mut dispatcher = CursorRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_cursor_program(temp.path()))
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_AUTH", "cursor_login")
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_PARAMETERIZED_CURSOR", "true")
            .with_env_for_test("FAKE_ACP_LOAD", if resume { "true" } else { "false" })
            .with_env_for_test(
                "FAKE_ACP_LOAD_SESSION_ID",
                if resume {
                    "persisted-cursor-session"
                } else {
                    ""
                },
            );
        if let Some(config_variant) = config_variant.filter(|variant| *variant != "resume") {
            dispatcher = dispatcher.with_env_for_test("FAKE_ACP_CURSOR_CONFIG", config_variant);
        }
        let mut session = session_intent(
            &workspace,
            resume.then(|| "persisted-cursor-session".to_string()),
        );
        session.thread_id = "thread-cursor-config".into();
        session.provider = ProviderCode::Cursor;
        session.model = Some(model.to_string());
        session.cursor_settings = Some(settings);
        let result = dispatcher.dispatch(PersistentRuntimeOperation::EnsureSession { session });
        let frames = fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let ready = runtime
            .lock()
            .unwrap()
            .thread_manager
            .provider_session_state("thread-cursor-config")
            .and_then(|state| state.provider_session_id.as_ref())
            .is_some();
        let _ = owner.shutdown();
        (result, frames, ready)
    }

    #[test]
    fn cursor_parameterized_settings_apply_in_order_using_refreshed_options() {
        for (model, settings, expected) in [
            (
                "composer-2.5",
                pedelec_core::CursorSessionSettings {
                    effort: None,
                    fast: Some(false),
                },
                vec![("model-picker", "composer-2.5"), ("fast", "false")],
            ),
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings {
                    effort: Some("high".into()),
                    fast: Some(false),
                },
                vec![
                    ("model-picker", "grok-4.7"),
                    ("reasoning-picker", "high"),
                    ("fast", "false"),
                ],
            ),
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings {
                    effort: Some("xhigh".into()),
                    fast: Some(false),
                },
                vec![
                    ("model-picker", "grok-4.7"),
                    ("reasoning-picker", "xhigh"),
                    ("fast", "false"),
                ],
            ),
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings::default(),
                vec![("model-picker", "grok-4.7")],
            ),
        ] {
            let (result, frames, ready) = run_cursor_config_admission(model, settings, None);
            result.unwrap();
            assert!(ready, "Cursor session did not reach ready for {model}");
            let initialize = frames
                .iter()
                .find(|frame| frame["method"] == "initialize")
                .expect("initialize request should be captured");
            assert_eq!(
                initialize["params"]["clientCapabilities"]["_meta"]["parameterizedModelPicker"],
                true
            );
            let applied = frames
                .iter()
                .filter(|frame| frame["method"] == "session/set_config_option")
                .map(|frame| {
                    (
                        frame["params"]["configId"].as_str().unwrap(),
                        frame["params"]["value"].as_str().unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(applied, expected);
            assert!(!frames.iter().any(|frame| {
                frame["method"] == "session/set_config_option"
                    && frame["params"]["value"] == "grok-4.7-xhigh"
            }));
        }

        let (result, frames, ready) = run_cursor_config_admission(
            "grok-4.7",
            pedelec_core::CursorSessionSettings {
                effort: Some("xhigh".into()),
                fast: Some(false),
            },
            Some("resume"),
        );
        result.unwrap();
        assert!(ready, "resumed Cursor session did not reach ready");
        assert!(frames.iter().any(|frame| frame["method"] == "session/load"));
        let settings = frames
            .iter()
            .filter(|frame| frame["method"] == "session/set_config_option")
            .map(|frame| {
                (
                    frame["params"]["configId"].as_str().unwrap(),
                    frame["params"]["value"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            settings,
            vec![
                ("model-picker", "grok-4.7"),
                ("reasoning-picker", "xhigh"),
                ("fast", "false"),
            ]
        );
    }

    #[test]
    fn cursor_parameterized_config_rejects_unadvertised_requested_settings() {
        let cases = [
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings::default(),
                Some("unsupported_model"),
                "model",
                error_codes::PROVIDER_REQUEST_FAILED,
            ),
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings {
                    effort: Some("xhigh".into()),
                    fast: None,
                },
                Some("unsupported_effort"),
                "effort",
                error_codes::PROVIDER_REQUEST_FAILED,
            ),
            (
                "grok-4.7",
                pedelec_core::CursorSessionSettings {
                    effort: Some("high".into()),
                    fast: None,
                },
                Some("missing_effort"),
                "effort",
                error_codes::PROVIDER_PROTOCOL_ERROR,
            ),
            (
                "composer-2.5",
                pedelec_core::CursorSessionSettings {
                    effort: None,
                    fast: Some(false),
                },
                Some("missing_fast"),
                "fast",
                error_codes::PROVIDER_PROTOCOL_ERROR,
            ),
            (
                "composer-2.5",
                pedelec_core::CursorSessionSettings {
                    effort: None,
                    fast: Some(false),
                },
                Some("unsupported_fast"),
                "fast",
                error_codes::PROVIDER_REQUEST_FAILED,
            ),
        ];
        for (model, settings, variant, stage, code) in cases {
            let (result, _, ready) = run_cursor_config_admission(model, settings, variant);
            let error = result.unwrap_err();
            assert_eq!(error.code, code);
            let details = error.details.as_ref().unwrap();
            assert_eq!(details["stage"], stage);
            assert!(details["requestedValue"].is_string());
            assert!(details[stage].is_string());
            assert!(!ready);
        }
    }

    #[test]
    fn cursor_dispatcher_authenticates_maps_mode_and_bootstraps_once() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("cursor-frames.jsonl");
        let fake_program = fake_cursor_program(temp.path());
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-thread-cursor";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: "thread-cursor".into(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::Cursor,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec!["--model".into(), "fake/selected".into()],
                skills: vec![],
                status: ThreadStatus::Running,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        drop(runtime_guard);
        runtime.lock().unwrap().pending_provider_operations.insert(
            "thread-cursor".into(),
            PendingProviderOperation {
                operation_id: "test-operation".into(),
                kind: PendingProviderOperationKind::Prepare,
                started_at: Utc::now(),
            },
        );
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = CursorRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_program)
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_AUTH", "cursor_login")
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true");
        let mut session = session_intent(&workspace, None);
        session.thread_id = "thread-cursor".into();
        session.provider = ProviderCode::Cursor;
        dispatcher
            .dispatch(PersistentRuntimeOperation::EnsureSession {
                session: session.clone(),
            })
            .unwrap();
        let provider_id = runtime
            .lock()
            .unwrap()
            .thread_manager
            .provider_session_state("thread-cursor")
            .unwrap()
            .provider_session_id
            .clone()
            .unwrap();

        for (local_turn_id, message) in [
            ("cursor-first", "first task"),
            ("cursor-second", "second task"),
            ("cursor-leading-slash", "/foo"),
        ] {
            {
                let mut core = runtime.lock().unwrap();
                core.thread_manager
                    .thread_mut("thread-cursor")
                    .unwrap()
                    .status = ThreadStatus::Running;
                core.thread_manager
                    .provider_session_state_mut("thread-cursor")
                    .unwrap()
                    .active_provider_turn_id = Some(local_turn_id.into());
                core.pending_provider_operations.insert(
                    "thread-cursor".into(),
                    PendingProviderOperation {
                        operation_id: "test-operation".into(),
                        kind: PendingProviderOperationKind::UserTurn,
                        started_at: Utc::now(),
                    },
                );
            }
            dispatcher
                .dispatch(PersistentRuntimeOperation::StartTurn {
                    turn: PersistentProviderTurnIntent {
                        thread_id: "thread-cursor".into(),
                        local_turn_id: local_turn_id.into(),
                        provider_session_id: Some(provider_id.clone()),
                        message: message.into(),
                        session: {
                            let mut value = session.clone();
                            value.provider_session_id = Some(provider_id.clone());
                            value
                        },
                    },
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline
                && runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread("thread-cursor")
                    .unwrap()
                    .status
                    != ThreadStatus::Idle
            {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread("thread-cursor")
                    .unwrap()
                    .status,
                ThreadStatus::Idle
            );
        }

        let frames = fs::read_to_string(log).unwrap();
        assert!(frames.contains("\"method\":\"authenticate\""));
        let initialize: Value = frames
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .find(|frame: &Value| frame["method"] == "initialize")
            .expect("Cursor initialize request should be captured");
        assert_eq!(
            initialize["params"]["clientCapabilities"]["_meta"]["parameterizedModelPicker"],
            true
        );
        assert!(frames.contains("\"method\":\"session/set_mode\""));
        assert!(frames.contains("\"method\":\"session/set_config_option\""));
        let prompts = frames
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["method"] == "session/prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0]["params"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[Pedelec Host Bootstrap]"));
        assert!(prompts[0]["params"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .contains("first task"));
        assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "second task");
        assert_eq!(prompts[2]["params"]["prompt"][0]["text"], "/foo");
        assert!(!frames.contains("PEDELEC_PREPARED"));
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .session_total_tokens("thread-cursor"),
            None
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn opencode_and_cursor_restart_like_resume_load_once_and_keep_user_prompt_raw() {
        for provider in [AcpProviderKind::OpenCode, AcpProviderKind::Cursor] {
            let temp = tempdir().unwrap();
            let workspace = temp.path().join(format!("workspace-{}", provider.code()));
            fs::create_dir_all(&workspace).unwrap();
            let log = temp
                .path()
                .join(format!("{}-resume.jsonl", provider.code()));
            let fake_program = if provider.is_cursor() {
                fake_cursor_program(temp.path())
            } else {
                fake_opencode_program(temp.path())
            };
            let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
            let thread_id = format!("thread-{}-resume", provider.code());
            {
                let mut core = runtime.lock().unwrap();
                let workspace_id = format!("workspace-{thread_id}");
                core.register_workspace_for_test(&workspace_id, &workspace, WorkspaceKind::Custom)
                    .unwrap();
                core.thread_manager.insert_thread(
                    ThreadState {
                        thread_id: thread_id.clone(),
                        provider: provider.provider_code(),
                        effort_level: Some(EffortLevel::Default),
                        effort_args: vec!["--model".into(), "fake/selected".into()],
                        workspace_id,
                        skills: vec![],
                        status: ThreadStatus::Running,
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                        sdk_origin: None,
                    },
                    ProviderSessionState {
                        provider_session_id: Some("persisted-session".into()),
                        active_provider_turn_id: None,
                    },
                );
                core.pending_provider_operations.insert(
                    thread_id.clone(),
                    PendingProviderOperation {
                        operation_id: "test-operation".into(),
                        kind: PendingProviderOperationKind::Prepare,
                        started_at: Utc::now(),
                    },
                );
            }
            let owner = ProviderRuntimeOwner::new();
            let mut dispatcher = AcpRuntimeDispatcher::new_for_provider(
                provider,
                owner.clone(),
                Arc::clone(&runtime),
            )
            .with_program_for_test(fake_program)
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true");
            if provider.is_cursor() {
                dispatcher = dispatcher.with_env_for_test("FAKE_ACP_AUTH", "cursor_login");
            }
            let mut session = session_intent(&workspace, Some("persisted-session".into()));
            session.thread_id = thread_id.clone();
            session.provider = provider.provider_code();
            dispatcher
                .dispatch(PersistentRuntimeOperation::EnsureSession {
                    session: session.clone(),
                })
                .unwrap();
            {
                let mut core = runtime.lock().unwrap();
                core.thread_manager.thread_mut(&thread_id).unwrap().status = ThreadStatus::Running;
                core.thread_manager
                    .provider_session_state_mut(&thread_id)
                    .unwrap()
                    .active_provider_turn_id = Some("resume-turn".into());
                core.pending_provider_operations.insert(
                    thread_id.clone(),
                    PendingProviderOperation {
                        operation_id: "test-operation".into(),
                        kind: PendingProviderOperationKind::UserTurn,
                        started_at: Utc::now(),
                    },
                );
            }
            dispatcher
                .dispatch(PersistentRuntimeOperation::StartTurn {
                    turn: PersistentProviderTurnIntent {
                        thread_id: thread_id.clone(),
                        local_turn_id: "resume-turn".into(),
                        provider_session_id: Some("persisted-session".into()),
                        message: "resumed actual task".into(),
                        session,
                    },
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline
                && runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread(&thread_id)
                    .unwrap()
                    .status
                    != ThreadStatus::Idle
            {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread(&thread_id)
                    .unwrap()
                    .status,
                ThreadStatus::Idle
            );
            let frames = fs::read_to_string(&log).unwrap();
            let parsed = frames
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            assert_eq!(
                parsed
                    .iter()
                    .filter(|frame| frame["method"] == "session/load")
                    .count(),
                1,
                "provider={provider:?}"
            );
            assert!(!parsed.iter().any(|frame| frame["method"] == "session/new"));
            let prompt = parsed
                .iter()
                .find(|frame| frame["method"] == "session/prompt")
                .unwrap();
            assert_eq!(
                prompt["params"]["prompt"][0]["text"], "resumed actual task",
                "provider={provider:?}"
            );
            assert!(!prompt.to_string().contains("[Pedelec Host Bootstrap]"));
            let _ = owner.shutdown();
        }
    }

    #[test]
    fn cursor_extension_requests_have_provider_shaped_negative_responses() {
        let handler = CursorExtensionRequestHandler;
        let ask = handler
            .handle(&RpcServerRequest {
                id: pedelec_runtime::RpcId::Number(1),
                method: "cursor/ask_question".into(),
                params: json!({}),
            })
            .unwrap();
        assert_eq!(ask["outcome"]["outcome"], "skipped");
        let plan = handler
            .handle(&RpcServerRequest {
                id: pedelec_runtime::RpcId::Number(2),
                method: "cursor/create_plan".into(),
                params: json!({}),
            })
            .unwrap();
        assert_eq!(plan["outcome"]["outcome"], "rejected");
        assert!(handler
            .handle(&RpcServerRequest {
                id: pedelec_runtime::RpcId::Number(3),
                method: "cursor/unknown".into(),
                params: json!({}),
            })
            .is_none());
    }

    #[test]
    fn cursor_generated_image_imports_only_file_path_and_preserves_call_identity() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let generated = temp.path().join("generated.png");
        let reference = temp.path().join("reference.png");
        fs::write(&generated, test_png_bytes()).unwrap();
        fs::write(&reference, test_png_bytes()).unwrap();
        let runtime = active_artifact_runtime(
            ProviderCode::Cursor,
            &workspace,
            "thread-cursor-artifact",
            "turn-cursor-artifact",
        );
        let events = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: "thread-cursor-artifact".into(),
            })
            .unwrap();

        handle_provider_notification(
            &runtime,
            AcpProviderKind::Cursor,
            "cursor/generate_image",
            &json!({
                "toolCallId":"cursor-tool-42",
                "description":"Create a small icon",
                "filePath":generated,
                "referenceImagePaths":[reference]
            }),
            Some("thread-cursor-artifact"),
            Some("turn-cursor-artifact"),
        );

        let artifact_event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        let artifact = match artifact_event {
            pedelec_core::ThreadEvent::ProviderArtifact { artifact, .. } => artifact,
            other => panic!("expected ProviderArtifact, got {other:?}"),
        };
        assert_eq!(artifact.provider, ProviderCode::Cursor);
        assert_eq!(artifact.source, ProviderArtifactSource::ImageGeneration);
        assert_eq!(artifact.tool_call_id.as_deref(), Some("cursor-tool-42"));
        assert_eq!(
            artifact.provider_artifact_id.as_deref(),
            Some("cursor-tool-42")
        );
        let copied = workspace.join(".pedelec-runtime").join("assets").join(
            artifact
                .path
                .trim_start_matches('/')
                .split('/')
                .collect::<PathBuf>(),
        );
        assert_eq!(fs::read(copied).unwrap(), test_png_bytes());
        let artifact_dir = workspace
            .join(".pedelec-runtime")
            .join("assets")
            .join("provider-artifacts")
            .join("cursor")
            .join("thread-cursor-artifact");
        assert_eq!(fs::read_dir(artifact_dir).unwrap().count(), 1);
        assert!(reference.exists());
    }

    #[test]
    fn cursor_generate_image_without_session_id_materializes_through_the_acp_turn() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let generated = temp.path().join("generated.png");
        fs::write(&generated, test_png_bytes()).unwrap();
        let log = temp.path().join("cursor-image-frames.jsonl");
        let thread_id = "thread-cursor-live-image";
        let turn_id = "turn-cursor-live-image";
        let runtime = active_artifact_runtime(ProviderCode::Cursor, &workspace, thread_id, turn_id);
        let events = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = CursorRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_cursor_program(temp.path()))
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("CURSOR_API_KEY", "test-token")
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true")
            .with_env_for_test("FAKE_ACP_GENERATED_IMAGE", generated.to_string_lossy());
        let mut session = session_intent(&workspace, Some("provider-session".into()));
        session.thread_id = thread_id.into();
        session.provider = ProviderCode::Cursor;
        dispatcher
            .dispatch(PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: thread_id.into(),
                    local_turn_id: turn_id.into(),
                    provider_session_id: Some("provider-session".into()),
                    message: "cursor-generate-image".into(),
                    session,
                },
            })
            .unwrap();

        let mut seen_events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(remaining).unwrap();
            let completed = matches!(event, pedelec_core::ThreadEvent::OperationCompleted { .. });
            seen_events.push(event);
            if completed {
                break;
            }
        }
        let artifact_index = seen_events
            .iter()
            .position(|event| matches!(event, pedelec_core::ThreadEvent::ProviderArtifact { .. }))
            .expect("Cursor should materialize the generated image");
        let artifact = match &seen_events[artifact_index] {
            pedelec_core::ThreadEvent::ProviderArtifact { artifact, .. } => artifact,
            _ => unreachable!(),
        };
        assert_eq!(artifact.provider, ProviderCode::Cursor);
        assert_eq!(artifact.source, ProviderArtifactSource::ImageGeneration);
        assert_eq!(artifact.tool_call_id.as_deref(), Some("call-image-1"));
        let copied = workspace.join(".pedelec-runtime").join("assets").join(
            artifact
                .path
                .trim_start_matches('/')
                .split('/')
                .collect::<PathBuf>(),
        );
        assert_eq!(fs::read(copied).unwrap(), test_png_bytes());
        let _ = owner.shutdown();
    }

    #[test]
    fn cursor_generated_image_without_file_path_is_non_fatal() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let runtime = active_artifact_runtime(
            ProviderCode::Cursor,
            &workspace,
            "thread-cursor-no-image",
            "turn-cursor-no-image",
        );
        handle_provider_notification(
            &runtime,
            AcpProviderKind::Cursor,
            "cursor/generate_image",
            &json!({"toolCallId":"cursor-tool-no-path","description":"No generated file"}),
            Some("thread-cursor-no-image"),
            Some("turn-cursor-no-image"),
        );
        let core = runtime.lock().unwrap();
        assert_eq!(
            core.thread_status("thread-cursor-no-image"),
            Some(ThreadStatus::Running)
        );
        assert!(!workspace
            .join(".pedelec-runtime/assets/provider-artifacts/cursor/thread-cursor-no-image")
            .exists());
    }

    #[test]
    fn cursor_invalid_generated_paths_fail_the_active_operation() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let missing = temp.path().join("missing.png");
        let directory = temp.path().join("directory.png");
        fs::create_dir_all(&directory).unwrap();
        let mut paths = vec![missing, directory];
        let symlink_target = temp.path().join("target.png");
        let symlink = temp.path().join("link.png");
        fs::write(&symlink_target, b"target").unwrap();
        #[cfg(unix)]
        let symlink_created = std::os::unix::fs::symlink(&symlink_target, &symlink).is_ok();
        #[cfg(windows)]
        let symlink_created = std::os::windows::fs::symlink_file(&symlink_target, &symlink).is_ok();
        if symlink_created {
            paths.push(symlink);
        }

        for (index, path) in paths.into_iter().enumerate() {
            let thread_id = format!("thread-cursor-invalid-{index}");
            let turn_id = format!("turn-cursor-invalid-{index}");
            let runtime =
                active_artifact_runtime(ProviderCode::Cursor, &workspace, &thread_id, &turn_id);
            handle_provider_notification(
                &runtime,
                AcpProviderKind::Cursor,
                "cursor/generate_image",
                &json!({"toolCallId":"bad-path","filePath":path}),
                Some(&thread_id),
                Some(&turn_id),
            );
            let core = runtime.lock().unwrap();
            assert_eq!(core.thread_status(&thread_id), Some(ThreadStatus::Error));
            assert!(core.current_operation_id(&thread_id).is_none());
        }
    }

    #[test]
    fn open_code_does_not_interpret_cursor_generated_image_notifications() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let generated = temp.path().join("generated.png");
        fs::write(&generated, test_png_bytes()).unwrap();
        let runtime = active_artifact_runtime(
            ProviderCode::OpenCode,
            &workspace,
            "thread-open-no-cursor-extension",
            "turn-open-no-cursor-extension",
        );
        handle_provider_notification(
            &runtime,
            AcpProviderKind::OpenCode,
            "cursor/generate_image",
            &json!({"toolCallId":"not-cursor","filePath":generated}),
            Some("thread-open-no-cursor-extension"),
            Some("turn-open-no-cursor-extension"),
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread-open-no-cursor-extension"),
            Some(ThreadStatus::Running)
        );
        assert!(!workspace
            .join(".pedelec-runtime/assets/provider-artifacts")
            .exists());
    }

    #[test]
    fn opencode_inline_image_is_materialized_before_the_turn_completes() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("inline-image-frames.jsonl");
        let runtime = active_artifact_runtime(
            ProviderCode::OpenCode,
            &workspace,
            "thread-open-inline-image",
            "turn-open-inline-image",
        );
        let events = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: "thread-open-inline-image".into(),
            })
            .unwrap();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = OpenCodeRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_opencode_program(temp.path()))
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true");
        let mut session = session_intent(&workspace, Some("provider-session".into()));
        session.thread_id = "thread-open-inline-image".into();
        dispatcher
            .dispatch(PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: "thread-open-inline-image".into(),
                    local_turn_id: "turn-open-inline-image".into(),
                    provider_session_id: Some("provider-session".into()),
                    message: "inline-image".into(),
                    session,
                },
            })
            .unwrap();

        let mut seen_events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(remaining).unwrap();
            let completed = matches!(event, pedelec_core::ThreadEvent::OperationCompleted { .. });
            seen_events.push(event);
            if completed {
                break;
            }
        }
        let artifact_index = seen_events
            .iter()
            .position(|event| matches!(event, pedelec_core::ThreadEvent::ProviderArtifact { .. }))
            .expect("OpenCode should materialize the inline ACP image");
        let completion_index = seen_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    pedelec_core::ThreadEvent::OperationCompleted { success: true, .. }
                )
            })
            .expect("the provider turn should complete successfully");
        assert!(artifact_index < completion_index);
        let artifact = match &seen_events[artifact_index] {
            pedelec_core::ThreadEvent::ProviderArtifact { artifact, .. } => artifact,
            _ => unreachable!(),
        };
        assert_eq!(artifact.provider, ProviderCode::OpenCode);
        assert_eq!(artifact.mime_type, "image/png");
        let artifact_file = workspace.join(".pedelec-runtime/assets").join(
            artifact
                .path
                .trim_start_matches('/')
                .split('/')
                .collect::<PathBuf>(),
        );
        assert_eq!(fs::read(artifact_file).unwrap(), test_png_bytes());
        let _ = owner.shutdown();
    }

    #[test]
    fn failed_acp_artifact_cannot_be_revived_by_terminal_success() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let runtime = active_artifact_runtime(
            ProviderCode::OpenCode,
            &workspace,
            "thread-open-bad-artifact",
            "turn-open-bad-artifact",
        );
        let events = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: "thread-open-bad-artifact".into(),
            })
            .unwrap();
        let controller = AcpController::spawn(
            pedelec_runtime::AcpLaunchConfig::new(
                "artifact-failure-test",
                fake_opencode_program(temp.path()),
                temp.path(),
            )
            .with_env(
                "FAKE_ACP_LOG",
                temp.path()
                    .join("failure.jsonl")
                    .to_string_lossy()
                    .into_owned(),
            )
            .with_env(
                "FAKE_ACP_WORKSPACE",
                workspace.to_string_lossy().into_owned(),
            )
            .with_env("FAKE_ACP_LOAD", "true"),
            Arc::new(|_: &AcpPermissionRequest| AcpPermissionDecision::RejectOnce),
        )
        .unwrap();

        handle_event(
            &runtime,
            &controller,
            AcpProviderKind::OpenCode,
            AcpRuntimeEvent::ProviderArtifact {
                pedelec_thread_id: "thread-open-bad-artifact".into(),
                provider_session_id: "provider-session".into(),
                local_turn_id: "turn-open-bad-artifact".into(),
                provider_artifact_id: Some("bad-image".into()),
                tool_call_id: None,
                source: AcpArtifactSource::ProviderArtifact,
                suggested_filename: "artifact.png".into(),
                mime_type: Some("image/png".into()),
                payload: Err("ACP binary content is malformed base64".into()),
            },
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread-open-bad-artifact"),
            Some(ThreadStatus::Error)
        );

        handle_event(
            &runtime,
            &controller,
            AcpProviderKind::OpenCode,
            AcpRuntimeEvent::TurnCompleted {
                pedelec_thread_id: "thread-open-bad-artifact".into(),
                provider_session_id: "provider-session".into(),
                local_turn_id: "turn-open-bad-artifact".into(),
                status: AcpTurnStatus::Completed,
                stop_reason: "end_turn".into(),
                error: None,
            },
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread-open-bad-artifact"),
            Some(ThreadStatus::Error)
        );
        let emitted = events.try_iter().collect::<Vec<_>>();
        assert!(!emitted
            .iter()
            .any(|event| matches!(event, pedelec_core::ThreadEvent::ProviderArtifact { .. })));
        assert!(emitted.iter().any(|event| matches!(
            event,
            pedelec_core::ThreadEvent::OperationCompleted { success: false, .. }
        )));
        controller.shutdown().unwrap();
    }

    #[test]
    fn cursor_auth_detection_accepts_provider_native_token_without_ui() {
        assert!(cursor_auth_available(&[(
            "CURSOR_API_KEY".into(),
            "native-token".into()
        )]));
        assert!(!cursor_auth_available(&[(
            "CURSOR_AUTH_TOKEN".into(),
            "  ".into()
        )]));
    }

    #[test]
    fn idle_end_without_existing_controller_does_not_spawn_runtime() {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = OpenCodeRuntimeDispatcher::new(owner.clone(), runtime);
        dispatcher
            .dispatch(PersistentRuntimeOperation::EndSession {
                session: PersistentProviderEndIntent {
                    thread_id: "thread-idle".into(),
                    provider: ProviderCode::OpenCode,
                    provider_session_id: Some("persisted-session".into()),
                    active_provider_turn_id: None,
                },
            })
            .unwrap();
        assert_eq!(owner.registry().lifecycle(OPENCODE_RUNTIME_KEY), None);
    }

    #[test]
    fn opencode_instruction_assets_are_workspace_scoped_per_thread() {
        let temp = tempdir().unwrap();
        let workspace_a = temp.path().join("workspace-a");
        let workspace_b = temp.path().join("workspace-b");
        fs::create_dir_all(&workspace_a).unwrap();
        fs::create_dir_all(&workspace_b).unwrap();
        let mut first = session_intent(&workspace_a, None);
        first.thread_id = "thread-a".into();
        first.host_instructions = "instruction-a".into();
        let mut second = session_intent(&workspace_b, None);
        second.thread_id = "thread-b".into();
        second.host_instructions = "instruction-b".into();

        write_session_instruction(&first).unwrap();
        write_session_instruction(&second).unwrap();

        assert_eq!(
            fs::read_to_string(workspace_a.join(OPENCODE_INSTRUCTION_PATH)).unwrap(),
            "instruction-a"
        );
        assert_eq!(
            fs::read_to_string(workspace_b.join(OPENCODE_INSTRUCTION_PATH)).unwrap(),
            "instruction-b"
        );
    }

    #[test]
    fn dispatcher_uses_new_config_instruction_and_raw_user_prompt() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("frames.jsonl");
        let fake_program = fake_opencode_program(temp.path());
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-thread-open";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: "thread-open".into(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::OpenCode,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec!["--model".into(), "fake/selected".into()],
                skills: vec![],
                status: ThreadStatus::Running,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        drop(runtime_guard);
        runtime.lock().unwrap().pending_provider_operations.insert(
            "thread-open".into(),
            PendingProviderOperation {
                operation_id: "test-operation".into(),
                kind: PendingProviderOperationKind::Prepare,
                started_at: Utc::now(),
            },
        );
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = OpenCodeRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_program)
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true")
            .with_env_for_test("FAKE_ACP_USAGE", "1")
            .with_env_for_test("FAKE_ACP_USAGE_SEQUENCE", "10,20");
        let mut session = session_intent(&workspace, None);
        dispatcher
            .dispatch(PersistentRuntimeOperation::EnsureSession {
                session: session.clone(),
            })
            .unwrap();
        let instruction = fs::read_to_string(workspace.join(OPENCODE_INSTRUCTION_PATH)).unwrap();
        assert_eq!(instruction, session.host_instructions);
        let provider_id = runtime
            .lock()
            .unwrap()
            .thread_manager
            .provider_session_state("thread-open")
            .unwrap()
            .provider_session_id
            .clone()
            .unwrap();

        session.provider_session_id = Some(provider_id.clone());
        {
            let mut core = runtime.lock().unwrap();
            core.thread_manager
                .thread_mut("thread-open")
                .unwrap()
                .status = ThreadStatus::Running;
            core.thread_manager
                .provider_session_state_mut("thread-open")
                .unwrap()
                .active_provider_turn_id = Some("local-open".into());
            core.pending_provider_operations.insert(
                "thread-open".into(),
                PendingProviderOperation {
                    operation_id: "test-operation".into(),
                    kind: PendingProviderOperationKind::UserTurn,
                    started_at: Utc::now(),
                },
            );
        }
        dispatcher
            .dispatch(PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: "thread-open".into(),
                    local_turn_id: "local-open".into(),
                    provider_session_id: Some(provider_id.clone()),
                    message: "actual user task".into(),
                    session: session.clone(),
                },
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("thread-open")
                .unwrap()
                .status
                != ThreadStatus::Idle
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("thread-open")
                .unwrap()
                .status,
            ThreadStatus::Idle
        );
        assert_eq!(
            runtime.lock().unwrap().session_total_tokens("thread-open"),
            Some(10)
        );

        {
            let mut core = runtime.lock().unwrap();
            core.thread_manager
                .thread_mut("thread-open")
                .unwrap()
                .status = ThreadStatus::Running;
            core.thread_manager
                .provider_session_state_mut("thread-open")
                .unwrap()
                .active_provider_turn_id = Some("local-open-2".into());
            core.pending_provider_operations.insert(
                "thread-open".into(),
                PendingProviderOperation {
                    operation_id: "test-operation-2".into(),
                    kind: PendingProviderOperationKind::UserTurn,
                    started_at: Utc::now(),
                },
            );
        }
        dispatcher
            .dispatch(PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: "thread-open".into(),
                    local_turn_id: "local-open-2".into(),
                    provider_session_id: Some(provider_id),
                    message: "second user task".into(),
                    session,
                },
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("thread-open")
                .unwrap()
                .status
                != ThreadStatus::Idle
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            runtime.lock().unwrap().session_total_tokens("thread-open"),
            Some(30)
        );
        let frames = fs::read_to_string(log).unwrap();
        let initialize: Value = frames
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .find(|frame: &Value| frame["method"] == "initialize")
            .expect("OpenCode initialize request should be captured");
        assert_eq!(initialize["params"]["clientCapabilities"], json!({}));
        assert!(
            !frames.contains("parameterizedModelPicker"),
            "OpenCode must not inherit Cursor's ACP extension"
        );
        let prompts = frames
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["method"] == "session/prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompts.len(), 2);
        assert_eq!(
            prompts[0]["params"]["prompt"][0]["text"],
            "actual user task"
        );
        assert_eq!(
            prompts[1]["params"]["prompt"][0]["text"],
            "second user task"
        );
        assert!(!prompts.iter().any(|prompt| prompt
            .to_string()
            .contains("Pedelec is the host application")));
        assert!(frames.contains("session/set_config_option"));

        let diagnostic_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let has_stderr = runtime
                .lock()
                .unwrap()
                .provider_runtime_diagnostic_history()
                .iter()
                .any(|diagnostic| {
                    matches!(
                        diagnostic,
                        ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                            provider: ProviderCode::OpenCode,
                            text,
                            ..
                        } if text.contains("fake ACP diagnostic")
                    )
                });
            if has_stderr || Instant::now() >= diagnostic_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let diagnostics = runtime
            .lock()
            .unwrap()
            .provider_runtime_diagnostic_history();
        let (started_generation, started_pid) = diagnostics
            .iter()
            .find_map(|diagnostic| match diagnostic {
                ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                    provider: ProviderCode::OpenCode,
                    runtime_generation,
                    process_id,
                    ..
                } => Some((*runtime_generation, *process_id)),
                _ => None,
            })
            .expect("OpenCode runtime start diagnostic should be recorded");
        let stderr = diagnostics
            .iter()
            .find(|diagnostic| {
                matches!(
                    diagnostic,
                    ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                        provider: ProviderCode::OpenCode,
                        text,
                        ..
                    } if text.contains("fake ACP diagnostic")
                )
            })
            .expect("OpenCode runtime stderr diagnostic should be recorded");
        let ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
            runtime_generation,
            process_id,
            ..
        } = stderr
        else {
            unreachable!();
        };
        assert_eq!(*runtime_generation, started_generation);
        assert_eq!(*process_id, started_pid);
        assert_eq!(
            serde_json::to_value(stderr).unwrap()["type"],
            "provider_runtime_stderr"
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn opencode_dispatcher_escapes_leading_slash_in_acp_frame_and_preserves_whitespace() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("frames.jsonl");
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let thread_id = "thread-open-leading-slash";
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-thread-open-leading-slash";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::OpenCode,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec!["--model".into(), "fake/selected".into()],
                skills: vec![],
                status: ThreadStatus::Running,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        drop(runtime_guard);
        runtime.lock().unwrap().pending_provider_operations.insert(
            thread_id.into(),
            PendingProviderOperation {
                operation_id: "test-operation".into(),
                kind: PendingProviderOperationKind::Prepare,
                started_at: Utc::now(),
            },
        );

        let owner = ProviderRuntimeOwner::new();
        let dispatcher = OpenCodeRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_opencode_program(temp.path()))
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true")
            .with_env_for_test("FAKE_ACP_USAGE", "1");
        let mut session = session_intent(&workspace, None);
        session.thread_id = thread_id.into();
        dispatcher
            .dispatch(PersistentRuntimeOperation::EnsureSession {
                session: session.clone(),
            })
            .unwrap();
        let provider_id = runtime
            .lock()
            .unwrap()
            .thread_manager
            .provider_session_state(thread_id)
            .unwrap()
            .provider_session_id
            .clone()
            .unwrap();
        session.provider_session_id = Some(provider_id.clone());

        for (local_turn_id, message) in [
            ("leading-slash-image", "/upl_test.jpg 裡有什麼?"),
            ("leading-slash-whitespace", "   /foo"),
        ] {
            {
                let mut core = runtime.lock().unwrap();
                core.thread_manager.thread_mut(thread_id).unwrap().status = ThreadStatus::Running;
                core.thread_manager
                    .provider_session_state_mut(thread_id)
                    .unwrap()
                    .active_provider_turn_id = Some(local_turn_id.into());
                core.pending_provider_operations.insert(
                    thread_id.into(),
                    PendingProviderOperation {
                        operation_id: "test-operation".into(),
                        kind: PendingProviderOperationKind::UserTurn,
                        started_at: Utc::now(),
                    },
                );
            }
            dispatcher
                .dispatch(PersistentRuntimeOperation::StartTurn {
                    turn: PersistentProviderTurnIntent {
                        thread_id: thread_id.into(),
                        local_turn_id: local_turn_id.into(),
                        provider_session_id: Some(provider_id.clone()),
                        message: message.into(),
                        session: session.clone(),
                    },
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline
                && runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread(thread_id)
                    .unwrap()
                    .status
                    != ThreadStatus::Idle
            {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                runtime
                    .lock()
                    .unwrap()
                    .thread_manager
                    .thread(thread_id)
                    .unwrap()
                    .status,
                ThreadStatus::Idle
            );
        }

        let prompts = fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["method"] == "session/prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompts.len(), 2);
        assert_eq!(
            prompts[0]["params"]["prompt"][0]["text"],
            "- /upl_test.jpg 裡有什麼?"
        );
        assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "-    /foo");
        let _ = owner.shutdown();
    }

    fn active_artifact_runtime(
        provider: ProviderCode,
        workspace: &Path,
        thread_id: &str,
        turn_id: &str,
    ) -> SharedCoreRuntime {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let mut core = runtime.lock().unwrap();
        let workspace_id = format!("workspace-{thread_id}");
        core.register_workspace_for_test(&workspace_id, workspace, WorkspaceKind::Custom)
            .unwrap();
        core.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.to_string(),
                workspace_id,
                provider,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec![],
                skills: vec![],
                status: ThreadStatus::Running,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: Some("provider-session".into()),
                active_provider_turn_id: Some(turn_id.to_string()),
            },
        );
        core.pending_provider_operations.insert(
            thread_id.to_string(),
            PendingProviderOperation {
                operation_id: format!("operation-{turn_id}"),
                kind: PendingProviderOperationKind::UserTurn,
                started_at: Utc::now(),
            },
        );
        drop(core);
        runtime
    }

    fn session_intent(
        workspace: &Path,
        provider_session_id: Option<String>,
    ) -> PersistentProviderSessionIntent {
        PersistentProviderSessionIntent {
            thread_id: "thread-open".into(),
            provider: ProviderCode::OpenCode,
            provider_session_id,
            workspace_path: workspace.to_path_buf(),
            effort_level: Some(EffortLevel::Default),
            model: Some("fake/selected".into()),
            cursor_settings: None,
            reasoning_effort: None,
            antigravity_reasoning_effort: None,
            claude_reasoning_effort: None,
            approval_policy: PersistentApprovalPolicy::Never,
            sandbox_policy: PersistentSandboxPolicy::ReadOnly,
            host_instructions: "Pedelec is the host application\nthread-open privileged context"
                .into(),
            config: HashMap::new(),
            core_ipc_runtime_file_path: workspace.join("runtime.json"),
            tools: vec![],
            guidance: None,
        }
    }

    #[test]
    fn opencode_and_cursor_in_place_upgrade_routes_and_retires_independent_generations() {
        let temp = tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let owner = ProviderRuntimeOwner::new();
        let program = fake_opencode_program(temp.path());
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(if cfg!(windows) {
            "../pedelec-runtime/tests/fixtures/fake_acp_agent.ps1"
        } else {
            "../pedelec-runtime/tests/fixtures/fake_acp_agent.sh"
        });
        let copy = temp.path().join(if cfg!(windows) {
            "agent.ps1"
        } else {
            "agent.sh"
        });
        let source = fs::read_to_string(&fixture).unwrap();
        fs::write(&copy, source.replace("acp-session-", "old-session-")).unwrap();
        fs::write(
            &program,
            fs::read_to_string(&program)
                .unwrap()
                .replace(&fixture.display().to_string(), &copy.display().to_string()),
        )
        .unwrap();
        let mut old_controllers = Vec::new();
        let mut new_controllers = Vec::new();
        for provider in [AcpProviderKind::OpenCode, AcpProviderKind::Cursor] {
            fs::write(&copy, source.replace("acp-session-", "old-session-")).unwrap();
            let code = provider.provider_code();
            runtime.lock().unwrap().set_provider_selection_for_test(
                code.clone(),
                program.clone(),
                "0.147.0",
            );
            let a_id = format!("{}-a", provider.code());
            let b_id = format!("{}-b", provider.code());
            let a = crate::selection::test_session(&runtime, temp.path(), code.clone(), &a_id);
            let b = crate::selection::test_session(&runtime, temp.path(), code.clone(), &b_id);
            let dispatcher =
                AcpRuntimeDispatcher::new_for_provider(provider, owner.clone(), runtime.clone());
            dispatcher
                .dispatch(PersistentRuntimeOperation::EnsureSession { session: a.clone() })
                .unwrap();
            let old = dispatcher
                .router
                .state_for_test(&a_id, |d| d.any_controller().unwrap());
            assert_eq!(
                runtime
                    .lock()
                    .unwrap()
                    .provider_session_state(&a_id)
                    .unwrap()
                    .provider_session_id
                    .as_deref(),
                Some("old-session-1")
            );
            fs::write(&copy, source.replace("acp-session-", "new-session-")).unwrap();
            runtime.lock().unwrap().set_provider_selection_for_test(
                code.clone(),
                program.clone(),
                "0.160.0",
            );
            dispatcher
                .dispatch(PersistentRuntimeOperation::EnsureSession { session: a })
                .unwrap();
            assert_eq!(
                dispatcher
                    .router
                    .state_for_test(&a_id, |d| d.any_controller().unwrap().process_id()),
                old.process_id()
            );
            dispatcher
                .dispatch(PersistentRuntimeOperation::EnsureSession { session: b.clone() })
                .unwrap();
            let new = dispatcher
                .router
                .state_for_test(&b_id, |d| d.any_controller().unwrap());
            assert_ne!(old.process_id(), new.process_id());
            assert_eq!(
                runtime
                    .lock()
                    .unwrap()
                    .provider_session_state(&b_id)
                    .unwrap()
                    .provider_session_id
                    .as_deref(),
                Some("new-session-1")
            );
            dispatcher
                .dispatch(crate::selection::end_operation(code, &a_id))
                .unwrap();
            assert!(!old.is_healthy());
            assert!(new.is_healthy());
            dispatcher
                .dispatch(PersistentRuntimeOperation::EnsureSession { session: b })
                .unwrap();
            old_controllers.push(old);
            new_controllers.push(new);
        }
        assert_ne!(
            new_controllers[0].process_id(),
            new_controllers[1].process_id()
        );
        owner.shutdown();
        assert!(new_controllers.iter().all(|c| !c.is_healthy()));
    }

    fn fake_opencode_program(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let target = directory.join("fake-opencode.cmd");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_acp_agent.ps1");
            fs::write(
                &target,
                format!(
                    "@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"\r\n",
                    fixture.display()
                ),
            )
            .unwrap();
            target
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let target = directory.join("fake-opencode");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_acp_agent.sh");
            fs::write(
                &target,
                format!("#!/bin/sh\nexec sh '{}'\n", fixture.display()),
            )
            .unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            target
        }
    }

    fn fake_cursor_program(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let target = directory.join("fake-cursor.cmd");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_acp_agent.ps1");
            fs::write(
                &target,
                format!(
                    "@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"\r\n",
                    fixture.display()
                ),
            )
            .unwrap();
            target
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let target = directory.join("fake-cursor");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_acp_agent.sh");
            fs::write(
                &target,
                format!("#!/bin/sh\nexec sh '{}'\n", fixture.display()),
            )
            .unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            target
        }
    }
}
