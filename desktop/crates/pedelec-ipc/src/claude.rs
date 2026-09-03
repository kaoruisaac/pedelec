use crate::{record_protocol_traffic, PersistentRuntimeDispatcher};
use pedelec_core::{
    build_persistent_prepare_prompt, error_codes,
    ClaudeReasoningEffort as CoreClaudeReasoningEffort, PedelecError, PersistentProviderEndIntent,
    PersistentProviderSessionIntent, PersistentRuntimeOperation, ProviderCode,
    ProviderRuntimeDiagnostic, ProviderRuntimeEvent, SharedCoreRuntime,
};
use pedelec_runtime::{
    ClaudeReasoningEffort, ClaudeRuntimeError, ClaudeRuntimeEvent, ClaudeRuntimeLaunchConfig,
    ClaudeStreamController, ProviderRuntimeController, ProviderRuntimeOwner, RuntimeRegistryError,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CLAUDE_RUNTIME_KEY_PREFIX: &str = "claude-stream";

type PumpKey = (String, u64);

#[derive(Debug, Clone)]
pub struct ClaudeRuntimeDispatcher {
    owner: ProviderRuntimeOwner,
    core_runtime: SharedCoreRuntime,
    program_override: Option<PathBuf>,
    env_overrides: Vec<(OsString, OsString)>,
    controllers: Arc<Mutex<HashMap<String, Arc<ClaudeStreamController>>>>,
    event_pumps: Arc<Mutex<HashSet<PumpKey>>>,
    traffic_pumps: Arc<Mutex<HashSet<PumpKey>>>,
    stopped_generations: Arc<Mutex<HashSet<PumpKey>>>,
}

impl ClaudeRuntimeDispatcher {
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
        format!("{CLAUDE_RUNTIME_KEY_PREFIX}:{thread_id}")
    }

    fn controller_for(
        &self,
        session: &PersistentProviderSessionIntent,
    ) -> Result<Arc<ClaudeStreamController>, PedelecError> {
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
                    match program_override {
                        Some(program) => program,
                        None => core
                            .provider_executable_path(&ProviderCode::Claude)
                            .map_err(|error| {
                                RuntimeRegistryError::Initialization(format!(
                                    "{} ({})",
                                    error.message, error.code
                                ))
                            })?,
                    }
                };
                let mut launch = claude_launch_config(program, &launch_session);
                for (key, value) in env_overrides {
                    launch = launch.with_env(key, value);
                }
                let controller = ClaudeStreamController::spawn(thread_id.clone(), launch)
                    .map_err(|error| RuntimeRegistryError::Initialization(error.to_string()))?;
                controllers
                    .lock()
                    .map_err(|_| {
                        RuntimeRegistryError::Initialization(
                            "Claude controller map mutex was poisoned".to_string(),
                        )
                    })?
                    .insert(thread_id.clone(), Arc::clone(&controller));
                Ok(controller as Arc<dyn ProviderRuntimeController>)
            })
            .map_err(|error| registry_error(error, &session.thread_id))?;

        self.controllers
            .lock()
            .map_err(|_| mutex_error("Claude controller map"))?
            .get(&session.thread_id)
            .cloned()
            .filter(|controller| controller.is_healthy())
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "Claude persistent stream runtime is not healthy",
                    json!({
                        "provider": "claude",
                        "threadId": session.thread_id,
                        "operation": "admission",
                    }),
                )
            })
    }

    fn current_controller(&self, thread_id: &str) -> Option<Arc<ClaudeStreamController>> {
        self.controllers
            .lock()
            .ok()
            .and_then(|controllers| controllers.get(thread_id).cloned())
            .filter(|controller| controller.is_healthy())
    }

    fn start_traffic_pump(&self, controller: Arc<ClaudeStreamController>, thread_id: String) {
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
                            ProviderCode::Claude,
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

    fn start_event_pump(&self, controller: Arc<ClaudeStreamController>, thread_id: String) {
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
                provider: ProviderCode::Claude,
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
                    ClaudeRuntimeEvent::ProtocolError { .. }
                        | ClaudeRuntimeEvent::Disconnected { .. }
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
                    "active Claude turn has no healthy persistent stream runtime",
                    json!({
                        "provider": "claude",
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

impl PersistentRuntimeDispatcher for ClaudeRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        if operation.provider() != &ProviderCode::Claude {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_UNSUPPORTED,
                "Claude runtime received an operation for another provider",
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
                let prompt = build_persistent_prepare_prompt(None);
                controller.start_prepare(&prompt).map_err(|error| {
                    controller_error(
                        error,
                        Some(&controller),
                        &session.thread_id,
                        None,
                        "prepare",
                    )
                })
            }
            PersistentRuntimeOperation::StartTurn { turn } => controller
                .start_turn(&turn.local_turn_id, &turn.message)
                .map_err(|error| {
                    controller_error(
                        error,
                        Some(&controller),
                        &turn.thread_id,
                        Some(&turn.local_turn_id),
                        "turn",
                    )
                }),
            PersistentRuntimeOperation::EndSession { .. } => unreachable!(),
        }
    }
}

fn claude_launch_config(
    program: PathBuf,
    session: &PersistentProviderSessionIntent,
) -> ClaudeRuntimeLaunchConfig {
    let mut launch = ClaudeRuntimeLaunchConfig::new(program, &session.workspace_path)
        .with_model(session.model.clone())
        .with_effort(session.claude_reasoning_effort.map(map_effort))
        .with_resume_session_id(session.provider_session_id.clone())
        .with_host_instructions(session.host_instructions.clone())
        .with_env("PEDELEC_PROVIDER", "claude")
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

fn map_effort(effort: CoreClaudeReasoningEffort) -> ClaudeReasoningEffort {
    match effort {
        CoreClaudeReasoningEffort::Low => ClaudeReasoningEffort::Low,
        CoreClaudeReasoningEffort::Medium => ClaudeReasoningEffort::Medium,
        CoreClaudeReasoningEffort::High => ClaudeReasoningEffort::High,
        CoreClaudeReasoningEffort::XHigh => ClaudeReasoningEffort::XHigh,
        CoreClaudeReasoningEffort::Max => ClaudeReasoningEffort::Max,
    }
}

fn is_current_generation(
    controllers: &Mutex<HashMap<String, Arc<ClaudeStreamController>>>,
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
    controller: &ClaudeStreamController,
    thread_id: &str,
    event: ClaudeRuntimeEvent,
) {
    match event {
        ClaudeRuntimeEvent::SessionReady { session_id } => {
            if let Ok(mut core) = runtime.lock() {
                let _ = core.reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                    thread_id: thread_id.to_string(),
                    provider_session_id: session_id,
                });
            }
        }
        ClaudeRuntimeEvent::TurnStarted { local_turn_id } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                    provider: ProviderCode::Claude,
                    runtime_generation: controller.generation(),
                    process_id: controller.process_id(),
                    thread_id: thread_id.to_string(),
                    provider_thread_id: controller.session_id().unwrap_or_default(),
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
        ClaudeRuntimeEvent::AssistantDelta {
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
        ClaudeRuntimeEvent::AssistantMessage {
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
        ClaudeRuntimeEvent::UsageUpdated {
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
        ClaudeRuntimeEvent::TurnCompleted {
            local_turn_id,
            status,
            success,
            error,
            session_id,
        } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                    provider: ProviderCode::Claude,
                    runtime_generation: controller.generation(),
                    process_id: controller.process_id(),
                    thread_id: thread_id.to_string(),
                    provider_thread_id: session_id.clone().unwrap_or_default(),
                    provider_turn_id: local_turn_id.clone(),
                    status: status.clone(),
                },
            );
            let mapped_error = (!success).then(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_REQUEST_FAILED,
                    "Claude returned an unsuccessful result",
                    json!({
                        "provider": "claude",
                        "operation": "turn",
                        "stage": "completed",
                        "threadId": thread_id,
                        "providerSessionId": session_id,
                        "providerTurnId": local_turn_id,
                        "runtimeGeneration": controller.generation(),
                        "processId": controller.process_id(),
                        "status": status,
                        "providerError": error,
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
        ClaudeRuntimeEvent::Stderr { text } => record_diagnostic(
            runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                provider: ProviderCode::Claude,
                runtime_generation: controller.generation(),
                process_id: controller.process_id(),
                text,
            },
        ),
        ClaudeRuntimeEvent::ProtocolError {
            local_turn_id,
            message,
        } => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROTOCOL_ERROR,
                message.clone(),
                json!({
                    "provider": "claude",
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
                    provider: ProviderCode::Claude,
                    runtime_generation: Some(controller.generation()),
                    process_id: Some(controller.process_id()),
                    thread_id: Some(thread_id.to_string()),
                    provider_thread_id: controller.session_id(),
                    provider_turn_id: local_turn_id,
                    code: error_codes::PROVIDER_PROTOCOL_ERROR.to_string(),
                    message,
                    details: error.details.clone(),
                },
            );
            reduce_disconnect(runtime, thread_id, error);
        }
        ClaudeRuntimeEvent::Disconnected {
            generation,
            pid,
            reason,
        } => {
            record_diagnostic(
                runtime,
                ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                    provider: ProviderCode::Claude,
                    runtime_generation: generation,
                    process_id: pid,
                    thread_id: Some(thread_id.to_string()),
                    provider_thread_id: controller.session_id(),
                    reason: reason.clone(),
                },
            );
            reduce_disconnect(
                runtime,
                thread_id,
                PedelecError::with_details(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "Claude persistent stream runtime disconnected",
                    json!({
                        "provider": "claude",
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
        "Claude persistent stream runtime could not be started",
        json!({
            "provider": "claude",
            "operation": "startup",
            "threadId": thread_id,
            "error": error.to_string(),
        }),
    )
}

fn controller_error(
    error: ClaudeRuntimeError,
    controller: Option<&ClaudeStreamController>,
    thread_id: &str,
    local_turn_id: Option<&str>,
    operation: &str,
) -> PedelecError {
    let code = match error {
        ClaudeRuntimeError::RuntimeStart(_) | ClaudeRuntimeError::Write(_) => {
            error_codes::PROVIDER_RUNTIME_DISCONNECTED
        }
        ClaudeRuntimeError::Busy | ClaudeRuntimeError::ShuttingDown => {
            error_codes::PROVIDER_PROTOCOL_ERROR
        }
    };
    let mut details = json!({
        "provider": "claude",
        "operation": operation,
        "threadId": thread_id,
        "providerTurnId": local_turn_id,
        "error": error.to_string(),
    });
    if let Some(controller) = controller {
        details["runtimeGeneration"] = json!(controller.generation());
        details["processId"] = json!(controller.process_id());
        details["providerSessionId"] = json!(controller.session_id());
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
    controller: &ClaudeStreamController,
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
                provider: ProviderCode::Claude,
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
        error_codes, CreateThreadInput, CreateThreadSkillsInput, CreateThreadWorkspaceInput,
        EffortLevel, EndThreadExecutionIntent, EndThreadInput, PrepareThreadInput,
        ProviderExecutionIntent, SendTextInput, ThreadEvent, ThreadStatus, WorkspaceManager,
    };
    use std::fs;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn prepare_uses_append_system_prompt_and_reuses_one_process() {
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
                "claude-opus-4-8".into(),
                "--effort".into(),
                "medium".into(),
            ];
        }
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let traffic_rx = runtime
            .lock()
            .unwrap()
            .subscribe_provider_protocol_traffic();
        let event_rx = runtime
            .lock()
            .unwrap()
            .subscribe_thread(pedelec_core::SubscribeThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
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
        let controller = dispatcher
            .current_controller(&thread_id)
            .expect("prepare should leave one healthy controller");
        let process_id = controller.process_id();
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("claude-session-fresh")
        );
        let prepare_events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(!prepare_events.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { .. } | ThreadEvent::AssistantMessage { .. }
        )));

        let start = fs::read_to_string(&start_log).unwrap();
        assert!(start.contains("-p"));
        assert!(start.contains("--input-format stream-json"));
        assert!(start.contains("--output-format stream-json"));
        assert!(start.contains("--include-partial-messages"));
        assert!(start.contains("--append-system-prompt"));
        assert!(!start.contains("--disable-slash-commands"));
        assert!(start.contains("--model claude-opus-4-8"));
        assert!(start.contains("--effort medium"));
        assert!(!start.contains("--resume"));

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
        assert!(!frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Pedelec Host Bootstrap]"));
        assert_eq!(frames[1]["message"]["content"], "first actual task");
        assert_eq!(frames[2]["message"]["content"], "second actual task");
        for frame in &frames {
            assert_eq!(frame["type"], "user");
            assert_eq!(frame["message"]["role"], "user");
        }

        let traffic = collect_protocol_traffic(traffic_rx, 10);
        assert!(traffic.iter().any(|record| {
            record.provider == ProviderCode::Claude
                && record.thread_id.as_deref() == Some(thread_id.as_str())
                && record.process_id == process_id
                && record.direction == "client_to_provider"
                && record.message["type"] == "user"
        }));
        for event in ["system", "stream_event", "assistant", "result"] {
            assert!(
                traffic.iter().any(|record| {
                    record.provider == ProviderCode::Claude
                        && record.thread_id.as_deref() == Some(thread_id.as_str())
                        && record.process_id == process_id
                        && record.direction == "provider_to_client"
                        && record.message["type"] == event
                }),
                "missing {event} traffic"
            );
        }
        let log_root = workspace.join(".pedelec-runtime/logs");
        let protocol_logs = fs::read_dir(log_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("protocol-claude-{thread_id}-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(protocol_logs.len(), 1);
        let durable = fs::read_to_string(protocol_logs[0].path()).unwrap();
        assert!(durable.contains("\"direction\":\"client_to_provider\""));
        assert!(durable.contains("\"direction\":\"provider_to_client\""));
        let _ = owner.shutdown();
    }

    #[test]
    fn production_dispatcher_routes_claude_persistent_operations() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "production routing");
        let stdin_log = temp.path().join("stdin.jsonl");
        let start_log = temp.path().join("start.log");
        let owner = ProviderRuntimeOwner::new();
        let dispatcher = crate::ProviderRuntimeDispatcher::new(owner.clone(), Arc::clone(&runtime))
            .with_claude_program_for_test(fake_program(temp.path()))
            .with_claude_env_for_test("FAKE_CLAUDE_LOG", stdin_log.as_os_str())
            .with_claude_env_for_test("FAKE_CLAUDE_START_LOG", start_log.as_os_str());

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("claude-session-fresh")
        );
        dispatch_turn(&dispatcher, &runtime, &thread_id, "routed user turn");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["message"]["content"], "routed user turn");
        let _ = dispatcher.shutdown();
    }

    #[test]
    fn user_turns_do_not_wrap_bootstrap_and_keep_cjk_output() {
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

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &thread_id, "直接開始");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        let process_id = dispatcher
            .current_controller(&thread_id)
            .unwrap()
            .process_id();
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { text, .. } if text == "哈囉-2"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantMessage { text, .. } if text == "final-2"
        )));

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
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1]["message"]["content"], "直接開始");
        assert_eq!(frames[2]["message"]["content"], "第二輪");
        assert!(!frames[1]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Pedelec Host Bootstrap]"));
        let _ = owner.shutdown();
    }

    #[test]
    fn unsuccessful_user_turn_maps_to_provider_request_failed() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let runtime = test_runtime(temp.path());
        let thread_id = create_thread(&runtime, &workspace, "failure mapping");
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

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &thread_id, "FAIL_RESULT");
        wait_for_status(&runtime, &thread_id, ThreadStatus::Error);
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { error, .. }
                if error.code == error_codes::PROVIDER_REQUEST_FAILED
        )));
        let _ = owner.shutdown();
    }

    #[test]
    fn failed_prepare_retires_generation_and_next_turn_starts_a_new_process() {
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
        )
        .with_env_for_test("FAKE_CLAUDE_FAIL_PREPARE", "1");

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
            Some("claude-session-fresh")
        );
        let starts = fs::read_to_string(&start_log).unwrap();
        assert_eq!(starts.lines().count(), 2);
        let frames = stdin_frames(&stdin_log);
        assert_eq!(frames.len(), 2);
        assert!(frames[0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("[Session Preparation]"));
        assert_eq!(
            frames[1]["message"]["content"],
            "recover after failed prepare"
        );
        let _ = owner.shutdown();
    }

    #[test]
    fn idle_crash_replaces_generation_and_resumes_persisted_session() {
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

        dispatch_prepare(&dispatcher, &runtime, &thread_id);
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        dispatch_turn(
            &dispatcher,
            &runtime,
            &thread_id,
            "finish then EXIT_AFTER_RESULT",
        );
        wait_for_status(&runtime, &thread_id, ThreadStatus::Idle);
        assert_eq!(
            provider_session_id(&runtime, &thread_id).as_deref(),
            Some("claude-session-fresh")
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
        let start_pids = start_log_pids(&start_log);
        assert_eq!(start_pids.len(), 2);
        assert!(starts.contains("--resume claude-session-fresh"));
        assert!(starts.contains("--append-system-prompt"));
        assert!(!starts.contains("--disable-slash-commands"));
        let frames = stdin_frames(&stdin_log);
        assert_eq!(
            frames.last().unwrap()["message"]["content"],
            "resumed raw task"
        );
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

        dispatch_prepare(&dispatcher, &runtime, &healthy);
        wait_for_status(&runtime, &healthy, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &healthy, "healthy task");
        wait_for_status(&runtime, &healthy, ThreadStatus::Idle);
        let healthy_pid = dispatcher
            .current_controller(&healthy)
            .unwrap()
            .process_id();
        dispatch_prepare(&dispatcher, &runtime, &failed);
        wait_for_status(&runtime, &failed, ThreadStatus::Idle);
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
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Claude end should use persistent runtime");
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
    fn ending_live_thread_keeps_other_claude_runtime_alive() {
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
        dispatch_prepare(&dispatcher, &runtime, &first);
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        dispatch_turn(&dispatcher, &runtime, &first, "first task");
        wait_for_status(&runtime, &first, ThreadStatus::Idle);
        dispatch_prepare(&dispatcher, &runtime, &second);
        wait_for_status(&runtime, &second, ThreadStatus::Idle);
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
        let EndThreadExecutionIntent::PersistentRuntime(operation) = end.execution else {
            panic!("Claude end should use persistent runtime");
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
        dispatcher.record_shutdown_diagnostics();
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
                provider: ProviderCode::Claude,
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
    ) -> ClaudeRuntimeDispatcher {
        ClaudeRuntimeDispatcher::new(owner, runtime)
            .with_program_for_test(fake_program(temp))
            .with_env_for_test("FAKE_CLAUDE_LOG", stdin_log.as_os_str())
            .with_env_for_test("FAKE_CLAUDE_START_LOG", start_log.as_os_str())
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
            panic!("Claude prepare should produce a persistent operation");
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
            panic!("Claude turn should produce a persistent operation");
        };
        dispatcher.dispatch(operation).unwrap();
    }

    fn wait_for_status(runtime: &SharedCoreRuntime, thread_id: &str, expected: ThreadStatus) {
        wait_until(Duration::from_secs(8), || {
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
            let target = directory.join("fake-claude.cmd");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_claude_stream.ps1");
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
            let target = directory.join("fake-claude");
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../pedelec-runtime/tests/fixtures/fake_claude_stream.sh");
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
