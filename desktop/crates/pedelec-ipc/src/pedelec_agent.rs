use crate::{record_protocol_traffic, PersistentRuntimeDispatcher};
use pedelec_core::{
    error_codes, PedelecError, PedelecSettings, PersistentProviderEndIntent,
    PersistentProviderSessionIntent, PersistentRuntimeOperation, ProviderCode,
    ProviderRuntimeDiagnostic, ProviderRuntimeEvent, SharedCoreRuntime,
};
use pedelec_runtime::{
    PedelecAgentCloseOutcome, PedelecAgentRuntimeError, PedelecAgentRuntimeEvent,
    PedelecAgentRuntimeLaunchConfig, PedelecAgentServerController, PedelecAgentSessionConfig,
    PedelecAgentTurnStatus, ProviderRuntimeController, ProviderRuntimeOwner, RuntimeRegistryError,
};
use pedelec_shared::paths::path_for_external_use;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const PEDELEC_AGENT_OLLAMA_PRODUCT: &str = "ollama";
pub const PEDELEC_AGENT_OLLAMA_RUNTIME_KEY: &str = "pedelec-agent:ollama";

const MAX_RUNTIME_DIAGNOSTIC_TEXT_BYTES: usize = 4096;

const PRESERVED_AGENT_ERROR_CODES: &[&str] = &[
    error_codes::OLLAMA_UNAVAILABLE,
    error_codes::OLLAMA_AUTH_FAILED,
    error_codes::OLLAMA_MODEL_NOT_FOUND,
    error_codes::OLLAMA_CLOUD_LIMIT_EXCEEDED,
    error_codes::OLLAMA_REQUEST_FAILED,
    error_codes::OLLAMA_RESPONSE_INVALID,
    error_codes::OLLAMA_API_KEY_REQUIRED,
    error_codes::OLLAMA_BASE_URL_INVALID,
    error_codes::MODEL_REQUIRED,
];

/// Generic persistent dispatcher for one `pedelec-agent serve --provider <product>`
/// process generation. Production currently instantiates Ollama; LM Studio / vLLM
/// can reuse this type with a different product/runtime key.
#[derive(Debug, Clone)]
pub struct PedelecAgentRuntimeDispatcher {
    provider: ProviderCode,
    runtime_key: String,
    product: String,
    owner: ProviderRuntimeOwner,
    core_runtime: SharedCoreRuntime,
    program_override: Option<PathBuf>,
    env_overrides: Vec<(OsString, OsString)>,
    process_cwd: PathBuf,
    typed_controller: Arc<Mutex<Option<Arc<PedelecAgentServerController>>>>,
    event_pumps: Arc<Mutex<HashSet<u64>>>,
    traffic_pumps: Arc<Mutex<HashSet<u64>>>,
    stopped_generations: Arc<Mutex<HashSet<u64>>>,
}

impl PedelecAgentRuntimeDispatcher {
    pub fn new(
        owner: ProviderRuntimeOwner,
        core_runtime: SharedCoreRuntime,
        provider: ProviderCode,
        product: impl Into<String>,
    ) -> Self {
        let product = product.into();
        Self {
            provider,
            runtime_key: format!("pedelec-agent:{product}"),
            product,
            owner,
            core_runtime,
            program_override: None,
            env_overrides: Vec::new(),
            process_cwd: std::env::temp_dir(),
            typed_controller: Arc::new(Mutex::new(None)),
            event_pumps: Arc::new(Mutex::new(HashSet::new())),
            traffic_pumps: Arc::new(Mutex::new(HashSet::new())),
            stopped_generations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn for_ollama(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self::new(
            owner,
            core_runtime,
            ProviderCode::Ollama,
            PEDELEC_AGENT_OLLAMA_PRODUCT,
        )
    }

    pub fn runtime_key(&self) -> &str {
        &self.runtime_key
    }

    #[doc(hidden)]
    pub fn with_program_for_test(mut self, program: impl Into<PathBuf>) -> Self {
        self.program_override = Some(program.into());
        self
    }

    #[doc(hidden)]
    pub fn with_env_for_test(
        mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.env_overrides.push((key.into(), value.into()));
        self
    }

    #[doc(hidden)]
    pub fn with_process_cwd_for_test(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.process_cwd = cwd.into();
        self
    }

    fn controller_for(
        &self,
        runtime_file: &Path,
        operation_thread_id: &str,
    ) -> Result<Arc<PedelecAgentServerController>, PedelecError> {
        self.fail_unhealthy_controller(Some(operation_thread_id));
        let program_override = self.program_override.clone();
        let env_overrides = self.env_overrides.clone();
        let core_runtime = Arc::clone(&self.core_runtime);
        let process_cwd = self.process_cwd.clone();
        let runtime_file = runtime_file.to_path_buf();
        let typed_controller = Arc::clone(&self.typed_controller);
        let provider = self.provider.clone();
        let product = self.product.clone();
        self.owner
            .get_or_init(self.runtime_key.as_str(), move || {
                let program =
                    resolve_pedelec_agent_program(program_override.as_ref()).map_err(|error| {
                        RuntimeRegistryError::Initialization(format!(
                            "{} ({})",
                            error.message, error.code
                        ))
                    })?;
                let settings = core_runtime
                    .lock()
                    .map_err(|_| {
                        RuntimeRegistryError::Initialization(
                            "Core runtime mutex was poisoned".to_string(),
                        )
                    })
                    .and_then(|core| {
                        core.get_settings().map_err(|error| {
                            RuntimeRegistryError::Initialization(format!(
                                "{} ({})",
                                error.message, error.code
                            ))
                        })
                    })?;
                let launch = build_launch_config(
                    program,
                    process_cwd,
                    &provider,
                    &product,
                    &settings,
                    &runtime_file,
                    &env_overrides,
                )
                .map_err(|error| {
                    RuntimeRegistryError::Initialization(format!(
                        "{} ({})",
                        error.message, error.code
                    ))
                })?;
                let controller = PedelecAgentServerController::spawn(launch)
                    .map_err(|error| RuntimeRegistryError::Initialization(error.to_string()))?;
                *typed_controller.lock().map_err(|_| {
                    RuntimeRegistryError::Initialization(
                        "Pedelec Agent controller mutex was poisoned".to_string(),
                    )
                })? = Some(Arc::clone(&controller));
                Ok(controller as Arc<dyn ProviderRuntimeController>)
            })
            .map_err(|error| registry_error(error, &self.provider, &self.product))?;
        self.typed_controller
            .lock()
            .map_err(|_| mutex_error("Pedelec Agent controller"))?
            .clone()
            .ok_or_else(|| {
                registry_error(
                    RuntimeRegistryError::Initialization(
                        "Pedelec Agent controller was not installed".to_string(),
                    ),
                    &self.provider,
                    &self.product,
                )
            })
    }

    fn fail_unhealthy_controller(&self, excluded_thread_id: Option<&str>) {
        let controller = self
            .typed_controller
            .lock()
            .ok()
            .and_then(|controller| controller.clone())
            .filter(|controller| !controller.is_healthy());
        let Some(controller) = controller else {
            return;
        };
        let error = PedelecError::with_details(
            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
            "Pedelec Agent runtime disconnected",
            json!({
                "provider": self.provider,
                "product": self.product,
                "operation": "runtime",
                "runtimeGeneration": controller.generation(),
                "processId": controller.process_id(),
                "reason": "replacement requested before disconnect event was reduced",
            }),
        );
        if let Ok(mut core) = self.core_runtime.lock() {
            core.fail_persistent_runtime_except(self.provider.clone(), excluded_thread_id, error);
        }
    }

    fn start_traffic_pump(&self, controller: Arc<PedelecAgentServerController>) {
        let generation = controller.generation();
        if !self
            .traffic_pumps
            .lock()
            .map(|mut generations| generations.insert(generation))
            .unwrap_or(false)
        {
            return;
        }
        let runtime = Arc::clone(&self.core_runtime);
        let current_controller = Arc::clone(&self.typed_controller);
        let traffic_pumps = Arc::clone(&self.traffic_pumps);
        let provider = self.provider.clone();
        let process_id = controller.process_id();
        thread::spawn(move || {
            loop {
                match controller.recv_protocol_traffic_timeout(Duration::from_millis(50)) {
                    Ok(record) => {
                        let current = match current_controller.lock() {
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
                            provider.clone(),
                            generation,
                            process_id,
                            record,
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !controller.is_healthy() {
                            break;
                        }
                        let is_current = current_controller
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
            if let Ok(mut generations) = traffic_pumps.lock() {
                generations.remove(&generation);
            }
        });
    }

    fn start_event_pump(&self, controller: Arc<PedelecAgentServerController>) {
        self.start_traffic_pump(Arc::clone(&controller));
        let generation = controller.generation();
        let should_start = self
            .event_pumps
            .lock()
            .map(|mut generations| generations.insert(generation))
            .unwrap_or(false);
        if !should_start {
            return;
        }
        record_runtime_diagnostic(
            &self.core_runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                provider: self.provider.clone(),
                runtime_generation: generation,
                process_id: controller.process_id(),
            },
        );
        let runtime = Arc::clone(&self.core_runtime);
        let current_controller = Arc::clone(&self.typed_controller);
        let event_pumps = Arc::clone(&self.event_pumps);
        let stopped_generations = Arc::clone(&self.stopped_generations);
        let provider = self.provider.clone();
        let product = self.product.clone();
        thread::spawn(move || {
            while let Ok(event) = controller.recv_event() {
                let current = match current_controller.lock() {
                    Ok(current) => current,
                    Err(_) => break,
                };
                if !current
                    .as_ref()
                    .is_some_and(|active| active.generation() == generation)
                {
                    break;
                }
                match event {
                    PedelecAgentRuntimeEvent::TurnStarted {
                        pedelec_thread_id,
                        agent_session_id,
                        turn_id,
                    } => {
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                                provider: provider.clone(),
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id: pedelec_thread_id.clone(),
                                provider_thread_id: agent_session_id.clone(),
                                provider_turn_id: turn_id.clone(),
                            },
                        );
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                ProviderRuntimeEvent::TurnStarted {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id: turn_id,
                                },
                            );
                        }
                    }
                    PedelecAgentRuntimeEvent::AssistantDelta {
                        pedelec_thread_id,
                        turn_id,
                        text,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                ProviderRuntimeEvent::AssistantDelta {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id: Some(turn_id),
                                    text,
                                },
                            );
                        }
                    }
                    PedelecAgentRuntimeEvent::AssistantMessage {
                        pedelec_thread_id,
                        turn_id,
                        text,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                ProviderRuntimeEvent::AssistantMessage {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id: Some(turn_id),
                                    text,
                                },
                            );
                        }
                    }
                    PedelecAgentRuntimeEvent::UsageUpdated {
                        pedelec_thread_id,
                        turn_id,
                        usage,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                ProviderRuntimeEvent::UsageUpdated {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id: Some(turn_id),
                                    usage,
                                },
                            );
                        }
                    }
                    PedelecAgentRuntimeEvent::TurnCompleted {
                        pedelec_thread_id,
                        agent_session_id,
                        turn_id,
                        status,
                        error,
                    } => {
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                                provider: provider.clone(),
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id: pedelec_thread_id.clone(),
                                provider_thread_id: agent_session_id.clone(),
                                provider_turn_id: Some(turn_id.clone()),
                                status: turn_status_label(status).to_string(),
                            },
                        );
                        let success = status == PedelecAgentTurnStatus::Completed;
                        let error = if success {
                            None
                        } else {
                            Some(turn_completion_error(
                                &provider,
                                &product,
                                &pedelec_thread_id,
                                &agent_session_id,
                                Some(&turn_id),
                                error,
                                controller.generation(),
                                controller.process_id(),
                            ))
                        };
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                ProviderRuntimeEvent::TurnCompleted {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id: Some(turn_id),
                                    success,
                                    error,
                                },
                            );
                        }
                    }
                    PedelecAgentRuntimeEvent::ProtocolError {
                        pedelec_thread_id,
                        agent_session_id,
                        operation,
                        message,
                    } => {
                        controller.retire_for_protocol_error();
                        let error = PedelecError::with_details(
                            error_codes::PROVIDER_PROTOCOL_ERROR,
                            message,
                            json!({
                                "provider": provider,
                                "product": product,
                                "operation": operation,
                                "stage": "stream",
                                "providerThreadId": agent_session_id,
                                "runtimeGeneration": controller.generation(),
                                "processId": controller.process_id(),
                            }),
                        );
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeError {
                                provider: provider.clone(),
                                runtime_generation: Some(controller.generation()),
                                process_id: Some(controller.process_id()),
                                thread_id: pedelec_thread_id,
                                provider_thread_id: agent_session_id,
                                provider_turn_id: None,
                                code: error_codes::PROVIDER_PROTOCOL_ERROR.to_string(),
                                message: error.message.clone(),
                                details: error.details.clone(),
                            },
                        );
                        if let Ok(mut core) = runtime.lock() {
                            core.fail_persistent_runtime(provider.clone(), error);
                        }
                    }
                    PedelecAgentRuntimeEvent::Disconnected {
                        generation,
                        pid,
                        attachments,
                        reason,
                    } => {
                        let reason_text = format!("{reason:?}");
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                                provider: provider.clone(),
                                runtime_generation: generation,
                                process_id: pid,
                                thread_id: None,
                                provider_thread_id: None,
                                reason: reason_text.clone(),
                            },
                        );
                        for attachment in &attachments {
                            record_runtime_diagnostic(
                                &runtime,
                                ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                                    provider: provider.clone(),
                                    runtime_generation: generation,
                                    process_id: pid,
                                    thread_id: Some(attachment.pedelec_thread_id.clone()),
                                    provider_thread_id: Some(attachment.agent_session_id.clone()),
                                    reason: reason_text.clone(),
                                },
                            );
                        }
                        let error = PedelecError::with_details(
                            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                            "Pedelec Agent runtime disconnected",
                            json!({
                                "provider": provider,
                                "product": product,
                                "operation": "runtime",
                                "runtimeGeneration": generation,
                                "processId": pid,
                                "reason": reason_text,
                                "attachments": attachments
                                    .iter()
                                    .map(|attachment| {
                                        json!({
                                            "pedelecThreadId": attachment.pedelec_thread_id,
                                            "providerThreadId": attachment.agent_session_id,
                                        })
                                    })
                                    .collect::<Vec<_>>(),
                            }),
                        );
                        if let Ok(mut core) = runtime.lock() {
                            core.fail_persistent_runtime(provider.clone(), error);
                        }
                        mark_runtime_stopped(
                            &runtime,
                            &stopped_generations,
                            provider.clone(),
                            generation,
                            pid,
                            "runtime disconnected",
                        );
                        break;
                    }
                    PedelecAgentRuntimeEvent::Notification { .. } => {}
                    PedelecAgentRuntimeEvent::Stderr { text } => {
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                                provider: provider.clone(),
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                text: truncate_diagnostic_text(&text),
                            },
                        );
                    }
                }
            }
            if let Ok(mut generations) = event_pumps.lock() {
                generations.remove(&generation);
            }
        });
    }

    pub(crate) fn current_controller(&self) -> Option<Arc<PedelecAgentServerController>> {
        self.typed_controller
            .lock()
            .ok()
            .and_then(|controller| controller.clone())
            .filter(|controller| controller.is_healthy())
    }

    pub fn record_shutdown_diagnostics(&self) {
        let controller = self
            .typed_controller
            .lock()
            .ok()
            .and_then(|controller| controller.clone());
        if let Some(controller) = controller {
            mark_runtime_stopped(
                &self.core_runtime,
                &self.stopped_generations,
                self.provider.clone(),
                controller.generation(),
                controller.process_id(),
                "desktop shutdown",
            );
        }
    }

    pub fn retire(&self, reason: &str) -> Result<(), PedelecError> {
        let controller = self
            .typed_controller
            .lock()
            .map_err(|_| mutex_error("Pedelec Agent controller"))?
            .take();
        let Some(controller) = controller else {
            return Ok(());
        };
        let generation = controller.generation();
        let process_id = controller.process_id();
        mark_runtime_stopped(
            &self.core_runtime,
            &self.stopped_generations,
            self.provider.clone(),
            generation,
            process_id,
            reason,
        );
        controller.retire();
        let error = PedelecError::with_details(
            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
            "Pedelec Agent runtime was retired because provider settings changed",
            json!({
                "provider": self.provider,
                "product": self.product,
                "operation": "settings",
                "runtimeGeneration": generation,
                "processId": process_id,
                "reason": reason,
            }),
        );
        if let Ok(mut core) = self.core_runtime.lock() {
            core.fail_persistent_runtime(self.provider.clone(), error);
        }
        Ok(())
    }

    fn dispatch_end_session(
        &self,
        session: &PersistentProviderEndIntent,
    ) -> Result<(), PedelecError> {
        let Some(controller) = self.current_controller() else {
            if session.active_provider_turn_id.is_some() {
                let error = end_disconnect_error(
                    session,
                    &self.provider,
                    &self.product,
                    "the active Pedelec Agent turn has no healthy runtime generation",
                    None,
                );
                if let Ok(mut core) = self.core_runtime.lock() {
                    core.fail_persistent_runtime(self.provider.clone(), error.clone());
                }
                return Err(error);
            }
            return Ok(());
        };

        match controller.close_session(
            &session.thread_id,
            session.provider_session_id.as_deref(),
            session.active_provider_turn_id.as_deref(),
        ) {
            Ok(PedelecAgentCloseOutcome::Closed | PedelecAgentCloseOutcome::AlreadyDetached) => {
                Ok(())
            }
            Ok(PedelecAgentCloseOutcome::GenerationRetired { message }) => {
                let _ = self.typed_controller.lock().map(|mut current| {
                    if current
                        .as_ref()
                        .is_some_and(|active| active.generation() == controller.generation())
                    {
                        current.take();
                    }
                });
                mark_runtime_stopped(
                    &self.core_runtime,
                    &self.stopped_generations,
                    self.provider.clone(),
                    controller.generation(),
                    controller.process_id(),
                    &message,
                );
                let error = end_disconnect_error(
                    session,
                    &self.provider,
                    &self.product,
                    &message,
                    Some(&controller),
                );
                if let Ok(mut core) = self.core_runtime.lock() {
                    core.fail_persistent_runtime(self.provider.clone(), error);
                }
                Ok(())
            }
            Err(error) => {
                let mapped = map_agent_error(
                    error,
                    Some(&controller),
                    &self.provider,
                    &self.product,
                    &session.thread_id,
                    session.provider_session_id.as_deref(),
                    session.active_provider_turn_id.as_deref(),
                    "end",
                );
                if let Ok(mut core) = self.core_runtime.lock() {
                    core.fail_persistent_runtime(self.provider.clone(), mapped.clone());
                }
                Err(mapped)
            }
        }
    }
}

impl PersistentRuntimeDispatcher for PedelecAgentRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        if operation.provider() != &self.provider {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_UNSUPPORTED,
                "Pedelec Agent runtime received an operation for another provider",
                json!({
                    "expectedProvider": self.provider,
                    "operationProvider": operation.provider(),
                    "product": self.product,
                }),
            ));
        }
        if let PersistentRuntimeOperation::EndSession { session } = &operation {
            return self.dispatch_end_session(session);
        }

        let (thread_id, runtime_file) = match &operation {
            PersistentRuntimeOperation::EnsureSession { session } => (
                session.thread_id.clone(),
                session.core_ipc_runtime_file_path.clone(),
            ),
            PersistentRuntimeOperation::StartTurn { turn } => (
                turn.thread_id.clone(),
                turn.session.core_ipc_runtime_file_path.clone(),
            ),
            PersistentRuntimeOperation::EndSession { .. } => {
                unreachable!(
                    "persistent Pedelec Agent end operations are handled before session dispatch"
                )
            }
        };

        let controller = self.controller_for(&runtime_file, &thread_id)?;
        self.start_event_pump(Arc::clone(&controller));

        match operation {
            PersistentRuntimeOperation::EnsureSession { session } => {
                let config = session_config(&session)?;
                let result = controller.ensure_session(
                    &thread_id,
                    session.provider_session_id.as_deref(),
                    &config,
                );
                match result {
                    Ok(result) => {
                        let provider_thread_id = result.agent_session_id.clone();
                        self.core_runtime
                            .lock()
                            .map_err(|_| {
                                PedelecError::new(
                                    error_codes::CORE_RUNTIME_UNAVAILABLE,
                                    "Core runtime mutex was poisoned",
                                )
                            })?
                            .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                                thread_id: thread_id.clone(),
                                provider_session_id: provider_thread_id.clone(),
                            })?;
                        record_runtime_diagnostic(
                            &self.core_runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                                provider: self.provider.clone(),
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id,
                                provider_thread_id,
                                resumed: result.resumed,
                            },
                        );
                        Ok(())
                    }
                    Err(error) => Err(map_agent_error(
                        error,
                        Some(&controller),
                        &self.provider,
                        &self.product,
                        &thread_id,
                        session.provider_session_id.as_deref(),
                        None,
                        "admission",
                    )),
                }
            }
            PersistentRuntimeOperation::StartTurn { turn } => {
                let config = session_config(&turn.session)?;
                let session = controller
                    .ensure_session(&thread_id, turn.provider_session_id.as_deref(), &config)
                    .map_err(|error| {
                        map_agent_error(
                            error,
                            Some(&controller),
                            &self.provider,
                            &self.product,
                            &thread_id,
                            turn.provider_session_id.as_deref(),
                            None,
                            "admission",
                        )
                    })?;
                if let Err(error) =
                    reduce_session_ready(&self.core_runtime, &thread_id, &session.agent_session_id)
                {
                    return Err(error);
                }
                record_runtime_diagnostic(
                    &self.core_runtime,
                    ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                        provider: self.provider.clone(),
                        runtime_generation: controller.generation(),
                        process_id: controller.process_id(),
                        thread_id: thread_id.clone(),
                        provider_thread_id: session.agent_session_id.clone(),
                        resumed: session.resumed,
                    },
                );
                controller
                    .start_turn(
                        &thread_id,
                        &session.agent_session_id,
                        &turn.local_turn_id,
                        &turn.message,
                    )
                    .map_err(|error| {
                        map_agent_error(
                            error,
                            Some(&controller),
                            &self.provider,
                            &self.product,
                            &thread_id,
                            Some(&session.agent_session_id),
                            None,
                            "admission",
                        )
                    })?;
                Ok(())
            }
            PersistentRuntimeOperation::EndSession { .. } => Ok(()),
        }
    }
}

pub(crate) fn resolve_pedelec_agent_program(
    program_override: Option<&PathBuf>,
) -> Result<PathBuf, PedelecError> {
    if let Some(program) = program_override {
        if program.as_os_str().is_empty() {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_RUNTIME_START_FAILED,
                "Pedelec Agent executable path is empty",
                json!({ "provider": "pedelec-agent" }),
            ));
        }
        return Ok(program.clone());
    }
    let path = pedelec_shared::paths::pedelec_agent_install_path().map_err(|error| {
        PedelecError::with_details(
            error_codes::PROVIDER_RUNTIME_START_FAILED,
            "Pedelec Agent executable path could not be resolved",
            json!({
                "provider": "pedelec-agent",
                "error": error.message,
            }),
        )
    })?;
    if !path.is_file() {
        return Err(PedelecError::with_details(
            error_codes::PROVIDER_RUNTIME_START_FAILED,
            "Pedelec Agent executable was not found",
            json!({
                "provider": "pedelec-agent",
                "executablePath": path_for_external_use(&path),
                "binaryName": pedelec_shared::paths::pedelec_agent_binary_name(),
            }),
        ));
    }
    Ok(path)
}

fn build_launch_config(
    program: PathBuf,
    process_cwd: PathBuf,
    provider: &ProviderCode,
    product: &str,
    settings: &PedelecSettings,
    runtime_file: &Path,
    env_overrides: &[(OsString, OsString)],
) -> Result<PedelecAgentRuntimeLaunchConfig, PedelecError> {
    let mut launch = PedelecAgentRuntimeLaunchConfig::new(program, process_cwd, product);
    launch = launch.with_env("PEDELEC_PROVIDER", product);
    launch = launch.with_env(
        "PEDELEC_CORE_IPC_RUNTIME_FILE",
        runtime_file.to_string_lossy().into_owned(),
    );
    if let Some(path) = std::env::var_os("PATH") {
        launch = launch.with_env("PATH", path);
    }
    if let Ok(cli_path) = pedelec_shared::paths::pedelec_tool_install_path() {
        if cli_path.is_file() {
            launch = launch.with_env("PEDELEC_CLI_PATH", cli_path.into_os_string());
        }
    }
    if *provider == ProviderCode::Ollama {
        let api_key = settings.provider_settings.ollama.api_key.trim();
        if api_key.is_empty() {
            return Err(PedelecError::with_details(
                error_codes::OLLAMA_API_KEY_REQUIRED,
                "Ollama API key is required",
                json!({ "provider": "ollama" }),
            ));
        }
        launch = launch.with_env("OLLAMA_API_KEY", api_key);
        let tavily = settings.provider_settings.ollama.tavily_api_key.trim();
        if !tavily.is_empty() {
            launch = launch.with_env("TAVILY_API_KEY", tavily);
        }
    }
    for (key, value) in env_overrides {
        launch = launch.with_env(key.clone(), value.clone());
    }
    Ok(launch)
}

fn session_config(
    session: &PersistentProviderSessionIntent,
) -> Result<PedelecAgentSessionConfig, PedelecError> {
    let model = session
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            PedelecError::with_details(
                error_codes::MODEL_REQUIRED,
                "Ollama provider requires a model.",
                json!({
                    "provider": session.provider,
                    "threadId": session.thread_id,
                }),
            )
        })?;
    Ok(PedelecAgentSessionConfig {
        model: model.to_string(),
        workspace: session.workspace_path.clone(),
        host_instructions: {
            let host = session.host_instructions.trim();
            if host.is_empty() {
                None
            } else {
                Some(session.host_instructions.clone())
            }
        },
    })
}

fn reduce_session_ready(
    runtime: &SharedCoreRuntime,
    thread_id: &str,
    provider_thread_id: &str,
) -> Result<(), PedelecError> {
    runtime
        .lock()
        .map_err(|_| {
            PedelecError::new(
                error_codes::CORE_RUNTIME_UNAVAILABLE,
                "Core runtime mutex was poisoned",
            )
        })?
        .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
            thread_id: thread_id.to_string(),
            provider_session_id: provider_thread_id.to_string(),
        })
}

fn record_runtime_diagnostic(runtime: &SharedCoreRuntime, diagnostic: ProviderRuntimeDiagnostic) {
    if let Ok(mut core) = runtime.lock() {
        core.record_provider_runtime_diagnostic(diagnostic);
    }
}

fn mark_runtime_stopped(
    runtime: &SharedCoreRuntime,
    stopped_generations: &Mutex<HashSet<u64>>,
    provider: ProviderCode,
    generation: u64,
    process_id: u32,
    reason: &str,
) {
    let should_emit = stopped_generations
        .lock()
        .map(|mut generations| {
            if !generations.insert(generation) {
                return false;
            }
            if generations.len() > 2048 {
                generations.clear();
                generations.insert(generation);
            }
            true
        })
        .unwrap_or(true);
    if should_emit {
        record_runtime_diagnostic(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStopped {
                provider,
                runtime_generation: generation,
                process_id,
                reason: reason.to_string(),
            },
        );
    }
}

fn turn_status_label(status: PedelecAgentTurnStatus) -> &'static str {
    match status {
        PedelecAgentTurnStatus::Completed => "completed",
        PedelecAgentTurnStatus::Failed => "failed",
    }
}

fn truncate_diagnostic_text(text: &str) -> String {
    if text.len() <= MAX_RUNTIME_DIAGNOSTIC_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RUNTIME_DIAGNOSTIC_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn mutex_error(name: &str) -> PedelecError {
    PedelecError::new(
        error_codes::PROVIDER_PROTOCOL_ERROR,
        format!("{name} mutex was poisoned"),
    )
}

fn registry_error(
    error: RuntimeRegistryError,
    provider: &ProviderCode,
    product: &str,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_START_FAILED,
        "Pedelec Agent runtime could not be started",
        json!({
            "provider": provider,
            "product": product,
            "operation": "startup",
            "error": error.to_string(),
        }),
    )
}

fn map_agent_error(
    error: PedelecAgentRuntimeError,
    controller: Option<&PedelecAgentServerController>,
    provider: &ProviderCode,
    product: &str,
    thread_id: &str,
    provider_thread_id: Option<&str>,
    provider_turn_id: Option<&str>,
    stage: &str,
) -> PedelecError {
    let (code, operation, rpc_details) = match &error {
        PedelecAgentRuntimeError::RuntimeStart { operation, .. } => {
            (error_codes::PROVIDER_RUNTIME_START_FAILED, operation, None)
        }
        PedelecAgentRuntimeError::RuntimeDisconnected { operation, .. } => {
            (error_codes::PROVIDER_RUNTIME_DISCONNECTED, operation, None)
        }
        PedelecAgentRuntimeError::Protocol { operation, .. } => {
            (error_codes::PROVIDER_PROTOCOL_ERROR, operation, None)
        }
        PedelecAgentRuntimeError::Request {
            operation, details, ..
        } => {
            let agent_code = agent_error_code(details.as_ref());
            let code = PRESERVED_AGENT_ERROR_CODES
                .iter()
                .copied()
                .find(|preserved| agent_code.as_deref() == Some(*preserved))
                .unwrap_or(error_codes::PROVIDER_REQUEST_FAILED);
            (code, operation, details.clone())
        }
    };
    let mut details = json!({
        "provider": provider,
        "product": product,
        "operation": operation,
        "stage": stage,
        "threadId": thread_id,
        "error": error.to_string(),
    });
    if let Some(provider_thread_id) = provider_thread_id {
        details["providerThreadId"] = json!(provider_thread_id);
    }
    if let Some(provider_turn_id) = provider_turn_id {
        details["providerTurnId"] = json!(provider_turn_id);
    }
    if let Some(rpc_details) = rpc_details {
        details["rpc"] = rpc_details.clone();
        if let Some(agent_code) = agent_error_code(Some(&rpc_details)) {
            details["agentCode"] = json!(agent_code);
        }
    }
    if let Some(controller) = controller {
        details["runtimeGeneration"] = json!(controller.generation());
        details["processId"] = json!(controller.process_id());
    }
    PedelecError::with_details(code, error.to_string(), details)
}

fn agent_error_code(details: Option<&Value>) -> Option<String> {
    let details = details?;
    details
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .or_else(|| details.get("code").and_then(Value::as_str))
        .map(str::to_string)
}

fn turn_completion_error(
    provider: &ProviderCode,
    product: &str,
    thread_id: &str,
    provider_thread_id: &str,
    provider_turn_id: Option<&str>,
    error: Option<Value>,
    runtime_generation: u64,
    process_id: u32,
) -> PedelecError {
    let message = error
        .as_ref()
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Pedelec Agent turn failed".to_string());
    let agent_code = error
        .as_ref()
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let code = PRESERVED_AGENT_ERROR_CODES
        .iter()
        .copied()
        .find(|preserved| agent_code.as_deref() == Some(*preserved))
        .unwrap_or(error_codes::PROVIDER_REQUEST_FAILED);
    let mut details = json!({
        "provider": provider,
        "product": product,
        "threadId": thread_id,
        "providerThreadId": provider_thread_id,
        "runtimeGeneration": runtime_generation,
        "processId": process_id,
    });
    if let Some(provider_turn_id) = provider_turn_id {
        details["providerTurnId"] = json!(provider_turn_id);
    }
    if let Some(agent_code) = agent_code {
        details["agentCode"] = json!(agent_code);
    }
    if let Some(error) = error {
        details["rpc"] = error;
    }
    PedelecError::with_details(code, message, details)
}

fn end_disconnect_error(
    session: &PersistentProviderEndIntent,
    provider: &ProviderCode,
    product: &str,
    message: &str,
    controller: Option<&PedelecAgentServerController>,
) -> PedelecError {
    let mut details = json!({
        "provider": provider,
        "product": product,
        "operation": "end",
        "threadId": session.thread_id,
        "providerThreadId": session.provider_session_id,
        "providerTurnId": session.active_provider_turn_id,
    });
    if let Some(controller) = controller {
        details["runtimeGeneration"] = json!(controller.generation());
        details["processId"] = json!(controller.process_id());
    }
    PedelecError::with_details(error_codes::PROVIDER_RUNTIME_DISCONNECTED, message, details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderRuntimeDispatcher;
    use pedelec_core::{
        CreateThreadInput, CreateThreadSkillsInput, CreateThreadWorkspaceInput, EffortLevel,
        EffortsArgs, EndThreadExecutionIntent, EndThreadInput, OllamaProviderSettingsInput,
        PrepareThreadInput, ProviderExecutionIntent, ProviderExecutionOperationKind,
        ProviderSettingsInput, SendTextInput, ThreadEvent, ThreadStatus, UpdateSettingsInput,
        WorkspaceManager,
    };
    use std::fs;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn production_resolution_uses_installed_pedelec_agent_binary() {
        let expected = pedelec_shared::paths::pedelec_agent_install_path().unwrap();
        match resolve_pedelec_agent_program(None) {
            Ok(path) => {
                assert_eq!(path, expected);
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(pedelec_shared::paths::pedelec_agent_binary_name())
                );
            }
            Err(error) => {
                assert_eq!(error.code, error_codes::PROVIDER_RUNTIME_START_FAILED);
                let details = error
                    .details
                    .expect("missing executable should include path");
                assert_eq!(
                    details["executablePath"],
                    json!(path_for_external_use(&expected))
                );
                assert_eq!(
                    details["binaryName"],
                    json!(pedelec_shared::paths::pedelec_agent_binary_name())
                );
            }
        }
    }

    #[test]
    fn ollama_routes_through_shared_pedelec_agent_generation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let first = create_thread(&runtime, &temp.path().join("workspace-a"), "first");
        let second = create_thread(&runtime, &temp.path().join("workspace-b"), "second");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_provider_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &first);
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        let first_session = provider_session_id(&runtime, &first).unwrap();
        assert!(first_session.starts_with("agent-session-"));
        let pid = dispatcher
            .pedelec_agent_ollama_controller()
            .unwrap()
            .process_id();

        dispatch_prepare(&dispatcher, &runtime, &second);
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
        assert_eq!(
            dispatcher
                .pedelec_agent_ollama_controller()
                .unwrap()
                .process_id(),
            pid
        );
        assert_ne!(
            provider_session_id(&runtime, &second).unwrap(),
            first_session
        );

        let diagnostics = runtime
            .lock()
            .unwrap()
            .provider_runtime_diagnostic_history();
        assert!(diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                provider: ProviderCode::Ollama,
                thread_id,
                resumed: false,
                ..
            } if thread_id == &first
        )));
        assert!(!diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted { thread_id, .. }
                if thread_id == &first
        )));
        let _ = owner.shutdown();
    }

    #[test]
    fn ensure_session_and_start_turn_map_events_without_synthetic_prepare() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "guidance");
        let stdin_log = temp.path().join("stdin.jsonl");
        let events = runtime.lock().unwrap().subscribe_all_threads();
        let traffic = runtime
            .lock()
            .unwrap()
            .subscribe_provider_protocol_traffic();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let session_id = provider_session_id(&runtime, &thread_id).unwrap();
        let emitted = collect_thread_events(&events);
        assert!(!emitted
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantDelta { .. })));
        assert!(!emitted
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantMessage { .. })));

        dispatch_turn(&dispatcher, &runtime, &thread_id, "hello from host");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some(session_id.as_str())
        );

        let emitted = collect_thread_events(&events);
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { text, .. } if text == "hello"
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantMessage { text, .. } if text == "hello world"
        )));
        assert!(emitted
            .iter()
            .any(|event| matches!(event, ThreadEvent::Done { .. })));
        assert!(!emitted.iter().any(|event| match event {
            ThreadEvent::ToolCall { .. } | ThreadEvent::ToolResult { .. } => true,
            _ => event_mentions_tool(event),
        }));

        let frames = stdin_frames(&stdin_log);
        assert!(frames.iter().any(|frame| frame["method"] == "session/open"));
        assert!(frames.iter().any(|frame| frame["method"] == "turn/start"));
        assert!(frames.iter().all(|frame| {
            !frame.to_string().contains("OLLAMA_API_KEY")
                && !frame.to_string().contains("secret-key")
        }));
        let traffic_records = collect_protocol_traffic(traffic, 2);
        assert!(traffic_records
            .iter()
            .any(|record| record.provider == ProviderCode::Ollama));
        assert!(traffic_records.iter().all(|record| {
            !record.message.to_string().contains("secret-key")
                && !record.message.to_string().contains("tavily-secret")
        }));

        let diagnostics = runtime
            .lock()
            .unwrap()
            .provider_runtime_diagnostic_history();
        assert!(diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                provider: ProviderCode::Ollama,
                thread_id: started_thread,
                ..
            } if started_thread == &thread_id
        )));
        assert!(diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                provider: ProviderCode::Ollama,
                status,
                ..
            } if status == "completed"
        )));
        let _ = owner.shutdown();
    }

    #[test]
    fn start_turn_resumes_exact_session_after_generation_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "resume");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let session_id = provider_session_id(&runtime, &thread_id).unwrap();
        let first_pid = dispatcher.current_controller().unwrap().process_id();
        dispatcher.retire("test replacement").unwrap();
        assert!(dispatcher.current_controller().is_none());

        dispatch_turn(
            &dispatcher,
            &runtime,
            &thread_id,
            "continue after replacement",
        );
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some(session_id.as_str())
        );
        let second_pid = dispatcher.current_controller().unwrap().process_id();
        assert_ne!(first_pid, second_pid);
        let frames = stdin_frames(&stdin_log);
        assert!(frames.iter().any(|frame| {
            frame["method"] == "session/open" && frame["params"]["sessionId"] == session_id
        }));
        let _ = owner.shutdown();
    }

    #[test]
    fn disconnect_fails_active_work_and_keeps_idle_session_identity() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let idle = create_thread(&runtime, &temp.path().join("idle"), "idle");
        let active = create_thread(&runtime, &temp.path().join("active"), "active");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "already-started");

        dispatch_prepare(&dispatcher, &runtime, &idle);
        wait_for_status(&runtime, &idle, ThreadStatus::Idle);
        let idle_session = provider_session_id(&runtime, &idle).unwrap();
        dispatch_turn(&dispatcher, &runtime, &active, "keep running");
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&active) == Some(ThreadStatus::Running)
        });

        dispatcher.current_controller().unwrap().retire();
        wait_for_status(&runtime, &active, ThreadStatus::Error);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&idle),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            provider_session_id(&runtime, &idle).as_deref(),
            Some(idle_session.as_str())
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn unhealthy_controller_is_failed_before_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let active = create_thread(&runtime, &temp.path().join("active"), "active");
        let next = create_thread(&runtime, &temp.path().join("next"), "next");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "already-started");

        dispatch_turn(&dispatcher, &runtime, &active, "stay running");
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&active) == Some(ThreadStatus::Running)
        });
        dispatcher.current_controller().unwrap().retire();
        dispatch_prepare(&dispatcher, &runtime, &next);
        wait_for_status(&runtime, &next, ThreadStatus::Idle);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&active),
            Some(ThreadStatus::Error)
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn idle_detached_end_does_not_spawn_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "idle end");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &temp.path().join("stdin.jsonl"),
        );
        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Ollama end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime
            .lock()
            .unwrap()
            .finish_end_thread(&thread_id)
            .unwrap();
        assert!(dispatcher.current_controller().is_none());
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Ended)
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn healthy_close_and_ambiguous_close_do_not_leave_stopping() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let healthy = create_thread(&runtime, &temp.path().join("healthy"), "healthy");
        let ambiguous = create_thread(&runtime, &temp.path().join("ambiguous"), "ambiguous");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &healthy);
        wait_for_status(&runtime, &healthy, ThreadStatus::Idle);
        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: healthy.clone(),
            })
            .unwrap();
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Ollama end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime.lock().unwrap().finish_end_thread(&healthy).unwrap();
        assert_eq!(
            runtime.lock().unwrap().thread_status(&healthy),
            Some(ThreadStatus::Ended)
        );
        assert!(dispatcher.current_controller().is_some());

        dispatcher.retire("switch close mode").unwrap();
        let dispatcher = dispatcher.with_env_for_test("FAKE_PEDELEC_AGENT_CLOSE_MODE", "malformed");
        dispatch_prepare(&dispatcher, &runtime, &ambiguous);
        wait_for_status(&runtime, &ambiguous, ThreadStatus::Idle);
        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: ambiguous.clone(),
            })
            .unwrap();
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Ollama end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime
            .lock()
            .unwrap()
            .finish_end_thread(&ambiguous)
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().thread_status(&ambiguous),
            Some(ThreadStatus::Ended)
        );
        assert!(dispatcher.current_controller().is_none());
        let _ = owner.shutdown();
    }

    #[test]
    fn ollama_settings_changes_retire_generation_lazily() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "settings");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_provider_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let first_pid = dispatcher
            .pedelec_agent_ollama_controller()
            .unwrap()
            .process_id();
        let unchanged = current_update_input(&runtime);
        dispatcher.update_settings(&runtime, unchanged).unwrap();
        assert_eq!(
            dispatcher
                .pedelec_agent_ollama_controller()
                .unwrap()
                .process_id(),
            first_pid
        );

        for mutate in [
            mutate_base_url,
            mutate_timeout,
            mutate_api_key,
            mutate_tavily,
            mutate_effort,
        ] {
            let mut input = current_update_input(&runtime);
            mutate(&mut input);
            dispatcher.update_settings(&runtime, input).unwrap();
            assert!(
                dispatcher.pedelec_agent_ollama_controller().is_none(),
                "settings change should retire without spawning a replacement"
            );
            dispatch_turn(&dispatcher, &runtime, &thread_id, "resume after settings");
            wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
            let next_pid = dispatcher
                .pedelec_agent_ollama_controller()
                .unwrap()
                .process_id();
            assert_ne!(next_pid, first_pid);
        }
        let _ = owner.shutdown();
    }

    #[test]
    fn missing_program_override_does_not_use_ollama_terminal_api() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "missing binary");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            PedelecAgentRuntimeDispatcher::for_ollama(owner.clone(), Arc::clone(&runtime))
                .with_program_for_test(temp.path().join("missing-pedelec-agent"));
        let start = runtime
            .lock()
            .unwrap()
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        let Some(ProviderExecutionIntent::PersistentRuntime { operation }) = start.intent else {
            panic!("Ollama prepare should produce a persistent operation");
        };
        let error = dispatcher.dispatch(operation).unwrap_err();
        runtime.lock().unwrap().fail_provider_execution_dispatch(
            &thread_id,
            ProviderExecutionOperationKind::Prepare,
            error.clone(),
        );
        assert_eq!(error.code, error_codes::PROVIDER_RUNTIME_START_FAILED);
        assert_ne!(error.code, error_codes::PROVIDER_TERMINAL_UNSUPPORTED);
        assert!(dispatcher.current_controller().is_none());
        let _ = owner.shutdown();
    }

    #[test]
    fn second_turn_reuses_generation_without_session_open() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "second");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let session_id = provider_session_id(&runtime, &thread_id).unwrap();
        let pid = dispatcher.current_controller().unwrap().process_id();
        dispatch_turn(&dispatcher, &runtime, &thread_id, "first");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &thread_id, "second");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);

        assert_eq!(dispatcher.current_controller().unwrap().process_id(), pid);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some(session_id.as_str())
        );
        let opens = stdin_frames(&stdin_log)
            .into_iter()
            .filter(|frame| frame["method"] == "session/open")
            .count();
        assert_eq!(opens, 1);
        let _ = owner.shutdown();
    }

    #[test]
    fn parallel_sessions_share_generation_and_keep_owners() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let first = create_thread(&runtime, &temp.path().join("a"), "a");
        let second = create_thread(&runtime, &temp.path().join("b"), "b");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "already-started");

        dispatch_prepare(&dispatcher, &runtime, &first);
        dispatch_prepare(&dispatcher, &runtime, &second);
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
        let first_session = provider_session_id(&runtime, &first).unwrap();
        let second_session = provider_session_id(&runtime, &second).unwrap();
        assert_ne!(first_session, second_session);
        let pid = dispatcher.current_controller().unwrap().process_id();

        thread::scope(|scope| {
            scope.spawn(|| dispatch_turn(&dispatcher, &runtime, &first, "a-turn"));
            scope.spawn(|| dispatch_turn(&dispatcher, &runtime, &second, "b-turn"));
        });
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&first) == Some(ThreadStatus::Running)
                && runtime.lock().unwrap().thread_status(&second) == Some(ThreadStatus::Running)
        });
        assert_eq!(dispatcher.current_controller().unwrap().process_id(), pid);
        let frames = stdin_frames(&stdin_log);
        let first_turns = frames
            .iter()
            .filter(|frame| frame["method"] == "turn/start" && frame["params"]["threadId"] == first)
            .count();
        let second_turns = frames
            .iter()
            .filter(|frame| {
                frame["method"] == "turn/start" && frame["params"]["threadId"] == second
            })
            .count();
        assert_eq!(first_turns, 1);
        assert_eq!(second_turns, 1);
        let _ = owner.shutdown();
    }

    #[test]
    fn generation_crash_resumes_idle_session_on_next_turn() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let idle = create_thread(&runtime, &temp.path().join("idle"), "idle");
        let active = create_thread(&runtime, &temp.path().join("active"), "active");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "already-started");

        dispatch_prepare(&dispatcher, &runtime, &idle);
        dispatch_prepare(&dispatcher, &runtime, &active);
        wait_for_status(&runtime, &idle, ThreadStatus::Idle);
        wait_for_status(&runtime, &active, ThreadStatus::Idle);
        let idle_session = provider_session_id(&runtime, &idle).unwrap();
        dispatch_turn(&dispatcher, &runtime, &active, "keep running");
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&active) == Some(ThreadStatus::Running)
        });
        let first_pid = dispatcher.current_controller().unwrap().process_id();
        dispatcher.current_controller().unwrap().retire();
        wait_for_status(&runtime, &active, ThreadStatus::Error);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&idle),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            provider_session_id(&runtime, &idle).as_deref(),
            Some(idle_session.as_str())
        );

        let disconnected = runtime
            .lock()
            .unwrap()
            .provider_runtime_diagnostic_history()
            .into_iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic,
                    ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected { process_id, .. }
                        if *process_id == first_pid
                )
            })
            .count();
        assert!(disconnected >= 1);

        let resume = test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);
        dispatch_turn(&resume, &runtime, &idle, "continue idle");
        wait_for_status(&runtime, &idle, ThreadStatus::Idle);
        assert_ne!(resume.current_controller().unwrap().process_id(), first_pid);
        assert!(stdin_frames(&stdin_log).iter().any(|frame| {
            frame["method"] == "session/open" && frame["params"]["sessionId"] == idle_session
        }));
        let _ = owner.shutdown();
    }

    #[test]
    fn settings_retire_active_and_idle_then_spawn_lazily() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let idle = create_thread(&runtime, &temp.path().join("idle"), "idle");
        let active = create_thread(&runtime, &temp.path().join("active"), "active");
        let stdin_log = temp.path().join("stdin.jsonl");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_provider_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_pedelec_agent_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "already-started");

        dispatch_prepare(&dispatcher, &runtime, &idle);
        dispatch_prepare(&dispatcher, &runtime, &active);
        wait_for_status(&runtime, &idle, ThreadStatus::Idle);
        wait_for_status(&runtime, &active, ThreadStatus::Idle);
        let idle_session = provider_session_id(&runtime, &idle).unwrap();
        dispatch_turn(&dispatcher, &runtime, &active, "keep running");
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&active) == Some(ThreadStatus::Running)
        });
        let first_pid = dispatcher
            .pedelec_agent_ollama_controller()
            .unwrap()
            .process_id();

        let mut input = current_update_input(&runtime);
        mutate_base_url(&mut input);
        dispatcher.update_settings(&runtime, input).unwrap();
        assert!(dispatcher.pedelec_agent_ollama_controller().is_none());
        wait_for_status(&runtime, &active, ThreadStatus::Error);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&idle),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            provider_session_id(&runtime, &idle).as_deref(),
            Some(idle_session.as_str())
        );

        let resume = test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log);
        dispatch_turn(&resume, &runtime, &idle, "after settings");
        wait_for_status(&runtime, &idle, ThreadStatus::Idle);
        assert_ne!(resume.current_controller().unwrap().process_id(), first_pid);
        let _ = owner.shutdown();
    }

    #[test]
    fn close_pending_turn_drops_stale_events_without_protocol_error() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "pending close");
        let stdin_log = temp.path().join("stdin.jsonl");
        let events = runtime.lock().unwrap().subscribe_all_threads();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "pending");

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &thread_id, "hold");
        wait_until(Duration::from_secs(8), || {
            runtime.lock().unwrap().thread_status(&thread_id) == Some(ThreadStatus::Running)
        });
        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Ollama end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime
            .lock()
            .unwrap()
            .finish_end_thread(&thread_id)
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Ended)
        );
        thread::sleep(Duration::from_millis(250));
        let emitted = collect_thread_events(&events);
        assert!(!emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { error, .. }
                if error.code == error_codes::PROVIDER_PROTOCOL_ERROR
        )));
        assert!(!emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { text, .. } if text == "stale"
        )));
        let _ = owner.shutdown();
    }

    #[test]
    fn rpc_traffic_is_owner_scoped_and_omits_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let first = create_thread(&runtime, &temp.path().join("a"), "a");
        let second = create_thread(&runtime, &temp.path().join("b"), "b");
        let stdin_log = temp.path().join("stdin.jsonl");
        let traffic = runtime
            .lock()
            .unwrap()
            .subscribe_provider_protocol_traffic();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "multi-round");

        dispatch_prepare(&dispatcher, &runtime, &first);
        dispatch_prepare(&dispatcher, &runtime, &second);
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &first, "round trip");
        wait_for_status(&runtime, &first, ThreadStatus::Idle);

        let records = collect_protocol_traffic(traffic, 8);
        assert!(records.iter().any(|record| {
            record.thread_id.is_none() && record.message["method"] == "initialize"
        }));
        assert!(records.iter().any(|record| {
            record.thread_id.as_deref() == Some(first.as_str())
                && record.message["method"] == "session/open"
        }));
        assert!(records.iter().any(|record| {
            record.thread_id.as_deref() == Some(first.as_str())
                && record.kind == "notification"
                && record.message["method"] == "turn/tool_call"
        }));
        assert!(records.iter().any(|record| {
            record.thread_id.as_deref() == Some(first.as_str())
                && record.kind == "notification"
                && record.message["method"] == "turn/tool_result"
        }));
        assert!(records.iter().all(|record| {
            if record.message["method"] == "session/open" {
                record.thread_id.as_deref() != Some(second.as_str())
                    || record.message["params"]["threadId"] == second
            } else {
                true
            }
        }));
        let serialized = serde_json::to_string(&records).unwrap();
        assert!(!serialized.contains("secret-key"));
        assert!(!serialized.contains("tavily-secret"));
        assert!(!serialized.contains("OLLAMA_API_KEY"));
        let _ = owner.shutdown();
    }

    #[test]
    fn multi_round_streaming_keeps_delta_and_final_message_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &temp.path().join("workspace"), "multi-round");
        let stdin_log = temp.path().join("stdin.jsonl");
        let events = runtime.lock().unwrap().subscribe_all_threads();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher =
            test_dispatcher(owner.clone(), Arc::clone(&runtime), temp.path(), &stdin_log)
                .with_env_for_test("FAKE_PEDELEC_AGENT_TURN_MODE", "multi-round");

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let prepare_events = collect_thread_events(&events);
        assert!(!prepare_events
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantDelta { .. })));
        assert!(!prepare_events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Done { .. })));

        dispatch_turn(&dispatcher, &runtime, &thread_id, "hello");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let emitted = collect_thread_events(&events);
        let deltas = emitted
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::AssistantDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, ["thinking ", "final"]);
        let messages = emitted
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::AssistantMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["final answer"]);
        assert!(!emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantMessage { text, .. } if text.contains("thinking")
        )));
        let usage = runtime
            .lock()
            .unwrap()
            .provider_usage(&thread_id)
            .cloned()
            .expect("cumulative usage");
        assert_eq!(usage["totalTokens"], json!(7));
        assert!(emitted
            .iter()
            .any(|event| matches!(event, ThreadEvent::Done { .. })));
        let _ = owner.shutdown();
    }

    fn test_runtime(temp: &Path) -> SharedCoreRuntime {
        let mut core = pedelec_core::CoreRuntime::new_for_application();
        core.workspace_manager = WorkspaceManager::with_workspace_root(temp.join("managed"));
        core.settings_file_path = Some(temp.join("settings.json"));
        core.set_core_ipc_runtime("127.0.0.1:1", temp.join("runtime.json"));
        let runtime = Arc::new(Mutex::new(core));
        runtime
            .lock()
            .unwrap()
            .update_settings(ollama_settings_input(
                "http://127.0.0.1:11434",
                120_000,
                "secret-key",
                "",
                vec!["--model".into(), "qwen3:8b".into()],
            ))
            .unwrap();
        runtime
    }

    fn create_thread(runtime: &SharedCoreRuntime, workspace: &Path, guidance: &str) -> String {
        fs::create_dir_all(workspace).unwrap();
        runtime
            .lock()
            .unwrap()
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Ollama,
                effort_level: Some(EffortLevel::Default),
                skills: Some(CreateThreadSkillsInput {
                    guidance: guidance.to_string(),
                    tools: vec![],
                }),
                workspace: Some(CreateThreadWorkspaceInput {
                    path: workspace.to_path_buf(),
                }),
            })
            .unwrap()
            .thread_id
    }

    fn test_dispatcher(
        owner: ProviderRuntimeOwner,
        runtime: SharedCoreRuntime,
        temp: &Path,
        stdin_log: &Path,
    ) -> PedelecAgentRuntimeDispatcher {
        PedelecAgentRuntimeDispatcher::for_ollama(owner, runtime)
            .with_program_for_test(fake_program(temp))
            .with_env_for_test("FAKE_PEDELEC_AGENT_LOG", stdin_log.as_os_str())
            .with_process_cwd_for_test(temp)
    }

    fn test_provider_dispatcher(
        owner: ProviderRuntimeOwner,
        runtime: SharedCoreRuntime,
        temp: &Path,
        stdin_log: &Path,
    ) -> ProviderRuntimeDispatcher {
        ProviderRuntimeDispatcher::new(owner, runtime)
            .with_pedelec_agent_program_for_test(fake_program(temp))
            .with_pedelec_agent_env_for_test("FAKE_PEDELEC_AGENT_LOG", stdin_log.as_os_str())
    }

    fn dispatch_prepare(
        dispatcher: &impl PersistentRuntimeDispatcher,
        runtime: &SharedCoreRuntime,
        thread_id: &str,
    ) {
        let start = runtime
            .lock()
            .unwrap()
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.to_string(),
            })
            .unwrap();
        let Some(ProviderExecutionIntent::PersistentRuntime { operation }) = start.intent else {
            panic!("Ollama prepare should produce a persistent operation");
        };
        dispatcher.dispatch(operation).unwrap();
    }

    fn dispatch_turn(
        dispatcher: &impl PersistentRuntimeDispatcher,
        runtime: &SharedCoreRuntime,
        thread_id: &str,
        message: &str,
    ) {
        let start = runtime
            .lock()
            .unwrap()
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.to_string(),
                message: message.to_string(),
            })
            .unwrap();
        let ProviderExecutionIntent::PersistentRuntime { operation } = start.intent else {
            panic!("Ollama turn should produce a persistent operation");
        };
        dispatcher.dispatch(operation).unwrap();
    }

    fn wait_for_status(runtime: &SharedCoreRuntime, thread_id: &str, expected: ThreadStatus) {
        wait_until(Duration::from_secs(10), || {
            runtime.lock().unwrap().thread_status(thread_id) == Some(expected.clone())
        });
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(predicate(), "condition was not satisfied before timeout");
    }

    fn provider_session_id(runtime: &SharedCoreRuntime, thread_id: &str) -> Option<String> {
        runtime
            .lock()
            .unwrap()
            .thread_manager
            .provider_session_state(thread_id)
            .and_then(|state| state.provider_session_id.clone())
    }

    fn stdin_frames(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn collect_thread_events(
        receiver: &std::sync::mpsc::Receiver<ThreadEvent>,
    ) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    fn collect_protocol_traffic(
        receiver: std::sync::mpsc::Receiver<pedelec_core::ProviderProtocolTraffic>,
        minimum: usize,
    ) -> Vec<pedelec_core::ProviderProtocolTraffic> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut records = Vec::new();
        while records.len() < minimum && Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(record) => records.push(record),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        records.extend(receiver.try_iter());
        records
    }

    fn event_mentions_tool(event: &ThreadEvent) -> bool {
        format!("{event:?}").contains("read_file")
    }

    fn current_update_input(runtime: &SharedCoreRuntime) -> UpdateSettingsInput {
        let settings = runtime.lock().unwrap().get_settings().unwrap();
        ollama_settings_input(
            &settings.provider_settings.ollama.base_url,
            settings.provider_settings.ollama.timeout_ms,
            &settings.provider_settings.ollama.api_key,
            &settings.provider_settings.ollama.tavily_api_key,
            settings
                .provider_settings
                .ollama
                .efforts_args
                .default
                .clone(),
        )
    }

    fn ollama_settings_input(
        base_url: &str,
        timeout_ms: u64,
        api_key: &str,
        tavily_api_key: &str,
        default_effort: Vec<String>,
    ) -> UpdateSettingsInput {
        UpdateSettingsInput {
            default_provider: ProviderCode::Ollama,
            provider_settings: ProviderSettingsInput {
                ollama: OllamaProviderSettingsInput {
                    base_url: Some(base_url.to_string()),
                    timeout_ms: Some(timeout_ms),
                    api_key: Some(api_key.to_string()),
                    tavily_api_key: Some(tavily_api_key.to_string()),
                    efforts_args: EffortsArgs {
                        default: default_effort,
                        ..EffortsArgs::default()
                    },
                },
                ..ProviderSettingsInput::default()
            },
        }
    }

    fn mutate_base_url(input: &mut UpdateSettingsInput) {
        input.provider_settings.ollama.base_url = Some("http://127.0.0.1:11435".into());
    }

    fn mutate_timeout(input: &mut UpdateSettingsInput) {
        input.provider_settings.ollama.timeout_ms = Some(12_345);
    }

    fn mutate_api_key(input: &mut UpdateSettingsInput) {
        input.provider_settings.ollama.api_key = Some("rotated-key".into());
    }

    fn mutate_tavily(input: &mut UpdateSettingsInput) {
        input.provider_settings.ollama.tavily_api_key = Some("tavily-secret".into());
    }

    fn mutate_effort(input: &mut UpdateSettingsInput) {
        input.provider_settings.ollama.efforts_args.default =
            vec!["--model".into(), "qwen3:14b".into()];
    }

    fn fake_program(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let target = directory.join("fake-pedelec-agent.cmd");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_pedelec_agent.ps1");
            fs::write(
                &target,
                format!(
                    "@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" %*\r\n",
                    fixture.display()
                ),
            )
            .unwrap();
            target
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let target = directory.join("fake-pedelec-agent");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_pedelec_agent.sh");
            fs::write(
                &target,
                format!("#!/bin/sh\nexec sh '{}' \"$@\"\n", fixture.display()),
            )
            .unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            target
        }
    }
}
