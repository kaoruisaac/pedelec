use crate::{record_protocol_traffic, PersistentRuntimeDispatcher};
use pedelec_core::{
    build_persistent_prepare_prompt, build_persistent_user_prompt_with_bootstrap, error_codes,
    AntigravityReasoningEffort as CoreAntigravityReasoningEffort, PedelecError,
    PersistentProviderEndIntent, PersistentProviderSessionIntent, PersistentRuntimeOperation,
    ProviderCode, ProviderRuntimeDiagnostic, ProviderRuntimeEvent, SharedCoreRuntime,
};
use pedelec_runtime::{
    AntigravityReasoningEffort, AntigravityRuntimeError, AntigravityRuntimeEvent,
    AntigravityRuntimeLaunchConfig, AntigravityStreamController, ProviderRuntimeController,
    ProviderRuntimeOwner, RuntimeRegistryError,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const ANTIGRAVITY_RUNTIME_KEY_PREFIX: &str = "antigravity-stream";

type PumpKey = (String, u64);

#[derive(Debug, Clone)]
pub struct AntigravityRuntimeDispatcher {
    owner: ProviderRuntimeOwner,
    core_runtime: SharedCoreRuntime,
    program_override: Option<PathBuf>,
    env_overrides: Vec<(OsString, OsString)>,
    controllers: Arc<Mutex<HashMap<String, Arc<AntigravityStreamController>>>>,
    event_pumps: Arc<Mutex<HashSet<PumpKey>>>,
    traffic_pumps: Arc<Mutex<HashSet<PumpKey>>>,
    stopped_generations: Arc<Mutex<HashSet<PumpKey>>>,
}

impl AntigravityRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self {
            owner,
            core_runtime,
            program_override: None,
            env_overrides: Vec::new(),
            controllers: Arc::new(Mutex::new(HashMap::new())),
            event_pumps: Arc::new(Mutex::new(HashSet::new())),
            traffic_pumps: Arc::new(Mutex::new(HashSet::new())),
            stopped_generations: Arc::new(Mutex::new(HashSet::new())),
        }
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

    fn runtime_key(thread_id: &str) -> String {
        format!("{ANTIGRAVITY_RUNTIME_KEY_PREFIX}:{thread_id}")
    }

    fn controller_for(
        &self,
        session: &PersistentProviderSessionIntent,
    ) -> Result<Arc<AntigravityStreamController>, PedelecError> {
        let thread_id = session.thread_id.clone();
        let runtime_key = Self::runtime_key(&thread_id);
        let program_override = self.program_override.clone();
        let env_overrides = self.env_overrides.clone();
        let core_runtime = Arc::clone(&self.core_runtime);
        let controllers = Arc::clone(&self.controllers);
        let launch_session = session.clone();
        self.owner
            .get_or_init(runtime_key, move || {
                let program = {
                    let core = core_runtime.lock().map_err(|_| {
                        RuntimeRegistryError::Initialization(
                            "Core runtime mutex was poisoned".to_string(),
                        )
                    })?;
                    let program = match program_override {
                        Some(program) => program,
                        None => core
                            .provider_executable_path(&ProviderCode::Antigravity)
                            .map_err(|error| {
                                RuntimeRegistryError::Initialization(format!(
                                    "{} ({})",
                                    error.message, error.code
                                ))
                            })?,
                    };
                    core.prepare_antigravity_persistent_workspace(&launch_session.workspace_path)
                        .map_err(|error| {
                            RuntimeRegistryError::Initialization(format!(
                                "{} ({})",
                                error.message, error.code
                            ))
                        })?;
                    program
                };
                let mut launch = antigravity_launch_config(program, &launch_session);
                for (key, value) in env_overrides {
                    launch = launch.with_env(key, value);
                }
                let controller = AntigravityStreamController::spawn(thread_id.clone(), launch)
                    .map_err(|error| RuntimeRegistryError::Initialization(error.to_string()))?;
                controllers
                    .lock()
                    .map_err(|_| {
                        RuntimeRegistryError::Initialization(
                            "Antigravity controller map mutex was poisoned".to_string(),
                        )
                    })?
                    .insert(thread_id.clone(), Arc::clone(&controller));
                Ok(controller as Arc<dyn ProviderRuntimeController>)
            })
            .map_err(|error| registry_error(error, &session.thread_id))?;

        self.controllers
            .lock()
            .map_err(|_| mutex_error("Antigravity controller map"))?
            .get(&session.thread_id)
            .cloned()
            .filter(|controller| controller.is_healthy())
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "Antigravity persistent stream runtime is not healthy",
                    json!({
                        "provider": "antigravity",
                        "threadId": session.thread_id,
                        "operation": "admission",
                    }),
                )
            })
    }

    fn current_controller(&self, thread_id: &str) -> Option<Arc<AntigravityStreamController>> {
        self.controllers
            .lock()
            .ok()
            .and_then(|controllers| controllers.get(thread_id).cloned())
            .filter(|controller| controller.is_healthy())
    }

    fn start_traffic_pump(&self, controller: Arc<AntigravityStreamController>, thread_id: String) {
        let key = (thread_id.clone(), controller.generation());
        if !self
            .traffic_pumps
            .lock()
            .map(|mut pumps| pumps.insert(key.clone()))
            .unwrap_or(false)
        {
            return;
        }
        let runtime = Arc::clone(&self.core_runtime);
        let controllers = Arc::clone(&self.controllers);
        let pumps = Arc::clone(&self.traffic_pumps);
        let generation = controller.generation();
        let process_id = controller.process_id();
        thread::spawn(move || {
            loop {
                match controller.recv_protocol_traffic_timeout(Duration::from_millis(50)) {
                    Ok(record) => {
                        if !is_current_generation(&controllers, &thread_id, generation) {
                            break;
                        }
                        record_protocol_traffic(
                            &runtime,
                            ProviderCode::Antigravity,
                            generation,
                            process_id,
                            record,
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !controller.is_healthy()
                            || !is_current_generation(&controllers, &thread_id, generation)
                        {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            if let Ok(mut pumps) = pumps.lock() {
                pumps.remove(&(thread_id, generation));
            }
        });
    }

    fn start_event_pump(&self, controller: Arc<AntigravityStreamController>, thread_id: String) {
        self.start_traffic_pump(Arc::clone(&controller), thread_id.clone());
        let key = (thread_id.clone(), controller.generation());
        if !self
            .event_pumps
            .lock()
            .map(|mut pumps| pumps.insert(key.clone()))
            .unwrap_or(false)
        {
            return;
        }
        record_diagnostic(
            &self.core_runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                provider: ProviderCode::Antigravity,
                runtime_generation: controller.generation(),
                process_id: controller.process_id(),
            },
        );
        let runtime = Arc::clone(&self.core_runtime);
        let controllers = Arc::clone(&self.controllers);
        let pumps = Arc::clone(&self.event_pumps);
        let stopped = Arc::clone(&self.stopped_generations);
        let generation = controller.generation();
        thread::spawn(move || {
            while let Ok(event) = controller.recv_event() {
                if !is_current_generation(&controllers, &thread_id, generation) {
                    break;
                }
                let terminal = matches!(
                    event,
                    AntigravityRuntimeEvent::ProtocolError { .. }
                        | AntigravityRuntimeEvent::Disconnected { .. }
                );
                handle_runtime_event(&runtime, &controller, &thread_id, event);
                if terminal {
                    break;
                }
            }
            if let Ok(mut pumps) = pumps.lock() {
                pumps.remove(&(thread_id.clone(), generation));
            }
            if !controller.is_healthy() {
                mark_stopped(
                    &runtime,
                    &stopped,
                    &thread_id,
                    &controller,
                    "runtime event pump ended",
                );
            }
        });
    }

    fn dispatch_end_session(
        &self,
        session: &PersistentProviderEndIntent,
    ) -> Result<(), PedelecError> {
        let controller = self.current_controller(&session.thread_id);
        let Some(controller) = controller else {
            return if session.active_provider_turn_id.is_some() {
                Err(PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "active Antigravity turn has no healthy persistent stream runtime",
                    json!({
                        "provider": "antigravity",
                        "operation": "end",
                        "threadId": session.thread_id,
                        "providerSessionId": session.provider_session_id,
                        "providerTurnId": session.active_provider_turn_id,
                    }),
                ))
            } else {
                Ok(())
            };
        };
        let generation = controller.generation();
        let process_id = controller.process_id();
        let result = controller.shutdown_runtime().map_err(|error| {
            controller_error(
                error,
                Some(&controller),
                &session.thread_id,
                session.active_provider_turn_id.as_deref(),
                "end",
            )
        });
        if let Ok(mut controllers) = self.controllers.lock() {
            if controllers
                .get(&session.thread_id)
                .is_some_and(|current| current.generation() == generation)
            {
                controllers.remove(&session.thread_id);
            }
        }
        mark_stopped_raw(
            &self.core_runtime,
            &self.stopped_generations,
            &session.thread_id,
            generation,
            process_id,
            "session ended",
        );
        result
    }

    pub fn record_shutdown_diagnostics(&self) {
        let controllers = self
            .controllers
            .lock()
            .map(|controllers| controllers.clone())
            .unwrap_or_default();
        for (thread_id, controller) in controllers {
            mark_stopped(
                &self.core_runtime,
                &self.stopped_generations,
                &thread_id,
                &controller,
                "desktop shutdown",
            );
        }
    }
}

impl PersistentRuntimeDispatcher for AntigravityRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        if operation.provider() != &ProviderCode::Antigravity {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_UNSUPPORTED,
                "Antigravity runtime received an operation for another provider",
                json!({"operationProvider": operation.provider()}),
            ));
        }
        if let PersistentRuntimeOperation::EndSession { session } = &operation {
            return self.dispatch_end_session(session);
        }

        let session = match &operation {
            PersistentRuntimeOperation::EnsureSession { session } => session,
            PersistentRuntimeOperation::StartTurn { turn } => &turn.session,
            PersistentRuntimeOperation::EndSession { .. } => unreachable!(),
        };
        let controller = self.controller_for(session)?;
        self.start_event_pump(Arc::clone(&controller), session.thread_id.clone());

        match operation {
            PersistentRuntimeOperation::EnsureSession { session } => {
                let use_bootstrap = controller.needs_bootstrap();
                let prompt = build_persistent_prepare_prompt(
                    use_bootstrap.then_some(session.host_instructions.as_str()),
                );
                controller.start_prepare(&prompt).map_err(|error| {
                    controller_error(
                        error,
                        Some(&controller),
                        &session.thread_id,
                        None,
                        "prepare",
                    )
                })?;
                if use_bootstrap {
                    controller.mark_bootstrap_sent();
                }
                Ok(())
            }
            PersistentRuntimeOperation::StartTurn { turn } => {
                let use_bootstrap = controller.needs_bootstrap();
                let prompt = if use_bootstrap {
                    build_persistent_user_prompt_with_bootstrap(
                        &turn.session.host_instructions,
                        &turn.message,
                    )
                } else {
                    turn.message.clone()
                };
                controller
                    .start_turn(&turn.local_turn_id, &prompt)
                    .map_err(|error| {
                        controller_error(
                            error,
                            Some(&controller),
                            &turn.thread_id,
                            Some(&turn.local_turn_id),
                            "turn",
                        )
                    })?;
                if use_bootstrap {
                    controller.mark_bootstrap_sent();
                }
                Ok(())
            }
            PersistentRuntimeOperation::EndSession { .. } => unreachable!(),
        }
    }
}

fn antigravity_launch_config(
    program: PathBuf,
    session: &PersistentProviderSessionIntent,
) -> AntigravityRuntimeLaunchConfig {
    let mut launch = AntigravityRuntimeLaunchConfig::new(program, &session.workspace_path)
        .with_model(session.model.clone())
        .with_effort(session.antigravity_reasoning_effort.map(map_effort))
        .with_conversation_id(session.provider_session_id.clone())
        .with_env("PEDELEC_PROVIDER", "antigravity")
        .with_env(
            "PEDELEC_CORE_IPC_RUNTIME_FILE",
            session
                .core_ipc_runtime_file_path
                .to_string_lossy()
                .into_owned(),
        );
    if let Some(path) = std::env::var_os("PATH") {
        launch = launch.with_env("PATH", path);
    }
    launch
}

fn map_effort(effort: CoreAntigravityReasoningEffort) -> AntigravityReasoningEffort {
    match effort {
        CoreAntigravityReasoningEffort::Low => AntigravityReasoningEffort::Low,
        CoreAntigravityReasoningEffort::Medium => AntigravityReasoningEffort::Medium,
        CoreAntigravityReasoningEffort::High => AntigravityReasoningEffort::High,
    }
}

fn is_current_generation(
    controllers: &Mutex<HashMap<String, Arc<AntigravityStreamController>>>,
    thread_id: &str,
    generation: u64,
) -> bool {
    controllers
        .lock()
        .map(|controllers| {
            controllers
                .get(thread_id)
                .is_some_and(|controller| controller.generation() == generation)
        })
        .unwrap_or(false)
}

fn handle_runtime_event(
    runtime: &SharedCoreRuntime,
    controller: &AntigravityStreamController,
    thread_id: &str,
    event: AntigravityRuntimeEvent,
) {
    match event {
        AntigravityRuntimeEvent::SessionReady { conversation_id } => {
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                    thread_id: thread_id.to_string(),
                    provider_session_id: conversation_id,
                });
            }
        }
        AntigravityRuntimeEvent::TurnStarted { local_turn_id } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                    provider: ProviderCode::Antigravity,
                    runtime_generation: controller.generation(),
                    process_id: controller.process_id(),
                    thread_id: thread_id.to_string(),
                    provider_thread_id: controller.conversation_id().unwrap_or_default(),
                    provider_turn_id: local_turn_id.clone(),
                },
            );
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::TurnStarted {
                    thread_id: thread_id.to_string(),
                    provider_turn_id: local_turn_id,
                });
            }
        }
        AntigravityRuntimeEvent::AssistantDelta {
            local_turn_id,
            text,
        } => {
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::AssistantDelta {
                    thread_id: thread_id.to_string(),
                    provider_turn_id: Some(local_turn_id),
                    text,
                });
            }
        }
        AntigravityRuntimeEvent::AssistantMessage {
            local_turn_id,
            text,
        } => {
            if let Ok(mut core) = runtime.lock() {
                let _ =
                    core.reduce_provider_runtime_event(ProviderRuntimeEvent::AssistantMessage {
                        thread_id: thread_id.to_string(),
                        provider_turn_id: Some(local_turn_id),
                        text,
                    });
            }
        }
        AntigravityRuntimeEvent::UsageUpdated {
            local_turn_id,
            usage,
        } => {
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::UsageUpdated {
                    thread_id: thread_id.to_string(),
                    provider_turn_id: Some(local_turn_id),
                    usage,
                });
            }
        }
        AntigravityRuntimeEvent::TurnCompleted {
            local_turn_id,
            status,
            success,
            error,
            response,
            conversation_id,
        } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                    provider: ProviderCode::Antigravity,
                    runtime_generation: controller.generation(),
                    process_id: controller.process_id(),
                    thread_id: thread_id.to_string(),
                    provider_thread_id: conversation_id.clone().unwrap_or_default(),
                    provider_turn_id: local_turn_id.clone(),
                    status: status.clone(),
                },
            );
            let mapped_error = (!success).then(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_REQUEST_FAILED,
                    "Antigravity returned an unsuccessful result",
                    json!({
                        "provider": "antigravity",
                        "operation": "turn",
                        "stage": "completed",
                        "threadId": thread_id,
                        "providerSessionId": conversation_id,
                        "providerTurnId": local_turn_id,
                        "runtimeGeneration": controller.generation(),
                        "processId": controller.process_id(),
                        "status": status,
                        "providerError": error,
                        "response": response,
                    }),
                )
            });
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                    thread_id: thread_id.to_string(),
                    provider_turn_id: local_turn_id,
                    success,
                    error: mapped_error,
                });
            }
        }
        AntigravityRuntimeEvent::Stderr { text } => record_diagnostic(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                provider: ProviderCode::Antigravity,
                runtime_generation: controller.generation(),
                process_id: controller.process_id(),
                text,
            },
        ),
        AntigravityRuntimeEvent::ProtocolError {
            local_turn_id,
            message,
        } => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROTOCOL_ERROR,
                message.clone(),
                json!({
                    "provider": "antigravity",
                    "operation": "stream",
                    "threadId": thread_id,
                    "providerTurnId": local_turn_id,
                    "runtimeGeneration": controller.generation(),
                    "processId": controller.process_id(),
                }),
            );
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeError {
                    provider: ProviderCode::Antigravity,
                    runtime_generation: Some(controller.generation()),
                    process_id: Some(controller.process_id()),
                    thread_id: Some(thread_id.to_string()),
                    provider_thread_id: controller.conversation_id(),
                    provider_turn_id: local_turn_id,
                    code: error_codes::PROVIDER_PROTOCOL_ERROR.to_string(),
                    message,
                    details: error.details.clone(),
                },
            );
            reduce_disconnect(runtime, thread_id, error);
        }
        AntigravityRuntimeEvent::Disconnected {
            generation,
            pid,
            reason,
        } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                    provider: ProviderCode::Antigravity,
                    runtime_generation: generation,
                    process_id: pid,
                    thread_id: Some(thread_id.to_string()),
                    provider_thread_id: controller.conversation_id(),
                    reason: reason.clone(),
                },
            );
            reduce_disconnect(
                runtime,
                thread_id,
                PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "Antigravity persistent stream runtime disconnected",
                    json!({
                        "provider": "antigravity",
                        "operation": "runtime",
                        "threadId": thread_id,
                        "runtimeGeneration": generation,
                        "processId": pid,
                        "reason": reason,
                    }),
                ),
            );
        }
    }
}

fn reduce_disconnect(runtime: &SharedCoreRuntime, thread_id: &str, error: PedelecError) {
    if let Ok(mut core) = runtime.lock() {
        let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::RuntimeDisconnected {
            thread_id: Some(thread_id.to_string()),
            error,
        });
    }
}

fn registry_error(error: RuntimeRegistryError, thread_id: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_START_FAILED,
        "Antigravity persistent stream runtime could not be started",
        json!({
            "provider": "antigravity",
            "operation": "startup",
            "threadId": thread_id,
            "error": error.to_string(),
        }),
    )
}

fn controller_error(
    error: AntigravityRuntimeError,
    controller: Option<&AntigravityStreamController>,
    thread_id: &str,
    local_turn_id: Option<&str>,
    operation: &str,
) -> PedelecError {
    let code = match error {
        AntigravityRuntimeError::RuntimeStart(_) | AntigravityRuntimeError::Write(_) => {
            error_codes::PROVIDER_RUNTIME_DISCONNECTED
        }
        AntigravityRuntimeError::Busy | AntigravityRuntimeError::ShuttingDown => {
            error_codes::PROVIDER_PROTOCOL_ERROR
        }
    };
    let mut details = json!({
        "provider": "antigravity",
        "operation": operation,
        "threadId": thread_id,
        "providerTurnId": local_turn_id,
        "error": error.to_string(),
    });
    if let Some(controller) = controller {
        details["runtimeGeneration"] = json!(controller.generation());
        details["processId"] = json!(controller.process_id());
        details["providerSessionId"] = json!(controller.conversation_id());
    }
    PedelecError::with_details(code, error.to_string(), details)
}

fn record_diagnostic(runtime: &SharedCoreRuntime, diagnostic: ProviderRuntimeDiagnostic) {
    if let Ok(mut core) = runtime.lock() {
        core.record_provider_runtime_diagnostic(diagnostic);
    }
}

fn mark_stopped(
    runtime: &SharedCoreRuntime,
    stopped: &Mutex<HashSet<PumpKey>>,
    thread_id: &str,
    controller: &AntigravityStreamController,
    reason: &str,
) {
    mark_stopped_raw(
        runtime,
        stopped,
        thread_id,
        controller.generation(),
        controller.process_id(),
        reason,
    );
}

fn mark_stopped_raw(
    runtime: &SharedCoreRuntime,
    stopped: &Mutex<HashSet<PumpKey>>,
    thread_id: &str,
    generation: u64,
    process_id: u32,
    reason: &str,
) {
    let key = (thread_id.to_string(), generation);
    let should_emit = stopped
        .lock()
        .map(|mut stopped| stopped.insert(key))
        .unwrap_or(true);
    if should_emit {
        record_diagnostic(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStopped {
                provider: ProviderCode::Antigravity,
                runtime_generation: generation,
                process_id,
                reason: reason.to_string(),
            },
        );
    }
}

fn mutex_error(name: &str) -> PedelecError {
    PedelecError::new(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        format!("{name} mutex was poisoned"),
    )
}

#[allow(dead_code)]
fn _workspace_for_diagnostic(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_core::{
        CreateThreadInput, CreateThreadSkillsInput, CreateThreadWorkspaceInput, EffortLevel,
        EndThreadInput, PersistentRuntimeOperation, PrepareThreadInput,
        ProviderExecutionOperationKind, SendTextInput, ThreadEvent, ThreadStatus, WorkspaceManager,
    };
    use std::fs;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn prepare_materializes_agent_before_spawn_and_reuses_one_process() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "integration guidance");
        {
            let mut core = runtime.lock().unwrap();
            core.thread_manager
                .thread_mut(&thread_id)
                .unwrap()
                .effort_args = vec![
                "--model".into(),
                "agy-test-model".into(),
                "--effort".into(),
                "high".into(),
            ];
        }
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let traffic_rx = runtime
            .lock()
            .unwrap()
            .subscribe_provider_protocol_traffic();
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        )
        .with_env_for_test("FAKE_AGY_REQUIRE_AGENT_FILE", "1");

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let controller = dispatcher
            .current_controller(&thread_id)
            .expect("prepare should leave one healthy controller");
        let process_id = controller.process_id();

        let agent_path = workspace.join(".agents/agents/pedelec-runtime/agent.md");
        let agent = fs::read_to_string(agent_path).unwrap();
        assert!(agent.contains("name: pedelec-runtime"));
        assert!(agent.contains("Pedelec is the host application"));
        let start = fs::read_to_string(&start_log).unwrap();
        assert!(start.contains("agent=true"));
        assert!(start.contains("--input-format stream-json"));
        assert!(start.contains("--output-format stream-json"));
        assert!(start.contains("--agent pedelec-runtime"));
        assert!(start.contains("--model agy-test-model"));
        assert!(start.contains("--effort high"));
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("agy-conversation-fresh")
        );

        dispatch_turn(&dispatcher, &runtime, &thread_id, "first actual task");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &thread_id, "second actual task");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            dispatcher
                .current_controller(&thread_id)
                .unwrap()
                .process_id(),
            process_id
        );
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 3);
        assert!(frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Session Preparation]"));
        assert_eq!(frames[1]["message"]["content"], "first actual task");
        assert_eq!(frames[2]["message"]["content"], "second actual task");

        let traffic = collect_protocol_traffic(traffic_rx, 10);
        assert!(traffic.iter().any(|record| {
            record.provider == ProviderCode::Antigravity
                && record.thread_id.as_deref() == Some(thread_id.as_str())
                && record.process_id == process_id
                && record.direction == "client_to_provider"
                && record.message["event"] == "user"
        }));
        for event in ["init", "step_update", "result"] {
            assert!(traffic.iter().any(|record| {
                record.provider == ProviderCode::Antigravity
                    && record.thread_id.as_deref() == Some(thread_id.as_str())
                    && record.process_id == process_id
                    && record.direction == "provider_to_client"
                    && record.message["event"] == event
            }));
        }
        let log_root = workspace.join(".pedelec-runtime/logs");
        let protocol_logs = fs::read_dir(log_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("protocol-antigravity-{thread_id}-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(protocol_logs.len(), 1);
        let durable = fs::read_to_string(protocol_logs[0].path()).unwrap();
        assert!(durable.contains("\"direction\":\"client_to_provider\""));
        assert!(durable.contains("\"direction\":\"provider_to_client\""));
        let _ = owner.shutdown();
    }

    #[test]
    fn direct_first_turn_bootstraps_once_and_normalizes_cjk_output() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "direct guidance");
        let event_rx = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );

        dispatch_turn(&dispatcher, &runtime, &thread_id, "直接開始");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let process_id = dispatcher
            .current_controller(&thread_id)
            .unwrap()
            .process_id();
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("agy-conversation-fresh")
        );
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { text, .. } if text == "哈囉-1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantMessage { text, .. } if text == "final-1"
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::OperationCompleted { success: true, .. })));

        dispatch_turn(&dispatcher, &runtime, &thread_id, "第二輪");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            dispatcher
                .current_controller(&thread_id)
                .unwrap()
                .process_id(),
            process_id
        );
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 2);
        let first = frames[0]["message"]["content"].as_str().unwrap();
        assert!(first.contains("[Pedelec Host Bootstrap]"));
        assert!(first.contains("直接開始"));
        assert_eq!(frames[1]["message"]["content"], "第二輪");
        let _ = owner.shutdown();
    }

    #[test]
    fn failed_prepare_retires_generation_and_next_turn_restarts_with_bootstrap() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "FAIL_PREPARE");
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(provider_session_id(&runtime, &thread_id), None);
        wait_until(Duration::from_secs(3), || {
            dispatcher.current_controller(&thread_id).is_none()
        });

        dispatch_turn(
            &dispatcher,
            &runtime,
            &thread_id,
            "recover after failed prepare",
        );
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("agy-conversation-fresh")
        );
        let starts = fs::read_to_string(&start_log).unwrap();
        assert_eq!(starts.lines().count(), 2);
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 2);
        assert!(frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Session Preparation]"));
        let recovery = frames[1]["message"]["content"].as_str().unwrap();
        assert!(recovery.contains("[Pedelec Host Bootstrap]"));
        assert!(recovery.contains("recover after failed prepare"));
        let _ = owner.shutdown();
    }

    #[test]
    fn idle_crash_replaces_generation_and_resumes_persisted_conversation() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "resume guidance");
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );

        dispatch_turn(
            &dispatcher,
            &runtime,
            &thread_id,
            "finish then EXIT_AFTER_RESULT",
        );
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("agy-conversation-fresh")
        );
        let first_pid = start_log_pids(&start_log)[0];
        wait_until(Duration::from_secs(3), || {
            dispatcher.current_controller(&thread_id).is_none()
        });
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Idle)
        );

        dispatch_turn(&dispatcher, &runtime, &thread_id, "resumed raw task");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let second_pid = dispatcher
            .current_controller(&thread_id)
            .unwrap()
            .process_id();
        assert_ne!(first_pid, second_pid);
        let starts = fs::read_to_string(&start_log).unwrap();
        let lines = starts.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("--conversation agy-conversation-fresh"));
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["message"]["content"], "resumed raw task");
        let _ = owner.shutdown();
    }

    #[test]
    fn active_crash_is_thread_scoped_and_end_does_not_stop_other_runtime() {
        let temp = tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let failed = create_thread(
            &runtime,
            &temp.path().join("failed-workspace"),
            "failed guidance",
        );
        let healthy = create_thread(
            &runtime,
            &temp.path().join("healthy-workspace"),
            "healthy guidance",
        );
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );

        dispatch_turn(&dispatcher, &runtime, &healthy, "healthy task");
        wait_for_status(&runtime, &healthy, ThreadStatus::Idle);
        let healthy_pid = dispatcher
            .current_controller(&healthy)
            .unwrap()
            .process_id();
        dispatch_turn(&dispatcher, &runtime, &failed, "CRASH_ACTIVE during work");
        wait_for_status(&runtime, &failed, ThreadStatus::Error);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&healthy),
            Some(ThreadStatus::Idle)
        );
        assert!(dispatcher.current_controller(&failed).is_none());
        assert_eq!(
            dispatcher
                .current_controller(&healthy)
                .unwrap()
                .process_id(),
            healthy_pid
        );

        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: failed.clone(),
            })
            .unwrap();
        let operation @ PersistentRuntimeOperation::EndSession { .. } = end.execution else {
            panic!("Antigravity end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime.lock().unwrap().finish_end_thread(&failed).unwrap();
        assert_eq!(
            runtime.lock().unwrap().thread_status(&failed),
            Some(ThreadStatus::Ended)
        );

        dispatch_turn(&dispatcher, &runtime, &healthy, "healthy continues");
        wait_for_status(&runtime, &healthy, ThreadStatus::Idle);
        assert_eq!(
            dispatcher
                .current_controller(&healthy)
                .unwrap()
                .process_id(),
            healthy_pid
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn custom_agent_materialization_failure_prevents_process_spawn() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "materialization failure");
        let agents_path = workspace.join(".agents");
        if agents_path.is_dir() {
            fs::remove_dir_all(&agents_path).unwrap();
        }
        fs::write(&agents_path, "block directory creation").unwrap();
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );
        let start = runtime
            .lock()
            .unwrap()
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.clone(),
                message: "must not spawn".into(),
                operation_id: None,
            })
            .unwrap();
        let operation = start.intent;
        let error = dispatcher.dispatch(operation).unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_RUNTIME_START_FAILED);
        runtime.lock().unwrap().fail_provider_execution_dispatch(
            &thread_id,
            ProviderExecutionOperationKind::UserTurn,
            error,
        );
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Error)
        );
        assert!(!start_log.exists());
        assert!(!stdin_log.exists());
        assert!(dispatcher.current_controller(&thread_id).is_none());
        let _ = owner.shutdown();
    }

    #[test]
    fn ending_live_thread_keeps_other_antigravity_runtime_alive() {
        let temp = tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let first = create_thread(
            &runtime,
            &temp.path().join("workspace-first"),
            "first guidance",
        );
        let second = create_thread(
            &runtime,
            &temp.path().join("workspace-second"),
            "second guidance",
        );
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = test_dispatcher(
            owner.clone(),
            Arc::clone(&runtime),
            temp.path(),
            &stdin_log,
            &start_log,
        );
        dispatch_turn(&dispatcher, &runtime, &first, "first task");
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &second, "second task");
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
        let second_pid = dispatcher.current_controller(&second).unwrap().process_id();

        let end = runtime
            .lock()
            .unwrap()
            .begin_end_thread(EndThreadInput {
                thread_id: first.clone(),
            })
            .unwrap();
        let operation @ PersistentRuntimeOperation::EndSession { .. } = end.execution else {
            panic!("Antigravity end should use persistent runtime");
        };
        dispatcher.dispatch(operation).unwrap();
        runtime.lock().unwrap().finish_end_thread(&first).unwrap();
        assert_eq!(
            runtime.lock().unwrap().thread_status(&first),
            Some(ThreadStatus::Ended)
        );
        assert!(dispatcher.current_controller(&first).is_none());
        assert_eq!(
            dispatcher.current_controller(&second).unwrap().process_id(),
            second_pid
        );
        dispatch_turn(&dispatcher, &runtime, &second, "second still alive");
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
        assert_eq!(
            dispatcher.current_controller(&second).unwrap().process_id(),
            second_pid
        );
        let _ = owner.shutdown();
    }

    fn test_runtime(temp: &Path) -> SharedCoreRuntime {
        let mut core = pedelec_core::CoreRuntime::new_for_application();
        core.workspace_manager = WorkspaceManager::with_workspace_root(temp.join("managed"));
        core.settings_file_path = Some(temp.join("settings.json"));
        core.set_core_ipc_runtime("127.0.0.1:1", temp.join("runtime.json"));
        Arc::new(Mutex::new(core))
    }

    fn create_thread(runtime: &SharedCoreRuntime, workspace: &Path, guidance: &str) -> String {
        fs::create_dir_all(workspace).unwrap();
        runtime
            .lock()
            .unwrap()
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Antigravity,
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
        start_log: &Path,
    ) -> AntigravityRuntimeDispatcher {
        AntigravityRuntimeDispatcher::new(owner, runtime)
            .with_program_for_test(fake_program(temp))
            .with_env_for_test("FAKE_AGY_LOG", stdin_log.as_os_str())
            .with_env_for_test("FAKE_AGY_START_LOG", start_log.as_os_str())
    }

    fn dispatch_prepare(
        dispatcher: &AntigravityRuntimeDispatcher,
        runtime: &SharedCoreRuntime,
        thread_id: &str,
    ) {
        let start = runtime
            .lock()
            .unwrap()
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.to_string(),
                operation_id: None,
            })
            .unwrap();
        let Some(operation) = start.intent else {
            panic!("Antigravity prepare should produce a persistent operation");
        };
        dispatcher.dispatch(operation).unwrap();
    }

    fn dispatch_turn(
        dispatcher: &AntigravityRuntimeDispatcher,
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
                operation_id: None,
            })
            .unwrap();
        let operation = start.intent;
        dispatcher.dispatch(operation).unwrap();
    }

    fn wait_for_status(runtime: &SharedCoreRuntime, thread_id: &str, expected: ThreadStatus) {
        wait_until(Duration::from_secs(5), || {
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

    fn stdin_frames(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn start_log_pids(path: &Path) -> Vec<u32> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("pid="))
                    .and_then(|pid| pid.parse().ok())
            })
            .collect()
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

    fn fake_program(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let target = directory.join("fake-antigravity.cmd");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_antigravity_stream.ps1");
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
            let target = directory.join("fake-antigravity");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_antigravity_stream.sh");
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
