use crate::{record_rpc_traffic, PersistentRuntimeDispatcher};
use pedelec_core::{
    build_persistent_user_prompt_with_bootstrap, error_codes, PedelecError,
    PersistentProviderSessionIntent, PersistentRuntimeOperation, ProviderCode,
    ProviderRuntimeDiagnostic, ProviderRuntimeEvent, SharedCoreRuntime,
};
use pedelec_runtime::{
    AcpAuthentication, AcpConfigOptionUpdate, AcpController, AcpExtensionRequestHandler,
    AcpPermissionDecision, AcpPermissionRequest, AcpRuntimeError, AcpRuntimeEvent,
    AcpSessionConfig, AcpTurnStatus, AcpWorkspacePermissionPolicy, ProviderRuntimeController,
    ProviderRuntimeOwner, RpcServerRequest, RuntimeRegistryError,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
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
    workspaces: Arc<Mutex<HashMap<String, PathBuf>>>,
    event_pumps: Arc<Mutex<HashSet<u64>>>,
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
            workspaces: Arc::new(Mutex::new(HashMap::new())),
            event_pumps: Arc::new(Mutex::new(HashSet::new())),
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

        // A healthy app-lifetime controller remains authoritative even if a
        // background provider refresh has since produced an incomplete scan.
        // Only a replacement generation needs a newly scanned executable.
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
            .get_or_init(provider.runtime_key(), move || {
                let mut launch =
                    pedelec_runtime::AcpLaunchConfig::new(provider.label(), program, process_cwd)
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
                } else if !cursor_pre_authenticated {
                    launch = launch.with_authentication(AcpAuthentication::MethodId(
                        "cursor_login".to_string(),
                    ));
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
                    Arc::new(CursorExtensionRequestHandler) as Arc<dyn AcpExtensionRequestHandler>
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
            })
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

        // ACP providers advertise model selection as a categorized session
        // config option. The option id itself is intentionally not assumed.
        if let Some(model) = session.model.as_deref() {
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

    fn start_event_pump(&self, controller: Arc<AcpController>) {
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
            },
        );
        let runtime = Arc::clone(&self.core_runtime);
        let typed = Arc::clone(&self.typed_controller);
        let event_pumps = Arc::clone(&self.event_pumps);
        let stopped = Arc::clone(&self.stopped_generations);
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
                handle_event(&runtime, &controller, provider, event);
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
                core.fail_persistent_runtime_except(
                    self.provider.provider_code(),
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
            pedelec_runtime::AcpPermissionResolver::resolve(
                &AcpWorkspacePermissionPolicy::new(workspace),
                request,
            )
        })
        .unwrap_or(AcpPermissionDecision::RejectOnce)
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

fn handle_event(
    runtime: &SharedCoreRuntime,
    controller: &AcpController,
    provider: AcpProviderKind,
    event: AcpRuntimeEvent,
) {
    match event {
        AcpRuntimeEvent::RpcTraffic(record) => record_rpc_traffic(
            runtime,
            provider.provider_code(),
            controller.generation(),
            controller.process_id(),
            record,
        ),
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
        AcpRuntimeEvent::Notification { .. } => {}
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
                core.fail_persistent_runtime(provider.provider_code(), error);
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
                core.fail_persistent_runtime(
                    provider.provider_code(),
                    PedelecError::with_details(
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
                    ),
                );
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
    use chrono::Utc;
    use pedelec_core::{
        EffortLevel, PendingProviderOperation, PersistentApprovalPolicy,
        PersistentProviderEndIntent, PersistentProviderTurnIntent, PersistentSandboxPolicy,
        ProviderSessionState, ThreadState, ThreadStatus,
    };
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

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
                    core.use_persistent_provider_for_test(provider.clone());
                    let id = format!("thread-{}", format!("{provider:?}").to_lowercase());
                    core.thread_manager.insert_thread(
                        ThreadState {
                            thread_id: id.clone(),
                            provider: provider.clone(),
                            effort_level: EffortLevel::Default,
                            effort_args: vec![],
                            workspace_path: workspace.join(&id),
                            skills: vec![],
                            status: ThreadStatus::Running,
                            process_id: None,
                            created_at: Utc::now(),
                            updated_at: Utc::now(),
                            sdk_origin: None,
                        },
                        ProviderSessionState {
                            provider_session_id: Some(format!("session-{id}")),
                            active_provider_turn_id: Some(format!("turn-{id}")),
                            last_process_id: None,
                            has_user_message: true,
                        },
                    );
                    core.pending_provider_operations
                        .insert(id, PendingProviderOperation::UserTurn);
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

    #[test]
    fn cursor_dispatcher_authenticates_maps_mode_and_bootstraps_once() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("cursor-frames.jsonl");
        let fake_program = fake_cursor_program(temp.path());
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        runtime.lock().unwrap().thread_manager.insert_thread(
            ThreadState {
                thread_id: "thread-cursor".into(),
                provider: ProviderCode::Cursor,
                effort_level: EffortLevel::Default,
                effort_args: vec!["--model".into(), "fake/selected".into()],
                workspace_path: workspace.clone(),
                skills: vec![],
                status: ThreadStatus::Running,
                process_id: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime
            .lock()
            .unwrap()
            .pending_provider_operations
            .insert("thread-cursor".into(), PendingProviderOperation::Prepare);
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
                core.pending_provider_operations
                    .insert("thread-cursor".into(), PendingProviderOperation::UserTurn);
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
        assert!(frames.contains("\"method\":\"session/set_mode\""));
        assert!(frames.contains("\"method\":\"session/set_config_option\""));
        let prompts = frames
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["method"] == "session/prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0]["params"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[Pedelec Host Bootstrap]"));
        assert!(prompts[0]["params"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .contains("first task"));
        assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "second task");
        assert!(!frames.contains("PEDELEC_PREPARED"));
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
                core.use_persistent_provider_for_test(provider.provider_code());
                core.thread_manager.insert_thread(
                    ThreadState {
                        thread_id: thread_id.clone(),
                        provider: provider.provider_code(),
                        effort_level: EffortLevel::Default,
                        effort_args: vec!["--model".into(), "fake/selected".into()],
                        workspace_path: workspace.clone(),
                        skills: vec![],
                        status: ThreadStatus::Running,
                        process_id: None,
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                        sdk_origin: None,
                    },
                    ProviderSessionState {
                        provider_session_id: Some("persisted-session".into()),
                        active_provider_turn_id: None,
                        last_process_id: None,
                        has_user_message: true,
                    },
                );
                core.pending_provider_operations
                    .insert(thread_id.clone(), PendingProviderOperation::Prepare);
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
                core.pending_provider_operations
                    .insert(thread_id.clone(), PendingProviderOperation::UserTurn);
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
        runtime.lock().unwrap().thread_manager.insert_thread(
            ThreadState {
                thread_id: "thread-open".into(),
                provider: ProviderCode::OpenCode,
                effort_level: EffortLevel::Default,
                effort_args: vec!["--model".into(), "fake/selected".into()],
                workspace_path: workspace.clone(),
                skills: vec![],
                status: ThreadStatus::Running,
                process_id: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime
            .lock()
            .unwrap()
            .pending_provider_operations
            .insert("thread-open".into(), PendingProviderOperation::Prepare);
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = OpenCodeRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_program_for_test(fake_program)
            .with_process_cwd_for_test(temp.path())
            .with_env_for_test("FAKE_ACP_LOG", log.to_string_lossy())
            .with_env_for_test("FAKE_ACP_WORKSPACE", workspace.to_string_lossy())
            .with_env_for_test("FAKE_ACP_LOAD", "true");
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
            core.pending_provider_operations
                .insert("thread-open".into(), PendingProviderOperation::UserTurn);
        }
        dispatcher
            .dispatch(PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: "thread-open".into(),
                    local_turn_id: "local-open".into(),
                    provider_session_id: Some(provider_id),
                    message: "actual user task".into(),
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
            runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("thread-open")
                .unwrap()
                .status,
            ThreadStatus::Idle
        );
        let frames = fs::read_to_string(log).unwrap();
        let prompt = frames
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["method"] == "session/prompt")
            .unwrap();
        assert_eq!(prompt["params"]["prompt"][0]["text"], "actual user task");
        assert!(!prompt
            .to_string()
            .contains("Pedelec is the host application"));
        assert!(frames.contains("session/set_config_option"));
        let _ = owner.shutdown();
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
            effort_level: EffortLevel::Default,
            model: Some("fake/selected".into()),
            reasoning_effort: None,
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
