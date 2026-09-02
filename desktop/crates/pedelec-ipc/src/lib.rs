use encoding_rs::Encoding;
use pedelec_core::{
    error_codes, inspect_workspace_folder, wait_for_provider_readiness, CreateAssetDownloadInput,
    CreateAssetUploadInput, CreateThreadInput, EndThreadExecutionIntent, EndThreadInput,
    ListAssetsInput, PedelecError, PersistentRuntimeOperation, PrepareThreadInput,
    PrepareThreadOutput, ProviderExecutionIntent, ProviderProcessTermination, ProviderRpcTraffic,
    ProviderRuntimeDiagnostic, RunningProviderProcessPurpose, SendTextInput, SharedCoreRuntime,
    SubmitToolResultInput, SubscribeThreadInput, ThreadEvent, ToolCallInput, ToolInvocationOutcome,
    ToolInvocationRegistration, ToolInvocationWait, ToolSpecInput,
};
use pedelec_runtime::{
    CodexAppServerController, CodexApprovalPolicy, CodexReasoningEffort, CodexRuntimeError,
    CodexRuntimeEvent, CodexRuntimeLaunchConfig, CodexSandboxMode, CodexSessionConfig,
    CodexTurnConfig, CodexTurnSandboxPolicy, CodexTurnStatus, ProviderRuntimeController,
    ProviderRuntimeOwner, RpcTrafficRecord, RuntimeRegistryError, CODEX_RUNTIME_KEY,
};
use pedelec_shared::paths::path_for_external_use;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod opencode;
pub use opencode::{
    AcpProviderKind, AcpRuntimeDispatcher, CursorRuntimeDispatcher, OpenCodeRuntimeDispatcher,
    CURSOR_RUNTIME_KEY, OPENCODE_RUNTIME_KEY,
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const CORE_IPC_PROTOCOL: &str = "pedelec-core-ipc-v1";
pub const CORE_IPC_HOST: &str = "127.0.0.1";
pub const MAX_CORE_IPC_MESSAGE_BYTES: usize = 1024 * 1024;

/// The IPC layer owns dispatch orchestration, while the desktop runtime owns
/// the persistent provider implementation. Implementations must enqueue or
/// otherwise accept the semantic operation without requiring the Core mutex.
pub trait PersistentRuntimeDispatcher: Send + Sync + 'static {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError>;
}

/// Production provider router sharing one desktop-owned runtime registry.
#[derive(Debug, Clone)]
pub struct ProviderRuntimeDispatcher {
    codex: CodexRuntimeDispatcher,
    opencode: OpenCodeRuntimeDispatcher,
    cursor: CursorRuntimeDispatcher,
}

impl ProviderRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self {
            codex: CodexRuntimeDispatcher::new(owner.clone(), core_runtime.clone()),
            opencode: OpenCodeRuntimeDispatcher::new(owner.clone(), core_runtime.clone()),
            cursor: CursorRuntimeDispatcher::new(owner, core_runtime),
        }
    }

    pub fn shutdown(&self) -> Vec<String> {
        self.opencode.record_shutdown_diagnostic();
        self.cursor.record_shutdown_diagnostic();
        self.codex.shutdown()
    }
}

impl PersistentRuntimeDispatcher for ProviderRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        match operation.provider() {
            pedelec_core::ProviderCode::Codex => self.codex.dispatch(operation),
            pedelec_core::ProviderCode::OpenCode => self.opencode.dispatch(operation),
            pedelec_core::ProviderCode::Cursor => self.cursor.dispatch(operation),
            provider => Err(PedelecError::with_details(
                error_codes::PROVIDER_RUNTIME_START_FAILED,
                "provider has not migrated to a persistent production runtime",
                serde_json::json!({"provider": provider}),
            )),
        }
    }
}

/// Production Codex dispatcher. It owns no Core state beyond a shared handle,
/// and never performs provider I/O while the Core mutex is held.
#[derive(Debug, Clone)]
pub struct CodexRuntimeDispatcher {
    owner: ProviderRuntimeOwner,
    core_runtime: SharedCoreRuntime,
    program_override: Option<PathBuf>,
    process_cwd: PathBuf,
    typed_controller: Arc<Mutex<Option<Arc<CodexAppServerController>>>>,
    event_pumps: Arc<Mutex<HashSet<u64>>>,
    traffic_pumps: Arc<Mutex<HashSet<u64>>>,
    stopped_generations: Arc<Mutex<HashSet<u64>>>,
}

impl CodexRuntimeDispatcher {
    pub fn new(owner: ProviderRuntimeOwner, core_runtime: SharedCoreRuntime) -> Self {
        Self {
            owner,
            core_runtime,
            program_override: None,
            process_cwd: std::env::temp_dir(),
            typed_controller: Arc::new(Mutex::new(None)),
            event_pumps: Arc::new(Mutex::new(HashSet::new())),
            traffic_pumps: Arc::new(Mutex::new(HashSet::new())),
            stopped_generations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Deterministic fake-server seam for protocol tests. Production callers
    /// should let the completed provider scan select the Codex executable.
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

    fn controller_for(
        &self,
        runtime_file: &Path,
        operation_thread_id: &str,
    ) -> Result<Arc<CodexAppServerController>, PedelecError> {
        // A caller may reach the dispatcher before the old generation's
        // event-pump thread has delivered its disconnect event. Reduce its
        // Core impact before installing a replacement, otherwise the stale
        // pump would be correctly ignored but active old-generation threads
        // could remain stuck in Running/WaitingToolResult.
        self.fail_unhealthy_controller(Some(operation_thread_id));
        let program_override = self.program_override.clone();
        let core_runtime = Arc::clone(&self.core_runtime);
        let process_cwd = self.process_cwd.clone();
        let runtime_file = runtime_file.to_path_buf();
        let typed_controller = Arc::clone(&self.typed_controller);
        self.owner
            .get_or_init(CODEX_RUNTIME_KEY, move || {
                let program = match program_override {
                    Some(program) => program,
                    None => core_runtime
                        .lock()
                        .map_err(|_| {
                            RuntimeRegistryError::Initialization(
                                "Core runtime mutex was poisoned".to_string(),
                            )
                        })?
                        .provider_executable_path(&pedelec_core::ProviderCode::Codex)
                        .map_err(|error| {
                            RuntimeRegistryError::Initialization(format!(
                                "{} ({})",
                                error.message, error.code
                            ))
                        })?,
                };
                let mut launch = CodexRuntimeLaunchConfig::new(program, process_cwd);
                launch = launch.with_env("PEDELEC_PROVIDER", "codex");
                launch = launch.with_env(
                    "PEDELEC_CORE_IPC_RUNTIME_FILE",
                    runtime_file.to_string_lossy().into_owned(),
                );
                if let Some(path) = std::env::var_os("PATH") {
                    launch = launch.with_env("PATH", path);
                }
                let controller = CodexAppServerController::spawn(launch)
                    .map_err(|error| RuntimeRegistryError::Initialization(error.to_string()))?;
                *typed_controller.lock().map_err(|_| {
                    RuntimeRegistryError::Initialization(
                        "Codex controller mutex was poisoned".to_string(),
                    )
                })? = Some(Arc::clone(&controller));
                Ok(controller as std::sync::Arc<dyn ProviderRuntimeController>)
            })
            .map_err(|error| runtime_registry_error_to_pedelec(error, "startup"))?;
        self.typed_controller
            .lock()
            .map_err(|_| {
                PedelecError::new(
                    error_codes::PROVIDER_PROTOCOL_ERROR,
                    "Codex controller mutex was poisoned",
                )
            })?
            .clone()
            .ok_or_else(|| {
                runtime_registry_error_to_pedelec(
                    RuntimeRegistryError::Initialization(
                        "Codex controller was not installed".to_string(),
                    ),
                    "startup",
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
            "Codex App Server runtime disconnected",
            serde_json::json!({
                "provider": "codex",
                "operation": "runtime",
                "runtimeGeneration": controller.generation(),
                "processId": controller.process_id(),
                "reason": "replacement requested before disconnect event was reduced",
            }),
        );
        if let Ok(mut core) = self.core_runtime.lock() {
            core.fail_persistent_runtime_except(
                pedelec_core::ProviderCode::Codex,
                excluded_thread_id,
                error,
            );
        }
    }

    fn start_rpc_traffic_pump(&self, controller: Arc<CodexAppServerController>) {
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
        let process_id = controller.process_id();
        thread::spawn(move || {
            loop {
                match controller.recv_rpc_traffic_timeout(Duration::from_millis(50)) {
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
                        record_rpc_traffic(
                            &runtime,
                            pedelec_core::ProviderCode::Codex,
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

    fn start_event_pump(&self, controller: Arc<CodexAppServerController>) {
        self.start_rpc_traffic_pump(Arc::clone(&controller));
        let generation = controller.generation();
        let should_start = self
            .event_pumps
            .lock()
            .map(|mut generations| generations.insert(generation))
            .unwrap_or(false);
        if !should_start {
            return;
        }
        let runtime = Arc::clone(&self.core_runtime);
        record_runtime_diagnostic(
            &runtime,
            ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                provider: pedelec_core::ProviderCode::Codex,
                runtime_generation: generation,
                process_id: controller.process_id(),
            },
        );
        let runtime = Arc::clone(&self.core_runtime);
        let current_controller = Arc::clone(&self.typed_controller);
        let event_pumps = Arc::clone(&self.event_pumps);
        let stopped_generations = Arc::clone(&self.stopped_generations);
        let generation = controller.generation();
        thread::spawn(move || {
            while let Ok(event) = controller.recv_event() {
                // Keep the generation identity held through the reduction.
                // A check followed by an unlocked Core mutation could let an
                // old callback land after a replacement controller is
                // installed.
                let current = match current_controller.lock() {
                    Ok(current) => current,
                    Err(_) => break,
                };
                if !current
                    .as_ref()
                    .is_some_and(|active| active.generation() == generation)
                {
                    // A late event from an older runtime generation must not
                    // mutate a replacement generation's Core state.
                    break;
                }
                match event {
                    CodexRuntimeEvent::TurnStarted {
                        pedelec_thread_id,
                        provider_thread_id,
                        provider_turn_id,
                    } => {
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeTurnStarted {
                                provider: pedelec_core::ProviderCode::Codex,
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id: pedelec_thread_id.clone(),
                                provider_thread_id: provider_thread_id.clone(),
                                provider_turn_id: provider_turn_id.clone(),
                            },
                        );
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::TurnStarted {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id,
                                },
                            );
                        }
                    }
                    CodexRuntimeEvent::AssistantDelta {
                        pedelec_thread_id,
                        provider_turn_id,
                        text,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::AssistantDelta {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id,
                                    text,
                                },
                            );
                        }
                    }
                    CodexRuntimeEvent::AssistantMessage {
                        pedelec_thread_id,
                        provider_turn_id,
                        text,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::AssistantMessage {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id,
                                    text,
                                },
                            );
                        }
                    }
                    CodexRuntimeEvent::UsageUpdated {
                        pedelec_thread_id,
                        provider_turn_id,
                        usage,
                        ..
                    } => {
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::UsageUpdated {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id,
                                    usage,
                                },
                            );
                        }
                    }
                    CodexRuntimeEvent::TurnCompleted {
                        pedelec_thread_id,
                        provider_thread_id,
                        provider_turn_id,
                        status,
                        error,
                    } => {
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeTurnCompleted {
                                provider: pedelec_core::ProviderCode::Codex,
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id: pedelec_thread_id.clone(),
                                provider_thread_id: provider_thread_id.clone(),
                                provider_turn_id: provider_turn_id.clone(),
                                status: codex_turn_status_label(status).to_string(),
                            },
                        );
                        let success = status == CodexTurnStatus::Completed;
                        let error = if success {
                            None
                        } else {
                            Some(codex_turn_completion_error(
                                &pedelec_thread_id,
                                &provider_thread_id,
                                provider_turn_id.as_deref(),
                                status,
                                error,
                                controller.generation(),
                                controller.process_id(),
                            ))
                        };
                        if let Ok(mut core) = runtime.lock() {
                            let _ = core.reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::TurnCompleted {
                                    thread_id: pedelec_thread_id,
                                    provider_turn_id,
                                    success,
                                    error,
                                },
                            );
                        }
                    }
                    CodexRuntimeEvent::ProtocolError {
                        pedelec_thread_id,
                        provider_thread_id,
                        operation,
                        message,
                    } => {
                        controller.retire_for_protocol_error();
                        let error = PedelecError::with_details(
                            error_codes::PROVIDER_PROTOCOL_ERROR,
                            message,
                            serde_json::json!({
                                "provider": "codex",
                                "operation": operation,
                                "stage": "stream",
                                "providerThreadId": provider_thread_id,
                                "runtimeGeneration": controller.generation(),
                                "processId": controller.process_id(),
                            }),
                        );
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeError {
                                provider: pedelec_core::ProviderCode::Codex,
                                runtime_generation: Some(controller.generation()),
                                process_id: Some(controller.process_id()),
                                thread_id: pedelec_thread_id,
                                provider_thread_id,
                                provider_turn_id: None,
                                code: error_codes::PROVIDER_PROTOCOL_ERROR.to_string(),
                                message: error.message.clone(),
                                details: error.details.clone(),
                            },
                        );
                        if let Ok(mut core) = runtime.lock() {
                            core.fail_persistent_runtime(pedelec_core::ProviderCode::Codex, error);
                        }
                    }
                    CodexRuntimeEvent::Disconnected {
                        generation,
                        pid,
                        attachments,
                        reason,
                    } => {
                        let reason_text = format!("{reason:?}");
                        record_runtime_diagnostic(
                            &runtime,
                            ProviderRuntimeDiagnostic::ProviderRuntimeDisconnected {
                                provider: pedelec_core::ProviderCode::Codex,
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
                                    provider: pedelec_core::ProviderCode::Codex,
                                    runtime_generation: generation,
                                    process_id: pid,
                                    thread_id: Some(attachment.pedelec_thread_id.clone()),
                                    provider_thread_id: Some(attachment.provider_thread_id.clone()),
                                    reason: reason_text.clone(),
                                },
                            );
                        }
                        let error = PedelecError::with_details(
                            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                            "Codex App Server runtime disconnected",
                            serde_json::json!({
                                "provider": "codex",
                                "operation": "runtime",
                                "runtimeGeneration": generation,
                                "processId": pid,
                                "reason": reason_text,
                                "attachments": attachments
                                    .iter()
                                    .map(|attachment| {
                                        serde_json::json!({
                                            "pedelecThreadId": attachment.pedelec_thread_id,
                                            "providerThreadId": attachment.provider_thread_id,
                                        })
                                    })
                                    .collect::<Vec<_>>(),
                            }),
                        );
                        if let Ok(mut core) = runtime.lock() {
                            core.fail_persistent_runtime(pedelec_core::ProviderCode::Codex, error);
                        }
                        mark_runtime_stopped(
                            &runtime,
                            &stopped_generations,
                            generation,
                            pid,
                            "runtime disconnected",
                        );
                        break;
                    }
                    CodexRuntimeEvent::Notification { .. } => {}
                    CodexRuntimeEvent::Stderr { text } => {
                        record_runtime_diagnostic(
                            &runtime,
                            codex_runtime_stderr_diagnostic(
                                controller.generation(),
                                controller.process_id(),
                                text,
                            ),
                        );
                    }
                }
            }
            if let Ok(mut generations) = event_pumps.lock() {
                generations.remove(&generation);
            }
        });
    }
}

impl PersistentRuntimeDispatcher for CodexRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        if let PersistentRuntimeOperation::EndSession { session } = &operation {
            if session.provider != pedelec_core::ProviderCode::Codex {
                return Err(PedelecError::new(
                    error_codes::PROVIDER_UNSUPPORTED,
                    "Codex runtime received a non-Codex end operation",
                ));
            }
            return self.dispatch_end_session(session);
        }

        let (thread_id, runtime_file) = match &operation {
            PersistentRuntimeOperation::EnsureSession { session } => {
                if session.provider != pedelec_core::ProviderCode::Codex {
                    return Err(PedelecError::new(
                        error_codes::PROVIDER_UNSUPPORTED,
                        "Codex runtime received a non-Codex session",
                    ));
                }
                (
                    session.thread_id.clone(),
                    session.core_ipc_runtime_file_path.clone(),
                )
            }
            PersistentRuntimeOperation::StartTurn { turn } => {
                if turn.session.provider != pedelec_core::ProviderCode::Codex {
                    return Err(PedelecError::new(
                        error_codes::PROVIDER_UNSUPPORTED,
                        "Codex runtime received a non-Codex turn",
                    ));
                }
                (
                    turn.thread_id.clone(),
                    turn.session.core_ipc_runtime_file_path.clone(),
                )
            }
            PersistentRuntimeOperation::EndSession { .. } => {
                unreachable!("persistent Codex end operations are handled before session dispatch")
            }
        };

        let controller = self.controller_for(&runtime_file, &thread_id)?;
        // Install the pump after the controller is in the registry, so the
        // controller remains alive for its entire receive loop. Starting it
        // for every operation also makes a runtime replacement observable if
        // a later operation is the first one to reach the dispatcher.
        self.start_event_pump(Arc::clone(&controller));

        match operation {
            PersistentRuntimeOperation::EnsureSession { session } => {
                let config = codex_session_config(&session)?;
                let result = controller.ensure_session(
                    &thread_id,
                    session.provider_session_id.as_deref(),
                    &config,
                );
                match result {
                    Ok(result) => {
                        let provider_thread_id = result.provider_thread_id.clone();
                        self.core_runtime
                            .lock()
                            .map_err(|_| {
                                PedelecError::new(
                                    error_codes::CORE_RUNTIME_UNAVAILABLE,
                                    "Core runtime mutex was poisoned",
                                )
                            })?
                            .reduce_provider_runtime_event(
                                pedelec_core::ProviderRuntimeEvent::SessionReady {
                                    thread_id: thread_id.clone(),
                                    provider_session_id: provider_thread_id.clone(),
                                },
                            )?;
                        self.record_runtime_diagnostic(
                            ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                                provider: pedelec_core::ProviderCode::Codex,
                                runtime_generation: controller.generation(),
                                process_id: controller.process_id(),
                                thread_id,
                                provider_thread_id,
                                resumed: result.resumed,
                            },
                        );
                        Ok(())
                    }
                    Err(error) => Err(codex_error_to_pedelec_with_context(
                        error,
                        Some(&controller),
                        &thread_id,
                        session.provider_session_id.as_deref(),
                        None,
                        "admission",
                    )),
                }
            }
            PersistentRuntimeOperation::StartTurn { turn } => {
                let config = codex_session_config(&turn.session)?;
                let session = controller
                    .ensure_session(&thread_id, turn.provider_session_id.as_deref(), &config)
                    .map_err(|error| {
                        codex_error_to_pedelec_with_context(
                            error,
                            Some(&controller),
                            &thread_id,
                            turn.provider_session_id.as_deref(),
                            None,
                            "admission",
                        )
                    })?;
                if let Err(error) = self_reduce_session_ready(
                    &self.core_runtime,
                    &thread_id,
                    &session.provider_thread_id,
                ) {
                    return Err(error);
                }
                self.record_runtime_diagnostic(
                    ProviderRuntimeDiagnostic::ProviderRuntimeAttached {
                        provider: pedelec_core::ProviderCode::Codex,
                        runtime_generation: controller.generation(),
                        process_id: controller.process_id(),
                        thread_id: thread_id.clone(),
                        provider_thread_id: session.provider_thread_id.clone(),
                        resumed: session.resumed,
                    },
                );
                let turn_config = CodexTurnConfig {
                    input: turn.message,
                    cwd: turn.session.workspace_path,
                    model: turn.session.model,
                    effort: turn.session.reasoning_effort.map(|effort| match effort {
                        pedelec_core::CodexReasoningEffort::Low => CodexReasoningEffort::Low,
                        pedelec_core::CodexReasoningEffort::Medium => CodexReasoningEffort::Medium,
                        pedelec_core::CodexReasoningEffort::High => CodexReasoningEffort::High,
                        pedelec_core::CodexReasoningEffort::XHigh => CodexReasoningEffort::XHigh,
                        pedelec_core::CodexReasoningEffort::Max => CodexReasoningEffort::Max,
                    }),
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox_policy: CodexTurnSandboxPolicy::DangerFullAccess,
                };
                controller
                    .start_turn(
                        &thread_id,
                        &session.provider_thread_id,
                        &turn.local_turn_id,
                        &turn_config,
                    )
                    .map_err(|error| {
                        codex_error_to_pedelec_with_context(
                            error,
                            Some(&controller),
                            &thread_id,
                            Some(&session.provider_thread_id),
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

impl CodexRuntimeDispatcher {
    fn record_runtime_diagnostic(&self, diagnostic: ProviderRuntimeDiagnostic) {
        record_runtime_diagnostic(&self.core_runtime, diagnostic);
    }

    /// Stops the app-lifetime Codex runtime and publishes the lifecycle
    /// diagnostic before the owner closes its registry. The runtime is not
    /// stopped when the last Pedelec session ends.
    pub fn shutdown(&self) -> Vec<String> {
        let controller = self
            .typed_controller
            .lock()
            .ok()
            .and_then(|controller| controller.clone());
        if let Some(controller) = controller {
            let generation = controller.generation();
            let process_id = controller.process_id();
            mark_runtime_stopped(
                &self.core_runtime,
                &self.stopped_generations,
                generation,
                process_id,
                "desktop shutdown",
            );
        }
        self.owner.shutdown()
    }

    fn current_controller(&self) -> Option<Arc<CodexAppServerController>> {
        self.typed_controller
            .lock()
            .ok()
            .and_then(|controller| controller.clone())
            .filter(|controller| controller.is_healthy())
    }

    fn dispatch_end_session(
        &self,
        session: &pedelec_core::PersistentProviderEndIntent,
    ) -> Result<(), PedelecError> {
        let Some(controller) = self.current_controller() else {
            if session.active_provider_turn_id.is_some() {
                let error = runtime_disconnect_end_error(
                    session,
                    "the active Codex turn has no healthy runtime generation",
                    None,
                );
                if let Ok(mut core) = self.core_runtime.lock() {
                    core.fail_persistent_runtime(pedelec_core::ProviderCode::Codex, error.clone());
                }
                return Err(error);
            }
            // An idle detached session must not cause a new App Server to be
            // started solely for unsubscribe cleanup.
            return Ok(());
        };

        match controller.end_session(
            &session.thread_id,
            session.active_provider_turn_id.as_deref(),
        ) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Any inability to prove an active interrupt is a hard safety
                // failure. Stop the shared generation and reduce every active
                // Codex thread; Core finalizes the target end operation.
                controller.retire_for_protocol_error();
                let mapped =
                    runtime_disconnect_end_error(session, &error.to_string(), Some(&controller));
                if let Ok(mut core) = self.core_runtime.lock() {
                    core.fail_persistent_runtime(pedelec_core::ProviderCode::Codex, mapped);
                }
                Err(codex_error_to_pedelec_with_context(
                    error,
                    Some(&controller),
                    &session.thread_id,
                    session.provider_session_id.as_deref(),
                    session.active_provider_turn_id.as_deref(),
                    "end",
                ))
            }
        }
    }
}

fn runtime_disconnect_end_error(
    session: &pedelec_core::PersistentProviderEndIntent,
    message: &str,
    controller: Option<&CodexAppServerController>,
) -> PedelecError {
    let mut details = serde_json::json!({
        "provider": "codex",
        "operation": "end",
        "threadId": session.thread_id,
        "reason": message,
    });
    if let Some(provider_session_id) = &session.provider_session_id {
        details["providerThreadId"] = serde_json::json!(provider_session_id);
    }
    if let Some(provider_turn_id) = &session.active_provider_turn_id {
        details["providerTurnId"] = serde_json::json!(provider_turn_id);
    }
    if let Some(controller) = controller {
        details["runtimeGeneration"] = serde_json::json!(controller.generation());
        details["processId"] = serde_json::json!(controller.process_id());
    }
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_DISCONNECTED,
        "Codex App Server runtime was hard-stopped after active interruption could not be confirmed",
        details,
    )
}

fn record_runtime_diagnostic(runtime: &SharedCoreRuntime, diagnostic: ProviderRuntimeDiagnostic) {
    if let Ok(mut core) = runtime.lock() {
        core.record_provider_runtime_diagnostic(diagnostic);
    }
}

fn codex_runtime_stderr_diagnostic(
    runtime_generation: u64,
    process_id: u32,
    text: String,
) -> ProviderRuntimeDiagnostic {
    ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
        provider: pedelec_core::ProviderCode::Codex,
        runtime_generation,
        process_id,
        text,
    }
}

pub(crate) fn record_rpc_traffic(
    runtime: &SharedCoreRuntime,
    provider: pedelec_core::ProviderCode,
    runtime_generation: u64,
    process_id: u32,
    record: RpcTrafficRecord,
) {
    if let Ok(mut core) = runtime.lock() {
        core.record_provider_rpc_traffic(ProviderRpcTraffic {
            event_type: "provider_rpc_traffic".to_string(),
            provider,
            runtime_generation,
            process_id,
            thread_id: record.thread_id,
            ts: record.ts,
            direction: record.direction,
            kind: record.kind,
            message: record.message,
            unmatched: record.unmatched.then_some(true),
        });
    }
}

fn mark_runtime_stopped(
    runtime: &SharedCoreRuntime,
    stopped_generations: &Mutex<HashSet<u64>>,
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
                provider: pedelec_core::ProviderCode::Codex,
                runtime_generation: generation,
                process_id,
                reason: reason.to_string(),
            },
        );
    }
}

fn codex_turn_status_label(status: CodexTurnStatus) -> &'static str {
    match status {
        CodexTurnStatus::Completed => "completed",
        CodexTurnStatus::Failed => "failed",
        CodexTurnStatus::Interrupted => "interrupted",
    }
}

const MAX_RUNTIME_DIAGNOSTIC_TEXT_BYTES: usize = 4096;

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

fn self_reduce_session_ready(
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
        .reduce_provider_runtime_event(pedelec_core::ProviderRuntimeEvent::SessionReady {
            thread_id: thread_id.to_string(),
            provider_session_id: provider_thread_id.to_string(),
        })
}

fn codex_session_config(
    session: &pedelec_core::PersistentProviderSessionIntent,
) -> Result<CodexSessionConfig, PedelecError> {
    let effort = session.reasoning_effort.map(|effort| match effort {
        pedelec_core::CodexReasoningEffort::Low => CodexReasoningEffort::Low,
        pedelec_core::CodexReasoningEffort::Medium => CodexReasoningEffort::Medium,
        pedelec_core::CodexReasoningEffort::High => CodexReasoningEffort::High,
        pedelec_core::CodexReasoningEffort::XHigh => CodexReasoningEffort::XHigh,
        pedelec_core::CodexReasoningEffort::Max => CodexReasoningEffort::Max,
    });
    let approval_policy = match session.approval_policy {
        pedelec_core::PersistentApprovalPolicy::Never => CodexApprovalPolicy::Never,
    };
    let sandbox = match session.sandbox_policy {
        pedelec_core::PersistentSandboxPolicy::ReadOnly => CodexSandboxMode::ReadOnly,
    };
    let config = CodexSessionConfig {
        model: session.model.clone(),
        effort,
        cwd: session.workspace_path.clone(),
        approval_policy,
        sandbox,
        developer_instructions: session.host_instructions.clone(),
        config: session.config.clone(),
    };
    config
        .validate()
        .map_err(|error| codex_error_to_pedelec(error, None))?;
    Ok(config)
}

fn runtime_registry_error_to_pedelec(error: RuntimeRegistryError, operation: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_RUNTIME_START_FAILED,
        "Codex App Server runtime could not be started",
        serde_json::json!({
            "provider": "codex",
            "operation": operation,
            "error": error.to_string(),
        }),
    )
}

fn codex_error_to_pedelec(
    error: CodexRuntimeError,
    controller: Option<&CodexAppServerController>,
) -> PedelecError {
    let (code, operation) = match &error {
        CodexRuntimeError::RuntimeStart { operation, .. } => {
            (error_codes::PROVIDER_RUNTIME_START_FAILED, operation)
        }
        CodexRuntimeError::RuntimeDisconnected { operation, .. } => {
            (error_codes::PROVIDER_RUNTIME_DISCONNECTED, operation)
        }
        CodexRuntimeError::Protocol { operation, .. } => {
            (error_codes::PROVIDER_PROTOCOL_ERROR, operation)
        }
        CodexRuntimeError::Request { operation, .. } => {
            (error_codes::PROVIDER_REQUEST_FAILED, operation)
        }
    };
    let mut details = serde_json::json!({
        "provider": "codex",
        "operation": operation,
        "error": error.to_string(),
    });
    if let CodexRuntimeError::Request {
        details: Some(rpc_details),
        ..
    } = &error
    {
        details["rpc"] = rpc_details.clone();
    }
    if let Some(controller) = controller {
        details["runtimeGeneration"] = serde_json::json!(controller.generation());
        details["processId"] = serde_json::json!(controller.process_id());
    }
    PedelecError::with_details(code, error.to_string(), details)
}

fn codex_error_to_pedelec_with_context(
    error: CodexRuntimeError,
    controller: Option<&CodexAppServerController>,
    thread_id: &str,
    provider_thread_id: Option<&str>,
    provider_turn_id: Option<&str>,
    stage: &str,
) -> PedelecError {
    let mut mapped = codex_error_to_pedelec(error.clone(), controller);
    let mut details = mapped
        .details
        .take()
        .unwrap_or_else(|| serde_json::json!({}));
    details["provider"] = serde_json::json!("codex");
    details["threadId"] = serde_json::json!(thread_id);
    details["stage"] = serde_json::json!(stage);
    if let Some(provider_thread_id) = provider_thread_id {
        details["providerThreadId"] = serde_json::json!(provider_thread_id);
    }
    if let Some(provider_turn_id) = provider_turn_id {
        details["providerTurnId"] = serde_json::json!(provider_turn_id);
    }
    mapped.details = Some(details);
    mapped
}

fn codex_turn_completion_error(
    thread_id: &str,
    provider_thread_id: &str,
    provider_turn_id: Option<&str>,
    status: CodexTurnStatus,
    codex_error: Option<Value>,
    runtime_generation: u64,
    process_id: u32,
) -> PedelecError {
    let message = codex_error
        .as_ref()
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            match status {
                CodexTurnStatus::Failed => "Codex turn failed",
                CodexTurnStatus::Interrupted => "Codex turn was interrupted",
                CodexTurnStatus::Completed => "Codex turn failed",
            }
            .to_string()
        });
    let mut details = serde_json::json!({
        "provider": "codex",
        "operation": "turn",
        "stage": "completed",
        "threadId": thread_id,
        "providerThreadId": provider_thread_id,
        "runtimeGeneration": runtime_generation,
        "processId": process_id,
        "status": match status {
            CodexTurnStatus::Completed => "completed",
            CodexTurnStatus::Failed => "failed",
            CodexTurnStatus::Interrupted => "interrupted",
        },
    });
    if let Some(provider_turn_id) = provider_turn_id {
        details["providerTurnId"] = serde_json::json!(provider_turn_id);
    }
    if let Some(codex_error) = codex_error {
        details["codexError"] = codex_error;
    }
    PedelecError::with_details(error_codes::PROVIDER_REQUEST_FAILED, message, details)
}

#[derive(Debug, Default)]
pub struct RejectPersistentRuntimeDispatcher;

impl PersistentRuntimeDispatcher for RejectPersistentRuntimeDispatcher {
    fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
        Err(PedelecError::with_details(
            error_codes::PROVIDER_RUNTIME_START_FAILED,
            "persistent provider runtime dispatcher is unavailable",
            serde_json::json!({
                "threadId": operation.thread_id(),
                "operation": format!("{:?}", operation.kind())
            }),
        ))
    }
}

/// Desktop capabilities that Core IPC can invoke without coupling the Core
/// runtime to a particular desktop toolkit.
pub trait CoreIpcPlatformServices: Send + Sync + 'static {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError>;
}

#[derive(Debug, Default)]
pub struct NoopCoreIpcPlatformServices;

impl CoreIpcPlatformServices for NoopCoreIpcPlatformServices {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError> {
        Err(PedelecError::new(
            error_codes::DIRECTORY_PICKER_FAILED,
            "native directory picker is unavailable",
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeFile {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub endpoint: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcRequest {
    pub request_id: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_sdk_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcResponse {
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcEventMessage {
    pub r#type: String,
    pub event: ThreadEvent,
}

pub struct CoreIpcServerHandle {
    pub runtime_file: RuntimeFile,
    pub runtime_file_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCoreIpcRequest {
    request_id: Option<String>,
    r#type: Option<String>,
    payload: Option<Value>,
    caller_origin: Option<String>,
    caller_sdk_version: Option<String>,
}

struct HandledCoreIpcResponse {
    response: CoreIpcResponse,
    tool_delivery_request_id: Option<String>,
}

#[cfg(test)]
static FORCED_TOOL_RESPONSE_WRITE_FAILURES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Forces one actual Core IPC tool response write to fail in tests. The
/// request still runs through the socket handler and terminalizes normally,
/// so tests can verify the delivery acknowledgment contract.
#[cfg(test)]
pub fn force_tool_response_write_failure_for_test(request_id: impl Into<String>) {
    FORCED_TOOL_RESPONSE_WRITE_FAILURES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(request_id.into());
}

#[cfg(test)]
fn should_force_tool_response_write_failure(request_id: &str) -> bool {
    FORCED_TOOL_RESPONSE_WRITE_FAILURES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .remove(request_id)
}

pub fn start_core_ipc_server(
    runtime: SharedCoreRuntime,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    start_core_ipc_server_with_runtime_path_and_services(
        runtime,
        runtime_file_path,
        Arc::new(NoopCoreIpcPlatformServices),
    )
}

pub fn start_core_ipc_server_with_runtime_path(
    runtime: SharedCoreRuntime,
    runtime_file_path: impl Into<PathBuf>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    start_core_ipc_server_with_runtime_path_and_services(
        runtime,
        runtime_file_path,
        Arc::new(NoopCoreIpcPlatformServices),
    )
}

pub fn start_core_ipc_server_with_services(
    runtime: SharedCoreRuntime,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    start_core_ipc_server_with_runtime_path_and_services(
        runtime,
        runtime_file_path,
        platform_services,
    )
}

pub fn start_core_ipc_server_with_services_and_dispatcher(
    runtime: SharedCoreRuntime,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    start_core_ipc_server_with_runtime_path_services_and_dispatcher(
        runtime,
        runtime_file_path,
        platform_services,
        persistent_dispatcher,
    )
}

pub fn start_core_ipc_server_with_runtime_path_and_services(
    runtime: SharedCoreRuntime,
    runtime_file_path: impl Into<PathBuf>,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    start_core_ipc_server_with_runtime_path_services_and_dispatcher(
        runtime,
        runtime_file_path,
        platform_services,
        Arc::new(RejectPersistentRuntimeDispatcher),
    )
}

pub fn start_core_ipc_server_with_runtime_path_services_and_dispatcher(
    runtime: SharedCoreRuntime,
    runtime_file_path: impl Into<PathBuf>,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let listener = TcpListener::bind((CORE_IPC_HOST, 0)).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot bind Core IPC server",
            serde_json::json!({ "host": CORE_IPC_HOST, "error": err.to_string() }),
        )
    })?;
    let local_addr = listener.local_addr().map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot resolve Core IPC server address",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    let runtime_file = RuntimeFile {
        protocol: CORE_IPC_PROTOCOL.to_string(),
        host: CORE_IPC_HOST.to_string(),
        port: local_addr.port(),
        endpoint: local_addr.to_string(),
        pid: std::process::id(),
    };
    let runtime_file_path = runtime_file_path.into();
    write_runtime_file(&runtime_file_path, &runtime_file)?;
    runtime
        .lock()
        .unwrap()
        .set_core_ipc_runtime(runtime_file.endpoint.clone(), runtime_file_path.clone());

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else {
                continue;
            };
            let runtime = Arc::clone(&runtime);
            let platform_services = Arc::clone(&platform_services);
            let persistent_dispatcher = Arc::clone(&persistent_dispatcher);
            thread::spawn(move || {
                let _ = handle_core_ipc_connection(
                    stream,
                    runtime,
                    platform_services,
                    persistent_dispatcher,
                );
            });
        }
    });

    Ok(CoreIpcServerHandle {
        runtime_file,
        runtime_file_path,
    })
}

pub fn default_runtime_file_path() -> Result<PathBuf, PedelecError> {
    dirs::home_dir()
        .map(|home| home.join(".pedelec").join("runtime.json"))
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::IPC_UNAVAILABLE,
                "cannot resolve user home directory for runtime.json",
            )
        })
}

pub fn send_core_ipc_request(request: &CoreIpcRequest) -> Result<CoreIpcResponse, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    send_core_ipc_request_with_runtime_path(request, runtime_file_path)
}

pub fn send_core_ipc_request_with_runtime_path(
    request: &CoreIpcRequest,
    runtime_file_path: impl AsRef<Path>,
) -> Result<CoreIpcResponse, PedelecError> {
    let mut stream = connect_core_ipc_with_runtime_path(runtime_file_path)?;
    write_json_line(&mut stream, request).map_err(core_unavailable_error)?;

    let mut reader = BufReader::new(stream);
    let line = read_bounded_json_line(&mut reader).map_err(core_unavailable_error)?;
    let value: Value = serde_json::from_slice(&line).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC response was not valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    serde_json::from_value(value).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC response had invalid shape",
            serde_json::json!({ "error": err.to_string() }),
        )
    })
}

pub fn connect_core_ipc() -> Result<TcpStream, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    connect_core_ipc_with_runtime_path(runtime_file_path)
}

pub fn connect_core_ipc_with_runtime_path(
    runtime_file_path: impl AsRef<Path>,
) -> Result<TcpStream, PedelecError> {
    let runtime_file = read_runtime_file(runtime_file_path)?;
    if runtime_file.protocol != CORE_IPC_PROTOCOL || runtime_file.host != CORE_IPC_HOST {
        return Err(PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        ));
    }

    TcpStream::connect(&runtime_file.endpoint).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })
}

pub fn write_json_line<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    writer.write_all(&payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub fn read_bounded_json_line<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();

    loop {
        let (consumed, found_newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if output.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Core IPC connection closed",
                    ));
                }
                return Ok(output);
            }

            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    output.extend_from_slice(&available[..index]);
                    (index + 1, true)
                }
                None => {
                    output.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        reader.consume(consumed);

        if output.len() > MAX_CORE_IPC_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Core IPC message exceeds size limit",
            ));
        }

        if found_newline {
            break;
        }
    }

    Ok(output)
}

fn handle_core_ipc_connection(
    stream: TcpStream,
    runtime: SharedCoreRuntime,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
) -> io::Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let writer = Arc::new(Mutex::new(stream));

    loop {
        let line = match read_bounded_json_line(&mut reader) {
            Ok(line) => line,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                let response = error_response(
                    "",
                    PedelecError::new(
                        error_codes::MESSAGE_TOO_LARGE,
                        "Core IPC message exceeds size limit",
                    ),
                );
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        let value = match serde_json::from_slice::<Value>(&line) {
            Ok(value) => value,
            Err(err) => {
                let response = error_response(
                    "",
                    PedelecError::with_details(
                        error_codes::IPC_UNAVAILABLE,
                        "Core IPC request was not valid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    ),
                );
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                continue;
            }
        };

        let request = match parse_core_ipc_request(value) {
            Ok(request) => request,
            Err(response) => {
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                continue;
            }
        };

        if request.r#type == "subscribe_thread" {
            let response = handle_subscribe_thread(&request, &runtime, Arc::clone(&writer));
            let mut writer = writer.lock().unwrap();
            write_json_line(&mut *writer, &response)?;
            continue;
        }

        let handled = if request.r#type == "tool_call" {
            handle_tool_call_request_with_metadata(&request, Arc::clone(&runtime))
        } else {
            HandledCoreIpcResponse {
                response: handle_core_ipc_request_with_services(
                    request,
                    Arc::clone(&runtime),
                    Arc::clone(&platform_services),
                    Arc::clone(&persistent_dispatcher),
                ),
                tool_delivery_request_id: None,
            }
        };
        let mut writer = writer.lock().unwrap();
        let write_succeeded = write_tool_delivery_response(
            &mut *writer,
            &handled.response,
            handled.tool_delivery_request_id.as_deref(),
        )
        .is_ok();
        drop(writer);
        if write_succeeded {
            if let Some(request_id) = handled.tool_delivery_request_id {
                runtime
                    .lock()
                    .unwrap()
                    .tool_request_broker
                    .acknowledge_tool_delivery(&request_id);
            }
        } else {
            return Ok(());
        }
    }
}

fn write_tool_delivery_response<W: Write>(
    writer: &mut W,
    response: &CoreIpcResponse,
    _tool_delivery_request_id: Option<&str>,
) -> io::Result<()> {
    #[cfg(test)]
    if _tool_delivery_request_id.is_some()
        && should_force_tool_response_write_failure(&response.request_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test-forced Core IPC tool response write failure",
        ));
    }

    write_json_line(writer, response)
}

fn parse_core_ipc_request(value: Value) -> Result<CoreIpcRequest, CoreIpcResponse> {
    let raw: RawCoreIpcRequest = serde_json::from_value(value).map_err(|err| {
        error_response(
            "",
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "Core IPC request had invalid shape",
                serde_json::json!({ "error": err.to_string() }),
            ),
        )
    })?;

    let request_id = raw.request_id.unwrap_or_default();
    if request_id.trim().is_empty() {
        return Err(error_response(
            "",
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "requestId is required"),
        ));
    }

    let request_type = raw.r#type.unwrap_or_default();
    if request_type.trim().is_empty() {
        return Err(error_response(
            &request_id,
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "type is required"),
        ));
    }

    Ok(CoreIpcRequest {
        request_id,
        r#type: request_type,
        caller_origin: raw.caller_origin,
        caller_sdk_version: raw.caller_sdk_version,
        payload: raw.payload,
    })
}

#[allow(dead_code)]
fn handle_core_ipc_request(request: CoreIpcRequest, runtime: SharedCoreRuntime) -> CoreIpcResponse {
    handle_core_ipc_request_with_services(
        request,
        runtime,
        Arc::new(NoopCoreIpcPlatformServices),
        Arc::new(RejectPersistentRuntimeDispatcher),
    )
}

fn handle_core_ipc_request_with_services(
    request: CoreIpcRequest,
    runtime: SharedCoreRuntime,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
) -> CoreIpcResponse {
    match request.r#type.as_str() {
        "pick_workspace_folder" => {
            handle_pick_workspace_folder_request(&request, platform_services)
        }
        "create_thread" => match decode_payload::<CreateThreadInput>(&request) {
            Ok(input) => match match request.caller_origin.as_deref() {
                Some(origin) => runtime.lock().unwrap().create_sdk_thread(
                    input,
                    origin,
                    request.caller_sdk_version.as_deref(),
                ),
                None => runtime.lock().unwrap().create_thread(input),
            } {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "list_providers" => match wait_for_provider_readiness(&runtime) {
            Ok(()) => ok_response(
                &request.request_id,
                serde_json::json!(runtime.lock().unwrap().list_sdk_providers()),
            ),
            Err(err) => error_response(&request.request_id, err),
        },
        "get_settings" => match runtime.lock().unwrap().get_sdk_settings() {
            Ok(settings) => ok_response(&request.request_id, serde_json::json!(settings)),
            Err(err) => error_response(&request.request_id, err),
        },
        "ping" => ok_response(
            &request.request_id,
            serde_json::json!({ "connected": true }),
        ),
        "send_text" => match decode_payload::<SendTextInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| {
                    start_provider_process_with_dispatcher(
                        runtime,
                        Arc::clone(&persistent_dispatcher),
                        input,
                    )
                }) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "prepare_thread" => match decode_payload::<PrepareThreadInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| {
                    prepare_provider_process_with_dispatcher(
                        runtime,
                        Arc::clone(&persistent_dispatcher),
                        input,
                    )
                }) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "create_asset_upload" => match decode_payload::<CreateAssetUploadInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| runtime.lock().unwrap().create_asset_upload(input))
            {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "create_asset_download" => match decode_payload::<CreateAssetDownloadInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| runtime.lock().unwrap().create_asset_download(input))
            {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "list_assets" => match decode_payload::<ListAssetsInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| runtime.lock().unwrap().list_assets(input))
            {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "end_thread" => match decode_payload::<EndThreadInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| {
                    end_thread_with_dispatcher(runtime, Arc::clone(&persistent_dispatcher), input)
                }) {
                Ok(()) => ok_response(&request.request_id, serde_json::json!({})),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "submit_tool_result" => match decode_payload::<SubmitToolResultInput>(&request) {
            Ok(input) => match authorize_thread_request(&runtime, &request, &input.thread_id)
                .and_then(|_| runtime.lock().unwrap().submit_tool_result(input))
            {
                Ok(()) => ok_response(&request.request_id, serde_json::json!({})),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "tool_call" => handle_tool_call_request(&request, runtime),
        "tool_spec" => match decode_payload::<ToolSpecInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().tool_spec(input) {
                Ok(spec) => ok_response(&request.request_id, serde_json::json!(spec)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        _ => error_response(
            &request.request_id,
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "unknown Core IPC request type",
                serde_json::json!({ "type": request.r#type }),
            ),
        ),
    }
}

fn handle_pick_workspace_folder_request(
    request: &CoreIpcRequest,
    platform_services: Arc<dyn CoreIpcPlatformServices>,
) -> CoreIpcResponse {
    if request
        .caller_origin
        .as_deref()
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .is_none()
    {
        return error_response(
            &request.request_id,
            PedelecError::new(
                error_codes::IPC_UNAUTHORIZED,
                "directory picker requires an approved caller origin",
            ),
        );
    }

    match platform_services.pick_directory() {
        Ok(None) => ok_response(&request.request_id, serde_json::json!({ "path": null })),
        Ok(Some(path)) => {
            let inspection = match inspect_workspace_folder(&path) {
                Ok(inspection) => inspection,
                Err(err) => return error_response(&request.request_id, err),
            };
            match path.into_os_string().into_string() {
                Ok(path) => ok_response(
                    &request.request_id,
                    serde_json::json!({
                        "path": path,
                        "isEmptyFolder": inspection.is_empty_folder,
                        "hasWorkspaceConfig": inspection.has_workspace_config,
                    }),
                ),
                Err(_) => error_response(
                    &request.request_id,
                    PedelecError::new(
                        error_codes::DIRECTORY_PICKER_FAILED,
                        "selected directory path could not be represented as a string",
                    ),
                ),
            }
        }
        Err(err) => error_response(&request.request_id, err),
    }
}

fn handle_subscribe_thread(
    request: &CoreIpcRequest,
    runtime: &SharedCoreRuntime,
    writer: Arc<Mutex<TcpStream>>,
) -> CoreIpcResponse {
    let input = match decode_payload::<SubscribeThreadInput>(request) {
        Ok(input) => input,
        Err(err) => return error_response(&request.request_id, err),
    };

    if let Err(err) = authorize_thread_request(runtime, request, &input.thread_id) {
        return error_response(&request.request_id, err);
    }
    let event_rx = match runtime.lock().unwrap().subscribe_thread(input) {
        Ok(event_rx) => event_rx,
        Err(err) => return error_response(&request.request_id, err),
    };

    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            let message = CoreIpcEventMessage {
                r#type: "thread_event".to_string(),
                event,
            };
            let Ok(mut writer) = writer.lock() else {
                break;
            };
            if write_json_line(&mut *writer, &message).is_err() {
                break;
            }
        }
    });

    ok_response(
        &request.request_id,
        serde_json::json!({ "subscribed": true }),
    )
}

fn authorize_thread_request(
    runtime: &SharedCoreRuntime,
    request: &CoreIpcRequest,
    thread_id: &str,
) -> Result<(), PedelecError> {
    runtime
        .lock()
        .unwrap()
        .authorize_thread_access(thread_id, request.caller_origin.as_deref())
}

fn handle_tool_call_request(
    request: &CoreIpcRequest,
    runtime: SharedCoreRuntime,
) -> CoreIpcResponse {
    handle_tool_call_request_with_metadata(request, runtime).response
}

fn handle_tool_call_request_with_metadata(
    request: &CoreIpcRequest,
    runtime: SharedCoreRuntime,
) -> HandledCoreIpcResponse {
    let input = match decode_payload::<ToolCallInput>(request) {
        Ok(input) => input,
        Err(err) => {
            return HandledCoreIpcResponse {
                response: error_response(&request.request_id, err),
                tool_delivery_request_id: None,
            }
        }
    };

    let registration = match runtime.lock().unwrap().begin_tool_call(input) {
        Ok(registration) => registration,
        Err(err) => {
            return HandledCoreIpcResponse {
                response: error_response(&request.request_id, err),
                tool_delivery_request_id: None,
            }
        }
    };
    let tool_delivery_request_id = match &registration {
        ToolInvocationRegistration::Created(wait)
        | ToolInvocationRegistration::Joined(wait)
        | ToolInvocationRegistration::Replayed(wait) => Some(wait.request_id.clone()),
    };
    let wait = match registration {
        ToolInvocationRegistration::Created(wait)
        | ToolInvocationRegistration::Joined(wait)
        | ToolInvocationRegistration::Replayed(wait) => wait,
    };
    let ToolInvocationWait {
        request_id,
        remaining_timeout,
        result_rx,
        ..
    } = wait;

    let response = match result_rx.recv_timeout(remaining_timeout) {
        Ok(ToolInvocationOutcome::Result(result)) => ok_response(&request.request_id, result),
        Ok(ToolInvocationOutcome::CoreError(error)) => error_response(&request.request_id, error),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            runtime.lock().unwrap().timeout_tool_call(&request_id);
            error_response(
                &request.request_id,
                PedelecError::new(error_codes::TOOL_TIMEOUT, "tool timeout"),
            )
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            runtime.lock().unwrap().timeout_tool_call(&request_id);
            error_response(
                &request.request_id,
                PedelecError::new(error_codes::TOOL_TIMEOUT, "tool timeout"),
            )
        }
    };
    HandledCoreIpcResponse {
        response,
        tool_delivery_request_id,
    }
}

fn decode_payload<T>(request: &CoreIpcRequest) -> Result<T, PedelecError>
where
    T: for<'de> Deserialize<'de>,
{
    let payload = request.payload.clone().unwrap_or(Value::Null);
    serde_json::from_value(payload).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC request payload had invalid shape",
            serde_json::json!({
                "type": request.r#type,
                "error": err.to_string()
            }),
        )
    })
}

pub fn start_provider_process(
    runtime: SharedCoreRuntime,
    input: SendTextInput,
) -> Result<pedelec_core::SendTextOutput, PedelecError> {
    start_provider_process_with_dispatcher(
        runtime,
        Arc::new(RejectPersistentRuntimeDispatcher),
        input,
    )
}

pub fn start_provider_process_with_dispatcher(
    runtime: SharedCoreRuntime,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
    input: SendTextInput,
) -> Result<pedelec_core::SendTextOutput, PedelecError> {
    wait_for_provider_readiness(&runtime)?;
    let thread_id = input.thread_id.clone();
    let start = runtime.lock().unwrap().begin_send_text_intent(input)?;
    dispatch_provider_execution(runtime, persistent_dispatcher, thread_id, start.intent)?;
    Ok(start.output)
}

pub fn start_debug_provider_process(
    runtime: SharedCoreRuntime,
    input: SendTextInput,
) -> Result<pedelec_core::SendTextOutput, PedelecError> {
    start_debug_provider_process_with_dispatcher(
        runtime,
        Arc::new(RejectPersistentRuntimeDispatcher),
        input,
    )
}

pub fn start_debug_provider_process_with_dispatcher(
    runtime: SharedCoreRuntime,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
    input: SendTextInput,
) -> Result<pedelec_core::SendTextOutput, PedelecError> {
    wait_for_provider_readiness(&runtime)?;
    let thread_id = input.thread_id.clone();
    let start = runtime
        .lock()
        .unwrap()
        .begin_debug_send_text_intent(input)?;
    dispatch_provider_execution(runtime, persistent_dispatcher, thread_id, start.intent)?;
    Ok(start.output)
}

pub fn prepare_provider_process(
    runtime: SharedCoreRuntime,
    input: PrepareThreadInput,
) -> Result<PrepareThreadOutput, PedelecError> {
    prepare_provider_process_with_dispatcher(
        runtime,
        Arc::new(RejectPersistentRuntimeDispatcher),
        input,
    )
}

pub fn prepare_provider_process_with_dispatcher(
    runtime: SharedCoreRuntime,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
    input: PrepareThreadInput,
) -> Result<PrepareThreadOutput, PedelecError> {
    wait_for_provider_readiness(&runtime)?;
    let thread_id = input.thread_id.clone();
    let start = runtime.lock().unwrap().begin_prepare_thread_intent(input)?;
    let Some(intent) = start.intent else {
        return Ok(start.output);
    };
    dispatch_provider_execution(runtime, persistent_dispatcher, thread_id, intent)?;
    Ok(start.output)
}

pub fn end_thread_with_dispatcher(
    runtime: SharedCoreRuntime,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
    input: EndThreadInput,
) -> Result<(), PedelecError> {
    let start = runtime.lock().unwrap().begin_end_thread(input)?;
    let thread_id = start.thread_id.clone();
    match start.execution {
        EndThreadExecutionIntent::LegacyProcess(stop) => {
            if let Some(stop) = stop {
                stop.stop();
            }
            runtime.lock().unwrap().finish_end_thread(&thread_id)
        }
        EndThreadExecutionIntent::PersistentRuntime(operation) => {
            let dispatch_result = persistent_dispatcher.dispatch(operation);
            if let Err(error) = dispatch_result {
                // End is a safety operation: a failed unsubscribe must not
                // leave the Pedelec thread permanently in Stopping.
                runtime.lock().unwrap().finish_end_thread(&thread_id)?;
                return Err(error);
            }
            runtime.lock().unwrap().finish_end_thread(&thread_id)
        }
    }
}

fn dispatch_provider_execution(
    runtime: SharedCoreRuntime,
    persistent_dispatcher: Arc<dyn PersistentRuntimeDispatcher>,
    thread_id: String,
    intent: ProviderExecutionIntent,
) -> Result<(), PedelecError> {
    let operation = intent.operation_kind();
    match intent {
        ProviderExecutionIntent::LegacyCommand { command, purpose } => {
            let result = start_provider_process_with_command(
                Arc::clone(&runtime),
                thread_id.clone(),
                command,
                purpose,
            );
            if result.is_ok() {
                runtime
                    .lock()
                    .unwrap()
                    .complete_provider_execution_dispatch(&thread_id);
            }
            result
        }
        ProviderExecutionIntent::PersistentRuntime {
            operation: runtime_operation,
        } => {
            if let Err(error) = persistent_dispatcher.dispatch(runtime_operation) {
                runtime.lock().unwrap().fail_provider_execution_dispatch(
                    &thread_id,
                    operation,
                    error.clone(),
                );
                return Err(error);
            }
            runtime
                .lock()
                .unwrap()
                .complete_provider_execution_dispatch(&thread_id);
            Ok(())
        }
    }
}

fn start_provider_process_with_command(
    runtime: SharedCoreRuntime,
    thread_id: String,
    command_spec: pedelec_core::CommandSpec,
    purpose: RunningProviderProcessPurpose,
) -> Result<(), PedelecError> {
    let resolved_program = match resolve_provider_program(&command_spec.program, &command_spec.env)
    {
        Ok(resolved_program) => resolved_program,
        Err(err) => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROCESS_START_FAILED,
                "provider program could not be found",
                provider_start_error_details(&thread_id, &command_spec, None, Some(err)),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    };

    let mut command = build_provider_process_command(&command_spec, &resolved_program);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROCESS_START_FAILED,
                "provider process could not be started",
                provider_start_error_details(
                    &thread_id,
                    &command_spec,
                    Some(&resolved_program),
                    Some(ProviderProgramResolveError {
                        candidates: Vec::new(),
                        error: err.to_string(),
                    }),
                ),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    };

    let process_id = child.id();
    runtime
        .lock()
        .unwrap()
        .emit_provider_command_started(&thread_id, process_id, &command_spec);

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(command_spec.stdin.as_bytes()) {
            let _ = child.kill();
            let error = PedelecError::with_details(
                error_codes::PROVIDER_STDIN_CLOSED,
                "provider stdin closed before prompt was written",
                serde_json::json!({
                    "threadId": thread_id,
                    "processId": process_id,
                    "error": err.to_string()
                }),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    } else {
        let _ = child.kill();
        let error = PedelecError::with_details(
            error_codes::PROVIDER_STDIN_CLOSED,
            "provider stdin was not available",
            serde_json::json!({
                "threadId": thread_id,
                "processId": process_id
            }),
        );
        runtime
            .lock()
            .unwrap()
            .fail_provider_process_start(&thread_id, error.clone(), purpose);
        return Err(error);
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(Some(child)));
    let termination = runtime.lock().unwrap().register_provider_process(
        &thread_id,
        process_id,
        Arc::clone(&child),
        purpose,
    );

    let stdout_reader = stdout.map(|stdout| {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stdout,
            ProviderStream::Stdout,
            Arc::clone(&termination),
        )
    });
    let stderr_reader = stderr.map(|stderr| {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stderr,
            ProviderStream::Stderr,
            Arc::clone(&termination),
        )
    });
    spawn_provider_waiter(
        runtime,
        thread_id,
        process_id,
        child,
        termination,
        stdout_reader,
        stderr_reader,
    );

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedProviderProgram {
    Direct(PathBuf),
    #[cfg(windows)]
    CmdScript(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderProgramResolveError {
    candidates: Vec<PathBuf>,
    error: String,
}

fn resolve_provider_program(
    program: &str,
    command_env: &[(String, String)],
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    let program_path = Path::new(program);
    if has_path_separator(program) {
        return resolve_provider_program_path(program_path);
    }

    let Some(path_value) = command_env_path(command_env).or_else(|| env::var_os("PATH")) else {
        return Err(ProviderProgramResolveError {
            candidates: Vec::new(),
            error: "PATH was not available".to_string(),
        });
    };

    let path_dirs = env::split_paths(&path_value).collect::<Vec<_>>();
    let candidates = provider_program_lookup_candidates(program, &path_dirs);
    for candidate in &candidates {
        if candidate.is_file() {
            return resolved_provider_program_for_existing_path(candidate);
        }
    }

    Err(ProviderProgramResolveError {
        candidates,
        error: "program was not found in PATH".to_string(),
    })
}

fn resolve_provider_program_path(
    program_path: &Path,
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    let candidates = provider_program_path_candidates(program_path);
    for candidate in &candidates {
        if candidate.is_file() {
            return resolved_provider_program_for_existing_path(candidate);
        }
    }

    Err(ProviderProgramResolveError {
        candidates,
        error: "program path was not found".to_string(),
    })
}

fn resolved_provider_program_for_existing_path(
    path: &Path,
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "cmd" | "bat") {
            return Ok(ResolvedProviderProgram::CmdScript(path.to_path_buf()));
        }
    }

    Ok(ResolvedProviderProgram::Direct(path.to_path_buf()))
}

fn provider_program_lookup_candidates(program: &str, path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let program_path = Path::new(program);
        if program_path.extension().is_some() {
            return path_dirs.iter().map(|dir| dir.join(program)).collect();
        }

        return ["exe", "cmd", "bat"]
            .iter()
            .flat_map(|extension| {
                path_dirs
                    .iter()
                    .map(move |dir| dir.join(format!("{program}.{extension}")))
            })
            .collect();
    }

    #[cfg(not(windows))]
    {
        path_dirs.iter().map(|dir| dir.join(program)).collect()
    }
}

fn provider_program_path_candidates(program_path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if program_path.extension().is_some() {
            return vec![program_path.to_path_buf()];
        }

        return ["exe", "cmd", "bat"]
            .iter()
            .map(|extension| program_path.with_extension(extension))
            .collect();
    }

    #[cfg(not(windows))]
    {
        vec![program_path.to_path_buf()]
    }
}

fn build_provider_process_command(
    spec: &pedelec_core::CommandSpec,
    resolved_program: &ResolvedProviderProgram,
) -> Command {
    let mut command = match resolved_program {
        ResolvedProviderProgram::Direct(program) => {
            let mut command = Command::new(program);
            command.args(&spec.args);
            command
        }
        #[cfg(windows)]
        ResolvedProviderProgram::CmdScript(program) => {
            let mut command = Command::new("cmd.exe");
            command.arg("/d").arg("/c").arg("call").arg(program);
            command.args(&spec.args);
            command
        }
    };

    command
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Captured output from a one-shot provider invocation.
///
/// This runner is intentionally separate from the normal SDK thread process
/// lifecycle. It is used by desktop-only capability probes and never creates
/// a Core thread or emits ThreadEvents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedProviderProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

const MAX_CAPTURED_PROVIDER_OUTPUT_BYTES: usize = 1024 * 1024;
const CAPTURE_READ_BUFFER_BYTES: usize = 16 * 1024;

/// Runs one provider command with bounded output capture and a hard timeout.
///
/// The exact executable path supplied by a provider scan is passed through the
/// existing resolver. In particular, Windows `.cmd`/`.bat` scripts continue to
/// use the existing `cmd.exe /d /c call` wrapper.
pub fn run_provider_command_captured(
    command_spec: pedelec_core::CommandSpec,
    timeout: Duration,
) -> Result<CapturedProviderProcessOutput, PedelecError> {
    run_provider_command_captured_with_cancel(
        command_spec,
        timeout,
        Arc::new(AtomicBool::new(false)),
    )
}

/// Cancellable form used by the desktop wizard when a run is reset or the app
/// is shutting down.
pub fn run_provider_command_captured_with_cancel(
    command_spec: pedelec_core::CommandSpec,
    timeout: Duration,
    cancellation: Arc<AtomicBool>,
) -> Result<CapturedProviderProcessOutput, PedelecError> {
    let resolved_program = resolve_provider_program(&command_spec.program, &command_spec.env)
        .map_err(|error| {
            PedelecError::with_details(
                error_codes::PROVIDER_PROCESS_START_FAILED,
                "provider program could not be found",
                serde_json::json!({
                    "program": command_spec.program,
                    "cwd": path_for_external_use(&command_spec.cwd),
                    "error": error.error,
                    "programLookupCandidates": error.candidates.iter()
                        .map(|candidate| candidate.to_string_lossy().to_string())
                        .collect::<Vec<_>>(),
                }),
            )
        })?;

    let mut command = build_provider_process_command(&command_spec, &resolved_program);
    let mut child = command.spawn().map_err(|error| {
        PedelecError::with_details(
            error_codes::PROVIDER_PROCESS_START_FAILED,
            "provider process could not be started",
            serde_json::json!({
                "program": command_spec.program,
                "cwd": path_for_external_use(&command_spec.cwd),
                "error": error.to_string(),
            }),
        )
    })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        PedelecError::new(
            error_codes::PROVIDER_STDIN_CLOSED,
            "provider stdin was not available",
        )
    })?;
    if let Err(error) = stdin.write_all(command_spec.stdin.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(PedelecError::with_details(
            error_codes::PROVIDER_STDIN_CLOSED,
            "provider stdin closed before probe prompt was written",
            serde_json::json!({ "error": error.to_string() }),
        ));
    }
    drop(stdin);

    let stdout = child.stdout.take().ok_or_else(|| {
        PedelecError::new(
            error_codes::PROVIDER_PROCESS_START_FAILED,
            "provider stdout was not available",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        PedelecError::new(
            error_codes::PROVIDER_PROCESS_START_FAILED,
            "provider stderr was not available",
        )
    })?;
    let stdout_reader = thread::spawn(move || capture_provider_output(stdout));
    let stderr_reader = thread::spawn(move || capture_provider_output(stderr));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancellation.load(Ordering::Acquire) => {
                cancelled = true;
                let _ = child.kill();
                break child.wait().map_err(|error| {
                    PedelecError::with_details(
                        error_codes::PROVIDER_PROCESS_STOP_FAILED,
                        "cancelled provider process could not be reaped",
                        serde_json::json!({ "error": error.to_string() }),
                    )
                })?;
            }
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().map_err(|error| {
                    PedelecError::with_details(
                        error_codes::PROVIDER_PROCESS_STOP_FAILED,
                        "timed-out provider process could not be reaped",
                        serde_json::json!({ "error": error.to_string() }),
                    )
                })?;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PedelecError::with_details(
                    error_codes::PROVIDER_PROCESS_STOP_FAILED,
                    "provider process status could not be read",
                    serde_json::json!({ "error": error.to_string() }),
                ));
            }
        }
    };

    let stdout = stdout_reader.join().map_err(|_| {
        PedelecError::new(
            error_codes::PROVIDER_PROCESS_STOP_FAILED,
            "provider stdout capture thread panicked",
        )
    })?;
    let stderr = stderr_reader.join().map_err(|_| {
        PedelecError::new(
            error_codes::PROVIDER_PROCESS_STOP_FAILED,
            "provider stderr capture thread panicked",
        )
    })?;

    Ok(CapturedProviderProcessOutput {
        exit_code: exit_status.code(),
        stdout: stdout.text,
        stderr: stderr.text,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        timed_out,
        cancelled,
    })
}

#[derive(Debug)]
struct CapturedProviderStream {
    text: String,
    truncated: bool,
}

fn capture_provider_output<R: Read>(mut reader: R) -> CapturedProviderStream {
    let mut output = Vec::with_capacity(MAX_CAPTURED_PROVIDER_OUTPUT_BYTES.min(64 * 1024));
    let mut buffer = [0u8; CAPTURE_READ_BUFFER_BYTES];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if output.len() < MAX_CAPTURED_PROVIDER_OUTPUT_BYTES {
                    let remaining = MAX_CAPTURED_PROVIDER_OUTPUT_BYTES - output.len();
                    output.extend_from_slice(&buffer[..read.min(remaining)]);
                    if read > remaining {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    CapturedProviderStream {
        text: bounded_capture_text(&output),
        truncated,
    }
}

fn bounded_capture_text(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() > MAX_CAPTURED_PROVIDER_OUTPUT_BYTES {
        let mut end = MAX_CAPTURED_PROVIDER_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn command_env_path(command_env: &[(String, String)]) -> Option<OsString> {
    command_env
        .iter()
        .rev()
        .find(|(key, _)| env_key_is_path(key))
        .map(|(_, value)| OsString::from(value))
}

#[cfg(windows)]
fn env_key_is_path(key: &str) -> bool {
    key.eq_ignore_ascii_case("PATH")
}

#[cfg(not(windows))]
fn env_key_is_path(key: &str) -> bool {
    key == "PATH"
}

fn has_path_separator(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

fn provider_start_error_details(
    thread_id: &str,
    spec: &pedelec_core::CommandSpec,
    resolved_program: Option<&ResolvedProviderProgram>,
    resolve_error: Option<ProviderProgramResolveError>,
) -> Value {
    let mut details = serde_json::json!({
        "threadId": thread_id,
        "program": spec.program,
        "args": spec.args,
        "cwd": path_for_external_use(&spec.cwd),
        "path": command_env_path(&spec.env)
            .or_else(|| env::var_os("PATH"))
            .map(|path| path.to_string_lossy().to_string())
    });
    if let Some(resolved_program) = resolved_program {
        details["resolvedProgram"] = match resolved_program {
            ResolvedProviderProgram::Direct(program) => serde_json::json!({
                "type": "direct",
                "path": program.to_string_lossy()
            }),
            #[cfg(windows)]
            ResolvedProviderProgram::CmdScript(program) => serde_json::json!({
                "type": "cmdScript",
                "path": program.to_string_lossy()
            }),
        };
    }
    if let Some(resolve_error) = resolve_error {
        details["error"] = serde_json::json!(resolve_error.error);
        if !resolve_error.candidates.is_empty() {
            details["programLookupCandidates"] = serde_json::json!(resolve_error
                .candidates
                .iter()
                .map(|candidate| candidate.to_string_lossy().to_string())
                .collect::<Vec<_>>());
        }
    }
    details
}

#[derive(Debug, Clone, Copy)]
enum ProviderStream {
    Stdout,
    Stderr,
}

fn spawn_provider_reader<R>(
    runtime: SharedCoreRuntime,
    thread_id: String,
    mut reader: R,
    stream: ProviderStream,
    termination: Arc<ProviderProcessTermination>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        let mut decoder = ProviderOutputDecoder::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes_read) => {
                    let Some(text) = decoder.decode_chunk(&buffer[..bytes_read]) else {
                        continue;
                    };
                    emit_provider_reader_text(&runtime, &thread_id, stream, text, &termination);
                }
                Err(_) => break,
            }
        }
        if let Some(text) = decoder.flush() {
            emit_provider_reader_text(&runtime, &thread_id, stream, text, &termination);
        }
    })
}

fn emit_provider_reader_text(
    runtime: &SharedCoreRuntime,
    thread_id: &str,
    stream: ProviderStream,
    text: String,
    termination: &ProviderProcessTermination,
) {
    let mut text = Some(text);
    loop {
        if termination.is_cancelled() {
            return;
        }
        match runtime.try_lock() {
            Ok(mut runtime) => {
                if termination.is_cancelled() {
                    return;
                }
                let text = text.take().expect("provider reader text is present");
                match stream {
                    ProviderStream::Stdout => runtime.emit_provider_stdout(thread_id, text),
                    ProviderStream::Stderr => runtime.emit_provider_stderr(thread_id, text),
                }
                return;
            }
            Err(std::sync::TryLockError::WouldBlock) => thread::yield_now(),
            Err(std::sync::TryLockError::Poisoned(_)) => return,
        }
    }
}

struct ProviderOutputDecoder {
    pending: Vec<u8>,
    fallback_encoding: Option<&'static Encoding>,
}

impl ProviderOutputDecoder {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            fallback_encoding: provider_output_fallback_encoding(),
        }
    }

    fn decode_chunk(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        self.decode_pending(false)
    }

    fn flush(&mut self) -> Option<String> {
        self.decode_pending(true)
    }

    fn decode_pending(&mut self, flush: bool) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }

        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                Some(text)
            }
            Err(err) if err.error_len().is_none() && !flush => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to == 0 {
                    return None;
                }

                let suffix = self.pending.split_off(valid_up_to);
                let text = String::from_utf8(self.pending.split_off(0)).ok();
                self.pending = suffix;
                text
            }
            Err(_) => {
                let text = self.decode_with_fallback();
                self.pending.clear();
                Some(text)
            }
        }
    }

    fn decode_with_fallback(&self) -> String {
        if let Some(encoding) = self.fallback_encoding {
            let (text, _, _) = encoding.decode(&self.pending);
            return text.into_owned();
        }

        String::from_utf8_lossy(&self.pending).to_string()
    }
}

#[cfg(windows)]
fn provider_output_fallback_encoding() -> Option<&'static Encoding> {
    let code_page = unsafe { windows_sys::Win32::Globalization::GetACP() };
    provider_output_encoding_for_windows_code_page(code_page)
}

#[cfg(windows)]
fn provider_output_encoding_for_windows_code_page(code_page: u32) -> Option<&'static Encoding> {
    let label = match code_page {
        65001 => "utf-8",
        950 => "big5",
        936 => "gbk",
        932 => "shift_jis",
        949 => "euc-kr",
        874 => "windows-874",
        866 => "ibm866",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1252 => "windows-1252",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        _ => return None,
    };
    Encoding::for_label(label.as_bytes())
}

#[cfg(not(windows))]
fn provider_output_fallback_encoding() -> Option<&'static Encoding> {
    None
}

fn spawn_provider_waiter(
    runtime: SharedCoreRuntime,
    thread_id: String,
    process_id: u32,
    child: Arc<Mutex<Option<std::process::Child>>>,
    termination: Arc<ProviderProcessTermination>,
    stdout_reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
) {
    thread::spawn(move || {
        let child = {
            let Ok(mut child) = child.lock() else {
                termination.mark_completed();
                return;
            };
            child.take()
        };

        let Some(mut child) = child else {
            termination.mark_completed();
            return;
        };

        let wait_result = child.wait();
        if let Some(reader) = stdout_reader {
            let _ = reader.join();
        }
        if let Some(reader) = stderr_reader {
            let _ = reader.join();
        }
        termination.mark_completed();
        match wait_result {
            Ok(status) => {
                if !termination.is_cancelled() {
                    if let Ok(mut runtime) = runtime.lock() {
                        runtime.complete_provider_process(&thread_id, process_id, status);
                    }
                }
            }
            Err(err) => {
                if !termination.is_cancelled() {
                    if let Ok(mut runtime) = runtime.lock() {
                        runtime.fail_provider_process_wait(&thread_id, process_id, err.to_string());
                    }
                }
            }
        }
    });
}

fn ok_response(request_id: &str, result: Value) -> CoreIpcResponse {
    CoreIpcResponse {
        request_id: request_id.to_string(),
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn error_response(request_id: &str, error: PedelecError) -> CoreIpcResponse {
    CoreIpcResponse {
        request_id: request_id.to_string(),
        ok: false,
        result: None,
        error: Some(error),
    }
}

fn write_runtime_file(path: &Path, runtime_file: &RuntimeFile) -> Result<(), PedelecError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot create runtime.json directory",
                serde_json::json!({ "path": parent.to_string_lossy(), "error": err.to_string() }),
            )
        })?;
    }

    let payload = serde_json::to_string_pretty(runtime_file).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot serialize runtime.json",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(path, payload).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot write runtime.json",
            serde_json::json!({ "path": path.to_string_lossy(), "error": err.to_string() }),
        )
    })
}

fn read_runtime_file(path: impl AsRef<Path>) -> Result<RuntimeFile, PedelecError> {
    let path = path.as_ref();
    let payload = fs::read_to_string(path).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })?;
    serde_json::from_str(&payload).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })
}

fn core_unavailable_error(_err: io::Error) -> PedelecError {
    PedelecError::new(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        "pedelec-app is not running",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Barrier};
    use std::time::Duration;

    #[test]
    fn codex_runtime_stderr_diagnostic_preserves_runtime_identity() {
        let diagnostic = codex_runtime_stderr_diagnostic(42, 4242, "codex stderr".to_string());
        assert!(matches!(
            &diagnostic,
            ProviderRuntimeDiagnostic::ProviderRuntimeStderr {
                provider: pedelec_core::ProviderCode::Codex,
                runtime_generation: 42,
                process_id: 4242,
                text,
            } if text == "codex stderr"
        ));
        assert_eq!(
            serde_json::to_value(diagnostic).unwrap()["type"],
            "provider_runtime_stderr"
        );
    }

    #[test]
    fn provider_start_diagnostics_externalize_cwd_without_changing_the_spec() {
        let cwd = if cfg!(windows) {
            PathBuf::from(r"\\?\C:\Users\kaoru\OneDrive\桌面\test")
        } else {
            PathBuf::from("/tmp/pedelec-workspace")
        };
        let spec = pedelec_core::CommandSpec {
            program: "codex".into(),
            args: vec!["--cd".into(), "external path placeholder".into()],
            cwd: cwd.clone(),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let details = provider_start_error_details("thread", &spec, None, None);

        assert_eq!(details["cwd"], path_for_external_use(&cwd));
        assert_eq!(spec.cwd, cwd);
    }

    #[test]
    fn list_providers_waits_for_initial_provider_scan_without_holding_runtime_lock() {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        runtime.lock().unwrap().provider_path_value_override = Some(OsString::new());

        // Prevent the request from entering the runtime until it has started.
        // Once released, the request must wait on the readiness primitive
        // rather than hold this mutex during the wait.
        let startup_guard = runtime.lock().unwrap();
        let request_runtime = Arc::clone(&runtime);
        let (response_tx, response_rx) = mpsc::channel();
        let barrier = Arc::new(Barrier::new(2));
        let request_barrier = barrier.clone();
        let request_thread = thread::spawn(move || {
            request_barrier.wait();
            let response = handle_core_ipc_request(
                CoreIpcRequest {
                    request_id: "providers_wait".into(),
                    r#type: "list_providers".into(),
                    caller_origin: None,
                    caller_sdk_version: None,
                    payload: Some(serde_json::json!({})),
                },
                request_runtime,
            );
            response_tx.send(response).unwrap();
        });

        barrier.wait();
        assert!(response_rx.try_recv().is_err());
        drop(startup_guard);

        let refresh_runtime = Arc::clone(&runtime);
        let (refresh_tx, refresh_rx) = mpsc::channel();
        thread::spawn(move || {
            refresh_runtime.lock().unwrap().refresh_providers();
            refresh_tx.send(()).unwrap();
        });

        assert!(refresh_rx.recv_timeout(Duration::from_secs(2)).is_ok());
        let response = response_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        request_thread.join().unwrap();
        assert!(response.ok);
        let providers = response.result.unwrap().as_array().unwrap().clone();
        assert!(
            providers
                .iter()
                .filter(|provider| provider.get("code") != Some(&serde_json::json!("ollama")))
                .all(|provider| {
                    provider.get("error")
                        != Some(&serde_json::json!("provider scan has not completed"))
                }),
            "providers were returned before the completed scan was installed: {providers:?}"
        );
    }

    #[test]
    fn ping_is_not_blocked_by_initial_provider_scan() {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));

        let response = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "ping_during_scan".into(),
                r#type: "ping".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: None,
            },
            runtime,
        );

        assert!(response.ok);
        assert_eq!(
            response.result,
            Some(serde_json::json!({ "connected": true }))
        );
    }

    #[test]
    fn send_text_waits_for_initial_provider_readiness_before_starting_process() {
        let runtime = waiting_provider_runtime("send_text_wait");
        let request_runtime = Arc::clone(&runtime);
        let (response_tx, response_rx) = mpsc::channel();
        let request_thread = thread::spawn(move || {
            response_tx
                .send(start_provider_process(
                    request_runtime,
                    SendTextInput {
                        thread_id: "send_text_wait".into(),
                        message: "hello".into(),
                    },
                ))
                .unwrap();
        });

        assert!(response_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("send_text_wait")
                .unwrap()
                .status,
            pedelec_core::ThreadStatus::Idle
        );

        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_ready_for_test();

        let error = response_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        request_thread.join().unwrap();
        assert_eq!(error.code, error_codes::PROVIDER_PROCESS_START_FAILED);
    }

    #[test]
    fn prepare_thread_uses_the_same_provider_readiness_gate() {
        let runtime = waiting_provider_runtime("prepare_thread_wait");
        let request_runtime = Arc::clone(&runtime);
        let (response_tx, response_rx) = mpsc::channel();
        let request_thread = thread::spawn(move || {
            response_tx
                .send(prepare_provider_process(
                    request_runtime,
                    PrepareThreadInput {
                        thread_id: "prepare_thread_wait".into(),
                    },
                ))
                .unwrap();
        });

        assert!(response_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());

        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_ready_for_test();

        let error = response_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        request_thread.join().unwrap();
        assert_eq!(error.code, error_codes::PROVIDER_PROCESS_START_FAILED);
    }

    #[cfg(unix)]
    #[test]
    fn captured_runner_reuses_exact_program_cwd_env_and_stdin() {
        let temp = tempfile::tempdir().unwrap();
        let spec = pedelec_core::CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s|%s|' \"$PEDELEC_RUNNER_TEST\" \"$(pwd)\"; cat".into(),
            ],
            cwd: temp.path().to_path_buf(),
            env: vec![("PEDELEC_RUNNER_TEST".into(), "env-value".into())],
            prompt: String::new(),
            stdin: "stdin-value\n".into(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(2)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(
            output.stdout,
            format!("env-value|{}|stdin-value\n", temp.path().display())
        );
        assert!(output.stderr.is_empty());
        assert!(!output.timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn captured_runner_returns_non_zero_output_for_classification() {
        let spec = pedelec_core::CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf 'stdout'; printf 'stderr' >&2; exit 7".into(),
            ],
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(2)).unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stdout, "stdout");
        assert_eq!(output.stderr, "stderr");
    }

    #[cfg(unix)]
    #[test]
    fn captured_runner_bounds_stdout_and_stderr() {
        let spec = pedelec_core::CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "dd if=/dev/zero bs=1024 count=2048 2>/dev/null; dd if=/dev/zero bs=1024 count=2048 1>&2 2>/dev/null".into(),
            ],
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(2)).unwrap();
        assert_eq!(output.stdout.len(), MAX_CAPTURED_PROVIDER_OUTPUT_BYTES);
        assert_eq!(output.stderr.len(), MAX_CAPTURED_PROVIDER_OUTPUT_BYTES);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[cfg(unix)]
    #[test]
    fn captured_runner_timeout_kills_and_reaps_child() {
        let spec = pedelec_core::CommandSpec {
            program: "/bin/sleep".into(),
            args: vec!["10".into()],
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let started = std::time::Instant::now();
        let output = run_provider_command_captured(spec, Duration::from_millis(50)).unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[test]
    fn captured_runner_reuses_cmd_script_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("probe.cmd");
        fs::write(&script, "@echo off\r\necho wrapper-ok\r\n").unwrap();
        let spec = pedelec_core::CommandSpec {
            program: script.to_string_lossy().into_owned(),
            args: Vec::new(),
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(2)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("wrapper-ok"));
    }

    #[cfg(windows)]
    #[test]
    fn captured_runner_captures_windows_stdin_and_environment() {
        let comspec = std::env::var_os("ComSpec").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let spec = pedelec_core::CommandSpec {
            program: comspec.to_string_lossy().into_owned(),
            args: vec!["/d".into(), "/c".into(), "more".into()],
            cwd: temp.path().to_path_buf(),
            env: vec![("PEDELEC_RUNNER_TEST".into(), "env-value".into())],
            prompt: String::new(),
            stdin: "stdin-value\r\n".into(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(2)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("stdin-value"));
    }

    #[cfg(windows)]
    #[test]
    fn captured_runner_timeout_kills_windows_child() {
        let powershell_exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if !powershell_exe.is_file() {
            return;
        }
        let spec = pedelec_core::CommandSpec {
            program: powershell_exe.to_string_lossy().into_owned(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 10".into(),
            ],
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let output = run_provider_command_captured(spec, Duration::from_millis(50)).unwrap();
        assert!(output.timed_out);
    }

    #[cfg(windows)]
    #[test]
    fn captured_runner_bounds_windows_output() {
        let powershell_exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if !powershell_exe.is_file() {
            return;
        }
        let spec = pedelec_core::CommandSpec {
            program: powershell_exe.to_string_lossy().into_owned(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "$s='x'*2097152; [Console]::Out.Write($s); [Console]::Error.Write($s)".into(),
            ],
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let output = run_provider_command_captured(spec, Duration::from_secs(5)).unwrap();
        assert_eq!(output.stdout.len(), MAX_CAPTURED_PROVIDER_OUTPUT_BYTES);
        assert_eq!(output.stderr.len(), MAX_CAPTURED_PROVIDER_OUTPUT_BYTES);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[test]
    fn captured_runner_reports_spawn_failure() {
        let spec = pedelec_core::CommandSpec {
            program: "pedelec-runner-program-that-does-not-exist".into(),
            args: Vec::new(),
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        };

        let error = run_provider_command_captured(spec, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_PROCESS_START_FAILED);
    }

    fn waiting_provider_runtime(thread_id: &str) -> SharedCoreRuntime {
        let runtime = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        let now = chrono::Utc::now();
        let mut runtime_guard = runtime.lock().unwrap();
        runtime_guard
            .provider_readiness
            .mark_initial_scanning_for_test();
        runtime_guard.thread_manager.insert_thread(
            pedelec_core::ThreadState {
                thread_id: thread_id.into(),
                provider: pedelec_core::ProviderCode::Codex,
                effort_level: pedelec_core::EffortLevel::Default,
                effort_args: Vec::new(),
                workspace_path: PathBuf::from("."),
                skills: Vec::new(),
                status: pedelec_core::ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            pedelec_core::ProviderAdapterState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime_guard.test_provider_command = Some(pedelec_core::CommandSpec {
            program: "pedelec-provider-readiness-test-command-that-does-not-exist".into(),
            args: Vec::new(),
            cwd: PathBuf::from("."),
            env: Vec::new(),
            prompt: String::new(),
            stdin: String::new(),
        });
        drop(runtime_guard);
        runtime
    }
}

#[cfg(test)]
#[path = "../../../tauri/src/pedelec_ipc/tests/mod.rs"]
mod tauri_ipc_tests;
