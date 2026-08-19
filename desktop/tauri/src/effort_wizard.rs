use pedelec_core::{
    build_effort_wizard_probe_plan, error_codes, EffortLevel, EffortWizardApplyPatch,
    EffortWizardBootstrap, EffortWizardConfirmedProfiles, EffortWizardProbeDefinition,
    EffortWizardProbePlan, EffortWizardProviderApplyPatch, EffortWizardProviderRecommendation,
    EffortWizardTierDecision, EffortWizardTierDecisions, EffortsArgs, PedelecError,
    PedelecSettings, SharedCoreRuntime, WizardProviderCode,
};
use pedelec_ipc::{run_provider_command_captured_with_cancel, CapturedProviderProcessOutput};
use pedelec_shared::paths::path_for_external_use;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_PROMPT: &str = "Reply with OK.\n";
const PROBE_ROOT_NAME: &str = "pedelec-probe-runs";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortWizardRunStatus {
    Checking,
    ReviewReady,
    Applying,
    Completed,
    FatalError,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortWizardProviderStatus {
    SelectedPending,
    Checking,
    ReviewReady,
    ProbeError,
    NoRecommendation,
    Unavailable,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortWizardProbeOutcome {
    Supported,
    NotEntitled,
    TransientError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProbeResultState {
    pub probe_id: String,
    pub outcome: EffortWizardProbeOutcome,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderRunState {
    pub provider: WizardProviderCode,
    pub selected: bool,
    pub status: EffortWizardProviderStatus,
    pub current_efforts: EffortsArgs,
    pub recommendation: Option<EffortWizardProviderRecommendation>,
    pub probe_results: Vec<EffortWizardProbeResultState>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderReviewState {
    pub provider: WizardProviderCode,
    pub current_efforts: EffortsArgs,
    pub recommendation: Option<EffortWizardProviderRecommendation>,
    pub status: EffortWizardProviderStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardReviewState {
    pub providers: Vec<EffortWizardProviderReviewState>,
    pub reviewable_provider_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortWizardCompletionStatus {
    UpdatedOrConfirmed,
    KeptCurrent,
    NeedsAttention,
    NoRecommendation,
    Unavailable,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderCompletionState {
    pub provider: WizardProviderCode,
    pub status: EffortWizardCompletionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardCompletionState {
    pub providers: Vec<EffortWizardProviderCompletionState>,
    pub confirmed_provider_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardFatalError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardRunState {
    pub run_id: String,
    pub status: EffortWizardRunStatus,
    pub selected_providers: Vec<WizardProviderCode>,
    pub providers: Vec<EffortWizardProviderRunState>,
    pub review: Option<EffortWizardReviewState>,
    pub completion: Option<EffortWizardCompletionState>,
    pub error: Option<EffortWizardFatalError>,
    pub bootstrap: Option<EffortWizardBootstrap>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StartEffortWizardInput {
    pub providers: Vec<WizardProviderCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderDecision {
    pub provider: WizardProviderCode,
    pub tiers: EffortWizardTierDecisions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffortWizardInput {
    pub run_id: String,
    pub providers: Vec<EffortWizardProviderDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffortWizardOutput {
    pub state: EffortWizardRunState,
    pub bootstrap: EffortWizardBootstrap,
}

#[derive(Clone)]
pub struct EffortWizardOwner {
    inner: Arc<Mutex<EffortWizardCoordinator>>,
}

impl EffortWizardOwner {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EffortWizardCoordinator::default())),
        }
    }

    fn snapshot(&self) -> Option<EffortWizardRunState> {
        self.inner
            .lock()
            .unwrap()
            .current_run
            .as_ref()
            .map(|run| run.state.clone())
    }

    pub fn cancel_active(&self) {
        if let Some(run) = self.inner.lock().unwrap().current_run.as_ref() {
            if matches!(
                run.state.status,
                EffortWizardRunStatus::Checking | EffortWizardRunStatus::Applying
            ) {
                run.cancellation.store(true, Ordering::Release);
            }
        }
    }
}

impl Default for EffortWizardOwner {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct EffortWizardCoordinator {
    current_run: Option<EffortWizardRun>,
}

struct EffortWizardRun {
    state: EffortWizardRunState,
    snapshots: BTreeMap<WizardProviderCode, ProviderProbeSnapshot>,
    cancellation: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ProviderProbeSnapshot {
    plan: EffortWizardProbePlan,
    executable: PathBuf,
    workspace: PathBuf,
    current_efforts: EffortsArgs,
    cancellation: Arc<AtomicBool>,
}

pub trait EffortProbeExecutor: Send + Sync {
    fn execute(
        &self,
        command: pedelec_core::CommandSpec,
        timeout: Duration,
        cancellation: Arc<AtomicBool>,
    ) -> Result<CapturedProviderProcessOutput, PedelecError>;
}

trait EffortProbeDispatcher {
    fn dispatch(
        &self,
        provider: WizardProviderCode,
        snapshot: ProviderProbeSnapshot,
    ) -> Result<(), PedelecError>;
}

struct ThreadEffortProbeDispatcher {
    coordinator: Arc<Mutex<EffortWizardCoordinator>>,
    app: AppHandle,
    run_id: String,
}

impl EffortProbeDispatcher for ThreadEffortProbeDispatcher {
    fn dispatch(
        &self,
        provider: WizardProviderCode,
        snapshot: ProviderProbeSnapshot,
    ) -> Result<(), PedelecError> {
        let worker_coordinator = Arc::clone(&self.coordinator);
        let worker_app = self.app.clone();
        let worker_run_id = self.run_id.clone();
        thread::Builder::new()
            .name(format!("pedelec-effort-probe-{provider:?}"))
            .spawn(move || {
                run_provider_worker(
                    worker_coordinator,
                    worker_app,
                    worker_run_id,
                    provider,
                    snapshot,
                );
            })
            .map(|_| ())
            .map_err(|error| {
                wizard_fatal_error(
                    "an effort wizard probe worker could not be started",
                    serde_json::json!({
                        "provider": provider.as_str(),
                        "error": error.to_string(),
                    }),
                )
            })
    }
}

trait EffortWizardApplyBackend {
    fn apply_patch(&self, patch: EffortWizardApplyPatch) -> Result<PedelecSettings, PedelecError>;
    fn bootstrap(&self) -> Result<EffortWizardBootstrap, PedelecError>;
}

struct CoreRuntimeEffortWizardApplyBackend {
    runtime: SharedCoreRuntime,
}

impl EffortWizardApplyBackend for CoreRuntimeEffortWizardApplyBackend {
    fn apply_patch(&self, patch: EffortWizardApplyPatch) -> Result<PedelecSettings, PedelecError> {
        self.runtime
            .lock()
            .unwrap()
            .apply_effort_wizard_patch(patch)
    }

    fn bootstrap(&self) -> Result<EffortWizardBootstrap, PedelecError> {
        self.runtime.lock().unwrap().get_effort_wizard_bootstrap()
    }
}

struct IpcEffortProbeExecutor;

impl EffortProbeExecutor for IpcEffortProbeExecutor {
    fn execute(
        &self,
        command: pedelec_core::CommandSpec,
        timeout: Duration,
        cancellation: Arc<AtomicBool>,
    ) -> Result<CapturedProviderProcessOutput, PedelecError> {
        run_provider_command_captured_with_cancel(command, timeout, cancellation)
    }
}

#[derive(Debug)]
enum ProviderProbeExecution {
    Recommendation {
        recommendation: EffortWizardProviderRecommendation,
        results: Vec<EffortWizardProbeResultState>,
    },
    NoRecommendation {
        results: Vec<EffortWizardProbeResultState>,
    },
    Error {
        results: Vec<EffortWizardProbeResultState>,
        reason: String,
    },
    Fatal(PedelecError),
}

#[tauri::command]
pub fn get_effort_wizard_bootstrap(
    state: State<'_, pedelec_core::CoreRuntimeOwner>,
) -> Result<EffortWizardBootstrap, PedelecError> {
    let runtime = state.runtime();
    pedelec_core::wait_for_provider_readiness(&runtime)?;
    let bootstrap = runtime.lock().unwrap().get_effort_wizard_bootstrap();
    bootstrap
}

#[tauri::command]
pub fn get_effort_wizard_state(
    state: State<'_, EffortWizardOwner>,
) -> Option<EffortWizardRunState> {
    state.snapshot()
}

#[tauri::command]
pub fn start_effort_wizard(
    app: AppHandle,
    owner: State<'_, EffortWizardOwner>,
    runtime_owner: State<'_, pedelec_core::CoreRuntimeOwner>,
    input: StartEffortWizardInput,
) -> Result<EffortWizardRunState, PedelecError> {
    let runtime = runtime_owner.runtime();
    pedelec_core::wait_for_provider_readiness(&runtime)?;
    start_run(owner.inner.clone(), runtime, app, input)
}

#[tauri::command]
pub fn apply_effort_wizard_settings(
    app: AppHandle,
    owner: State<'_, EffortWizardOwner>,
    runtime_owner: State<'_, pedelec_core::CoreRuntimeOwner>,
    input: ApplyEffortWizardInput,
) -> Result<ApplyEffortWizardOutput, PedelecError> {
    let backend = CoreRuntimeEffortWizardApplyBackend {
        runtime: runtime_owner.runtime(),
    };
    apply_run(owner.inner.clone(), &backend, app, input)
}

#[tauri::command]
pub fn reset_effort_wizard(
    app: AppHandle,
    owner: State<'_, EffortWizardOwner>,
) -> Result<(), PedelecError> {
    let state = {
        let mut coordinator = owner.inner.lock().unwrap();
        let Some(run) = coordinator.current_run.as_mut() else {
            return Ok(());
        };
        if run.state.status == EffortWizardRunStatus::Applying {
            return Err(wizard_validation_error(
                "an effort wizard cannot be reset while settings are applying",
                serde_json::Value::Null,
            ));
        }
        if run.state.status == EffortWizardRunStatus::Checking {
            run.cancellation.store(true, Ordering::Release);
            run.state.status = EffortWizardRunStatus::Cancelled;
            Some(run.state.clone())
        } else {
            coordinator.current_run = None;
            None
        }
    };
    emit_state(&app, state);
    Ok(())
}

fn start_run(
    coordinator: Arc<Mutex<EffortWizardCoordinator>>,
    runtime: SharedCoreRuntime,
    app: AppHandle,
    input: StartEffortWizardInput,
) -> Result<EffortWizardRunState, PedelecError> {
    {
        let guard = coordinator.lock().unwrap();
        validate_start_against_current_run(&guard)?;
    }
    cleanup_stale_probe_runs();
    let runtime_guard = runtime.lock().unwrap();
    runtime_guard.validate_effort_wizard_selection(&input.providers)?;
    let settings = runtime_guard.get_settings()?;
    let bootstrap = runtime_guard.get_effort_wizard_bootstrap()?;

    let run_id = Uuid::now_v7().to_string();
    let root = probe_root(&run_id)?;
    let cancellation = Arc::new(AtomicBool::new(false));
    let selected = input.providers.clone();
    let selected_set = selected.iter().copied().collect::<HashSet<_>>();
    let available_set = bootstrap
        .providers
        .iter()
        .filter(|provider| provider.available)
        .map(|provider| provider.provider)
        .collect::<HashSet<_>>();

    let mut snapshots = BTreeMap::new();
    let mut provider_states = Vec::with_capacity(WizardProviderCode::all().len());
    for provider in WizardProviderCode::all() {
        let current_efforts = efforts_for_provider(&settings, *provider);
        if !available_set.contains(provider) {
            provider_states.push(provider_state(
                *provider,
                false,
                EffortWizardProviderStatus::Unavailable,
                current_efforts,
            ));
            continue;
        }
        if !selected_set.contains(provider) {
            provider_states.push(provider_state(
                *provider,
                false,
                EffortWizardProviderStatus::Skipped,
                current_efforts,
            ));
            continue;
        }

        let plan = match build_effort_wizard_probe_plan(*provider) {
            Ok(plan) => plan,
            Err(error) => {
                cleanup_probe_root(&root);
                return Err(error);
            }
        };
        let executable = match runtime_guard.provider_executable_path(&provider.as_provider_code())
        {
            Ok(executable) => executable,
            Err(error) => {
                cleanup_probe_root(&root);
                return Err(error);
            }
        };
        let workspace = root.join(provider.as_str());
        if let Err(error) = fs::create_dir(&workspace) {
            cleanup_probe_root(&root);
            return Err(wizard_fatal_error(
                "cannot create a controlled provider probe workspace",
                serde_json::json!({
                    "provider": provider.as_str(),
                    "error": error.to_string(),
                }),
            ));
        }
        snapshots.insert(
            *provider,
            ProviderProbeSnapshot {
                plan,
                executable,
                workspace,
                current_efforts: current_efforts.clone(),
                cancellation: Arc::clone(&cancellation),
            },
        );
        provider_states.push(provider_state(
            *provider,
            true,
            EffortWizardProviderStatus::SelectedPending,
            current_efforts,
        ));
    }

    let state = EffortWizardRunState {
        run_id: run_id.clone(),
        status: EffortWizardRunStatus::Checking,
        selected_providers: selected,
        providers: provider_states,
        review: None,
        completion: None,
        error: None,
        bootstrap: None,
    };

    let dispatch_snapshots = snapshots.clone();
    {
        let mut coordinator_guard = coordinator.lock().unwrap();
        if let Err(error) = validate_start_against_current_run(&coordinator_guard) {
            cleanup_probe_root(&root);
            return Err(error);
        }
        coordinator_guard.current_run = Some(EffortWizardRun {
            state: state.clone(),
            snapshots,
            cancellation,
        });
    }
    emit_state(&app, Some(state.clone()));

    let dispatcher = ThreadEffortProbeDispatcher {
        coordinator: Arc::clone(&coordinator),
        app: app.clone(),
        run_id: run_id.clone(),
    };
    if let Err(error) =
        dispatch_selected_probe_work(input.providers.as_slice(), &dispatch_snapshots, &dispatcher)
    {
        mark_fatal(&coordinator, &app, &run_id, error.clone());
        return Err(error);
    }

    Ok(state)
}

fn validate_start_against_current_run(
    coordinator: &EffortWizardCoordinator,
) -> Result<(), PedelecError> {
    if coordinator.current_run.as_ref().is_some_and(|run| {
        matches!(
            run.state.status,
            EffortWizardRunStatus::Checking | EffortWizardRunStatus::Applying
        )
    }) {
        return Err(wizard_validation_error(
            "an effort wizard run is already in progress",
            serde_json::Value::Null,
        ));
    }
    Ok(())
}

fn dispatch_selected_probe_work(
    selected: &[WizardProviderCode],
    snapshots: &BTreeMap<WizardProviderCode, ProviderProbeSnapshot>,
    dispatcher: &dyn EffortProbeDispatcher,
) -> Result<(), PedelecError> {
    for provider in selected {
        let Some(snapshot) = snapshots.get(provider).cloned() else {
            continue;
        };
        dispatcher.dispatch(*provider, snapshot)?;
    }
    Ok(())
}

fn run_provider_worker(
    coordinator: Arc<Mutex<EffortWizardCoordinator>>,
    app: AppHandle,
    run_id: String,
    provider: WizardProviderCode,
    snapshot: ProviderProbeSnapshot,
) {
    set_provider_checking(&coordinator, &app, &run_id, provider);

    let executor = IpcEffortProbeExecutor;
    let result = execute_provider_probe_with_executor(&snapshot, &executor);
    cleanup_probe_workspace(&snapshot.workspace);
    finish_provider(&coordinator, &app, &run_id, provider, result);
}

fn set_provider_checking(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    app: &AppHandle,
    run_id: &str,
    provider: WizardProviderCode,
) {
    let state = {
        let mut guard = coordinator.lock().unwrap();
        let Some(run) = guard
            .current_run
            .as_mut()
            .filter(|run| run.state.run_id == run_id)
        else {
            return;
        };
        if matches!(
            run.state.status,
            EffortWizardRunStatus::FatalError | EffortWizardRunStatus::Cancelled
        ) {
            return;
        }
        let Some(provider_state) = run
            .state
            .providers
            .iter_mut()
            .find(|state| state.provider == provider)
        else {
            return;
        };
        provider_state.status = EffortWizardProviderStatus::Checking;
        Some(run.state.clone())
    };
    emit_state(app, state);
}

fn finish_provider(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    app: &AppHandle,
    run_id: &str,
    provider: WizardProviderCode,
    result: ProviderProbeExecution,
) {
    let state = {
        let mut guard = coordinator.lock().unwrap();
        let Some(run) = guard
            .current_run
            .as_mut()
            .filter(|run| run.state.run_id == run_id)
        else {
            return;
        };
        if matches!(
            run.state.status,
            EffortWizardRunStatus::FatalError | EffortWizardRunStatus::Cancelled
        ) {
            return;
        }
        if apply_provider_execution(&mut run.state, provider, result) {
            return emit_state(app, Some(run.state.clone()));
        }

        mark_review_ready_if_complete(&mut run.state);
        Some(run.state.clone())
    };
    emit_state(app, state);
}

fn apply_provider_execution(
    state: &mut EffortWizardRunState,
    provider: WizardProviderCode,
    result: ProviderProbeExecution,
) -> bool {
    if !state
        .providers
        .iter()
        .any(|current| current.provider == provider)
    {
        return false;
    }
    match result {
        ProviderProbeExecution::Recommendation {
            recommendation,
            results,
        } => {
            let Some(provider_state) = state
                .providers
                .iter_mut()
                .find(|state| state.provider == provider)
            else {
                return false;
            };
            provider_state.status = EffortWizardProviderStatus::ReviewReady;
            provider_state.recommendation = Some(recommendation);
            provider_state.probe_results = results;
            provider_state.error = None;
        }
        ProviderProbeExecution::NoRecommendation { results } => {
            let Some(provider_state) = state
                .providers
                .iter_mut()
                .find(|state| state.provider == provider)
            else {
                return false;
            };
            provider_state.status = EffortWizardProviderStatus::NoRecommendation;
            provider_state.probe_results = results;
            provider_state.error =
                Some("the provider did not grant access to a bundled probe model".to_string());
        }
        ProviderProbeExecution::Error { results, reason } => {
            let Some(provider_state) = state
                .providers
                .iter_mut()
                .find(|state| state.provider == provider)
            else {
                return false;
            };
            provider_state.status = EffortWizardProviderStatus::ProbeError;
            provider_state.probe_results = results;
            provider_state.error = Some(reason);
        }
        ProviderProbeExecution::Fatal(error) => {
            state.status = EffortWizardRunStatus::FatalError;
            state.error = Some(EffortWizardFatalError {
                code: error.code,
                message: error.message,
            });
            return true;
        }
    }
    false
}

fn mark_review_ready_if_complete(state: &mut EffortWizardRunState) {
    if state
        .selected_providers
        .iter()
        .all(|selected| is_terminal_provider_state(state, *selected))
    {
        state.review = Some(build_review_state(state));
        state.status = EffortWizardRunStatus::ReviewReady;
    }
}

fn reviewable_provider_set(state: &EffortWizardRunState) -> HashSet<WizardProviderCode> {
    state
        .providers
        .iter()
        .filter(|provider| {
            provider.status == EffortWizardProviderStatus::ReviewReady
                && provider
                    .recommendation
                    .as_ref()
                    .is_some_and(EffortWizardProviderRecommendation::has_confirmed_tier)
        })
        .map(|provider| provider.provider)
        .collect()
}

fn validate_apply_decisions(
    state: &EffortWizardRunState,
    decisions: &[EffortWizardProviderDecision],
) -> Result<HashSet<WizardProviderCode>, PedelecError> {
    let reviewable = reviewable_provider_set(state);
    let mut seen = HashSet::new();
    for decision in decisions {
        if !seen.insert(decision.provider) {
            return Err(wizard_validation_error(
                "a provider may appear only once in apply decisions",
                serde_json::json!({ "provider": decision.provider.as_str() }),
            ));
        }
        if !reviewable.contains(&decision.provider) {
            return Err(wizard_validation_error(
                "apply decisions may only target reviewable providers",
                serde_json::json!({ "provider": decision.provider.as_str() }),
            ));
        }
    }
    if seen != reviewable {
        return Err(wizard_validation_error(
            "every reviewable provider requires explicit tier decisions",
            serde_json::json!({
                "expectedProviders": reviewable.iter().map(|provider| provider.as_str()).collect::<Vec<_>>(),
            }),
        ));
    }
    Ok(reviewable)
}

fn build_apply_patches(
    run: &EffortWizardRun,
    decisions: &[EffortWizardProviderDecision],
) -> Result<Vec<EffortWizardProviderApplyPatch>, PedelecError> {
    decisions
        .iter()
        .map(|decision| {
            let provider_state = run
                .state
                .providers
                .iter()
                .find(|provider| provider.provider == decision.provider)
                .ok_or_else(|| {
                    wizard_validation_error(
                        "apply decision provider is missing from the current run",
                        serde_json::json!({ "provider": decision.provider.as_str() }),
                    )
                })?;
            let recommendation = provider_state.recommendation.clone().ok_or_else(|| {
                wizard_validation_error(
                    "reviewable provider has no stored recommendation",
                    serde_json::json!({ "provider": decision.provider.as_str() }),
                )
            })?;
            let current_efforts = run
                .snapshots
                .get(&decision.provider)
                .ok_or_else(|| {
                    wizard_validation_error(
                        "selected reviewable provider has no settings snapshot",
                        serde_json::json!({ "provider": decision.provider.as_str() }),
                    )
                })?
                .current_efforts
                .clone();
            Ok(EffortWizardProviderApplyPatch {
                provider: decision.provider,
                preset_revision: recommendation.preset_revision,
                expected_current_efforts: current_efforts,
                confirmed_recommendation: recommendation,
                tier_decisions: decision.tiers,
                can_advance_revision: true,
            })
        })
        .collect()
}

fn apply_run(
    coordinator: Arc<Mutex<EffortWizardCoordinator>>,
    backend: &dyn EffortWizardApplyBackend,
    app: AppHandle,
    input: ApplyEffortWizardInput,
) -> Result<ApplyEffortWizardOutput, PedelecError> {
    let emit = |state| emit_state(&app, state);
    apply_run_with_backend(coordinator, backend, &emit, input)
}

fn apply_run_with_backend(
    coordinator: Arc<Mutex<EffortWizardCoordinator>>,
    backend: &dyn EffortWizardApplyBackend,
    emit: &dyn Fn(Option<EffortWizardRunState>),
    input: ApplyEffortWizardInput,
) -> Result<ApplyEffortWizardOutput, PedelecError> {
    let (run_id, decisions, patches) = {
        let mut guard = coordinator.lock().unwrap();
        let run = guard.current_run.as_mut().ok_or_else(|| {
            wizard_validation_error(
                "there is no effort wizard run to apply",
                serde_json::Value::Null,
            )
        })?;
        if run.state.run_id != input.run_id {
            return Err(wizard_validation_error(
                "the effort wizard run id is no longer current",
                serde_json::json!({ "runId": input.run_id }),
            ));
        }
        if run.state.status != EffortWizardRunStatus::ReviewReady {
            return Err(wizard_validation_error(
                "effort wizard settings can only be applied from review",
                serde_json::json!({ "status": run.state.status }),
            ));
        }

        validate_apply_decisions(&run.state, &input.providers)?;
        let patches = build_apply_patches(run, &input.providers)?;

        run.state.status = EffortWizardRunStatus::Applying;
        run.state.error = None;
        (
            run.state.run_id.clone(),
            input.providers,
            EffortWizardApplyPatch { providers: patches },
        )
    };
    emit(current_run_state(&coordinator, &run_id));

    if let Err(error) = backend.apply_patch(patches) {
        restore_apply_failure_with_emitter(&coordinator, &run_id, &error, emit);
        return Err(error);
    }

    let bootstrap = match backend.bootstrap() {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            restore_apply_failure_with_emitter(&coordinator, &run_id, &error, emit);
            return Err(error);
        }
    };
    let completion = build_completion_state(&coordinator, &run_id, &decisions)?;
    let state = {
        let mut guard = coordinator.lock().unwrap();
        let run = guard.current_run.as_mut().ok_or_else(|| {
            wizard_validation_error(
                "the effort wizard run was cleared during apply",
                serde_json::Value::Null,
            )
        })?;
        if run.state.run_id != run_id {
            return Err(wizard_validation_error(
                "the effort wizard run was replaced during apply",
                serde_json::Value::Null,
            ));
        }
        transition_after_apply(&mut run.state, completion, bootstrap.clone());
        run.state.clone()
    };
    emit(Some(state.clone()));
    Ok(ApplyEffortWizardOutput { state, bootstrap })
}

fn restore_apply_failure_with_emitter(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    run_id: &str,
    error: &PedelecError,
    emit: &dyn Fn(Option<EffortWizardRunState>),
) {
    let state = {
        let mut guard = coordinator.lock().unwrap();
        let Some(run) = guard
            .current_run
            .as_mut()
            .filter(|run| run.state.run_id == run_id)
        else {
            return;
        };
        restore_apply_failure_state(&mut run.state, error);
        Some(run.state.clone())
    };
    emit(state);
}

fn restore_apply_failure_state(state: &mut EffortWizardRunState, error: &PedelecError) {
    state.status = EffortWizardRunStatus::ReviewReady;
    state.error = Some(EffortWizardFatalError {
        code: error.code.clone(),
        message: error.message.clone(),
    });
}

fn transition_after_apply(
    state: &mut EffortWizardRunState,
    completion: EffortWizardCompletionState,
    bootstrap: EffortWizardBootstrap,
) {
    state.status = EffortWizardRunStatus::Completed;
    state.completion = Some(completion);
    state.bootstrap = Some(bootstrap);
    state.error = None;
}

fn build_completion_state(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    run_id: &str,
    decisions: &[EffortWizardProviderDecision],
) -> Result<EffortWizardCompletionState, PedelecError> {
    let guard = coordinator.lock().unwrap();
    let run = guard.current_run.as_ref().ok_or_else(|| {
        wizard_validation_error(
            "the effort wizard run was cleared during apply",
            serde_json::Value::Null,
        )
    })?;
    if run.state.run_id != run_id {
        return Err(wizard_validation_error(
            "the effort wizard run was replaced during apply",
            serde_json::Value::Null,
        ));
    }
    let decisions = decisions
        .iter()
        .map(|decision| (decision.provider, decision.tiers))
        .collect::<HashMap<_, _>>();
    let providers = run
        .state
        .providers
        .iter()
        .map(|provider| {
            let status = match provider.status {
                EffortWizardProviderStatus::Skipped => EffortWizardCompletionStatus::Skipped,
                EffortWizardProviderStatus::Unavailable => {
                    EffortWizardCompletionStatus::Unavailable
                }
                EffortWizardProviderStatus::NoRecommendation => {
                    EffortWizardCompletionStatus::NoRecommendation
                }
                EffortWizardProviderStatus::ProbeError => {
                    EffortWizardCompletionStatus::NeedsAttention
                }
                EffortWizardProviderStatus::ReviewReady => {
                    let tiers = decisions
                        .get(&provider.provider)
                        .copied()
                        .unwrap_or_default();
                    let has_update = [tiers.low, tiers.default, tiers.high]
                        .into_iter()
                        .any(|decision| decision == EffortWizardTierDecision::Update);
                    if has_update {
                        EffortWizardCompletionStatus::UpdatedOrConfirmed
                    } else {
                        EffortWizardCompletionStatus::KeptCurrent
                    }
                }
                EffortWizardProviderStatus::SelectedPending
                | EffortWizardProviderStatus::Checking => {
                    EffortWizardCompletionStatus::NeedsAttention
                }
            };
            EffortWizardProviderCompletionState {
                provider: provider.provider,
                status,
            }
        })
        .collect::<Vec<_>>();
    Ok(EffortWizardCompletionState {
        confirmed_provider_count: decisions.len(),
        providers,
    })
}

fn execute_provider_probe_with_executor(
    snapshot: &ProviderProbeSnapshot,
    executor: &dyn EffortProbeExecutor,
) -> ProviderProbeExecution {
    let mut probe_id = snapshot.plan.entry_probe.clone();
    let mut confirmed = EffortWizardConfirmedProfiles::default();
    let mut results = Vec::new();

    for _ in 0..2 {
        let Some(probe) = snapshot.plan.probe_definition(&probe_id) else {
            return ProviderProbeExecution::Fatal(wizard_fatal_error(
                "the bundled effort wizard probe transition is invalid",
                serde_json::json!({
                    "provider": snapshot.plan.provider.as_str(),
                    "probe": probe_id,
                }),
            ));
        };
        let command = build_probe_command(
            snapshot.plan.provider,
            &snapshot.executable,
            &snapshot.workspace,
            probe,
        );
        let output =
            match executor.execute(command, PROBE_TIMEOUT, Arc::clone(&snapshot.cancellation)) {
                Ok(output) => output,
                Err(_) => {
                    results.push(EffortWizardProbeResultState {
                        probe_id: probe.id.clone(),
                        outcome: EffortWizardProbeOutcome::TransientError,
                        reason: Some("provider process could not be started".to_string()),
                    });
                    return ProviderProbeExecution::Error {
                        results,
                        reason: "provider process could not be started".to_string(),
                    };
                }
            };

        if output.cancelled
            || output.timed_out
            || output.stdout_truncated
            || output.stderr_truncated
        {
            results.push(EffortWizardProbeResultState {
                probe_id: probe.id.clone(),
                outcome: EffortWizardProbeOutcome::TransientError,
                reason: Some(if output.cancelled {
                    "provider probe was cancelled".to_string()
                } else if output.timed_out {
                    "provider probe timed out".to_string()
                } else {
                    "provider probe output exceeded the capture limit".to_string()
                }),
            });
            return ProviderProbeExecution::Error {
                results,
                reason: "provider probe failed transiently".to_string(),
            };
        }

        let Some(exit_code) = output.exit_code else {
            results.push(EffortWizardProbeResultState {
                probe_id: probe.id.clone(),
                outcome: EffortWizardProbeOutcome::TransientError,
                reason: Some("provider process did not report an exit status".to_string()),
            });
            return ProviderProbeExecution::Error {
                results,
                reason: "provider probe failed transiently".to_string(),
            };
        };

        let classification = if exit_code == 0 {
            ProbeExecutionResult::Supported
        } else {
            classify_probe_failure(
                snapshot.plan.provider,
                Some(exit_code),
                &output.stdout,
                &output.stderr,
            )
        };
        match classification {
            ProbeExecutionResult::Supported => {
                results.push(EffortWizardProbeResultState {
                    probe_id: probe.id.clone(),
                    outcome: EffortWizardProbeOutcome::Supported,
                    reason: None,
                });
                let transition = match snapshot.plan.transition_after_supported(&probe.id) {
                    Ok(transition) => transition,
                    Err(error) => return ProviderProbeExecution::Fatal(error),
                };
                add_confirmations(
                    &mut confirmed,
                    transition.confirms.as_slice(),
                    &snapshot.plan,
                );
                if let Some(next_probe) = &transition.next_probe {
                    probe_id = next_probe.clone();
                } else {
                    return recommendation_or_none(
                        snapshot.plan.provider,
                        snapshot.plan.revision,
                        confirmed,
                        results,
                    );
                }
            }
            ProbeExecutionResult::NotEntitled { reason } => {
                results.push(EffortWizardProbeResultState {
                    probe_id: probe.id.clone(),
                    outcome: EffortWizardProbeOutcome::NotEntitled,
                    reason: Some(reason),
                });
                let transition = match snapshot.plan.transition_after_not_entitled(&probe.id) {
                    Ok(transition) => transition,
                    Err(error) => return ProviderProbeExecution::Fatal(error),
                };
                add_confirmations(
                    &mut confirmed,
                    transition.confirms.as_slice(),
                    &snapshot.plan,
                );
                if let Some(next_probe) = &transition.next_probe {
                    probe_id = next_probe.clone();
                } else {
                    return recommendation_or_none(
                        snapshot.plan.provider,
                        snapshot.plan.revision,
                        confirmed,
                        results,
                    );
                }
            }
            ProbeExecutionResult::TransientError { reason } => {
                results.push(EffortWizardProbeResultState {
                    probe_id: probe.id.clone(),
                    outcome: EffortWizardProbeOutcome::TransientError,
                    reason: Some(reason),
                });
                return ProviderProbeExecution::Error {
                    results,
                    reason: "provider probe failed transiently".to_string(),
                };
            }
        }
    }

    ProviderProbeExecution::Fatal(wizard_fatal_error(
        "the bundled effort wizard probe plan exceeded its two-probe limit",
        serde_json::json!({ "provider": snapshot.plan.provider.as_str() }),
    ))
}

fn recommendation_or_none(
    provider: WizardProviderCode,
    revision: u32,
    confirmed: EffortWizardConfirmedProfiles,
    results: Vec<EffortWizardProbeResultState>,
) -> ProviderProbeExecution {
    if confirmed.has_any() {
        ProviderProbeExecution::Recommendation {
            recommendation: EffortWizardProviderRecommendation {
                provider,
                preset_revision: revision,
                confirmed,
                deterministic_complete: true,
            },
            results,
        }
    } else {
        ProviderProbeExecution::NoRecommendation { results }
    }
}

fn add_confirmations(
    confirmed: &mut EffortWizardConfirmedProfiles,
    levels: &[EffortLevel],
    plan: &EffortWizardProbePlan,
) {
    for level in levels {
        let profile = match level {
            EffortLevel::Low => &plan.profiles.low,
            EffortLevel::Default => &plan.profiles.default,
            EffortLevel::High => &plan.profiles.high,
        };
        match level {
            EffortLevel::Low => confirmed.low = Some(profile.clone()),
            EffortLevel::Default => confirmed.default = Some(profile.clone()),
            EffortLevel::High => confirmed.high = Some(profile.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeExecutionResult {
    Supported,
    NotEntitled { reason: String },
    TransientError { reason: String },
}

fn classify_probe_failure(
    provider: WizardProviderCode,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> ProbeExecutionResult {
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if is_transient_diagnostic(exit_code, &text) {
        return ProbeExecutionResult::TransientError {
            reason: "provider authentication or service error".to_string(),
        };
    }

    if has_explicit_model_entitlement_denial(provider, &text) {
        return ProbeExecutionResult::NotEntitled {
            reason: "requested probe model is unavailable for this account".to_string(),
        };
    }

    ProbeExecutionResult::TransientError {
        reason: "provider returned an unrecognized probe error".to_string(),
    }
}

fn is_transient_diagnostic(exit_code: Option<i32>, text: &str) -> bool {
    if matches!(
        exit_code,
        Some(401 | 403 | 408 | 429 | 500 | 502 | 503 | 504)
    ) {
        return true;
    }
    [
        "unauthorized",
        "not authenticated",
        "authentication failed",
        "authentication required",
        "login expired",
        "oauth expired",
        "token expired",
        "network error",
        "network unavailable",
        "connection refused",
        "connection reset",
        "service unavailable",
        "temporarily unavailable",
        "internal server error",
        "rate limit",
        "too many requests",
        "timed out",
        "timeout",
        "econnreset",
        "enotfound",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn has_explicit_model_entitlement_denial(provider: WizardProviderCode, text: &str) -> bool {
    let model_context = text.contains("model")
        || text.contains("model_id")
        || text.contains("model id")
        || text.contains("capability");
    if !model_context {
        return false;
    }

    let unavailable = [
        "not available",
        "unavailable",
        "not found",
        "unknown model",
        "does not exist",
        "cannot be used",
        "can't be used",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    let account_denial = [
        "account does not have access",
        "account cannot access",
        "account can't access",
        "access denied for model",
        "model access denied",
        "subscription does not include",
        "not included in your plan",
        "requires a different plan",
        "requires an upgrade",
        "requires different entitlement",
        "requires a different entitlement",
        "not available to this account",
        "not entitled",
    ]
    .iter()
    .any(|needle| text.contains(needle));

    match provider {
        WizardProviderCode::Codex
        | WizardProviderCode::Claude
        | WizardProviderCode::Cursor
        | WizardProviderCode::Antigravity => unavailable || account_denial,
    }
}

fn build_probe_command(
    provider: WizardProviderCode,
    executable: &Path,
    workspace: &Path,
    probe: &EffortWizardProbeDefinition,
) -> pedelec_core::CommandSpec {
    let workspace = path_for_external_use(workspace);
    let executable = path_for_external_use(executable);
    let mut args = match provider {
        WizardProviderCode::Codex => vec![
            "exec".to_string(),
            "--cd".to_string(),
            workspace.clone(),
            "--sandbox".to_string(),
            "read-only".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
        ],
        WizardProviderCode::Claude => vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
        WizardProviderCode::Cursor => vec![
            "--workspace".to_string(),
            workspace.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--force".to_string(),
            "--trust".to_string(),
        ],
        WizardProviderCode::Antigravity => vec![
            "-p".to_string(),
            PROBE_PROMPT.trim_end().to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--mode".to_string(),
            "accept-edits".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
    };
    args.extend(probe.args.clone());
    if matches!(provider, WizardProviderCode::Codex) {
        args.push("-".to_string());
    }

    pedelec_core::CommandSpec {
        program: executable,
        args,
        cwd: workspace.into(),
        env: Vec::new(),
        prompt: PROBE_PROMPT.to_string(),
        stdin: if matches!(provider, WizardProviderCode::Antigravity) {
            String::new()
        } else {
            PROBE_PROMPT.to_string()
        },
    }
}

fn build_review_state(state: &EffortWizardRunState) -> EffortWizardReviewState {
    let providers = state
        .providers
        .iter()
        .map(|provider| EffortWizardProviderReviewState {
            provider: provider.provider,
            current_efforts: provider.current_efforts.clone(),
            recommendation: provider.recommendation.clone(),
            status: provider.status.clone(),
            error: provider.error.clone(),
        })
        .collect::<Vec<_>>();
    let reviewable_provider_count = providers
        .iter()
        .filter(|provider| {
            provider.status == EffortWizardProviderStatus::ReviewReady
                && provider
                    .recommendation
                    .as_ref()
                    .is_some_and(EffortWizardProviderRecommendation::has_confirmed_tier)
        })
        .count();
    EffortWizardReviewState {
        providers,
        reviewable_provider_count,
    }
}

fn build_probe_workspace(run_id: &str) -> Result<PathBuf, PedelecError> {
    let root = std::env::temp_dir().join(PROBE_ROOT_NAME).join(run_id);
    fs::create_dir_all(&root).map_err(|error| {
        wizard_fatal_error(
            "cannot create a controlled provider probe workspace",
            serde_json::json!({ "error": error.to_string() }),
        )
    })?;
    Ok(root)
}

fn probe_root(run_id: &str) -> Result<PathBuf, PedelecError> {
    build_probe_workspace(run_id)
}

/// Removes only stale directories under the Pedelec-owned probe root. Probe
/// workspaces never overlap a user-selected workspace or the repository.
pub fn cleanup_stale_probe_runs() {
    let root = std::env::temp_dir().join(PROBE_ROOT_NAME);
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }
    let _ = fs::remove_dir(root);
}

fn cleanup_probe_workspace(workspace: &Path) {
    let _ = fs::remove_dir_all(workspace);
    if let Some(root) = workspace.parent() {
        let _ = fs::remove_dir(root);
    }
}

fn cleanup_probe_root(root: &Path) {
    let _ = fs::remove_dir_all(root);
    if let Some(parent) = root.parent() {
        let _ = fs::remove_dir(parent);
    }
}

fn provider_state(
    provider: WizardProviderCode,
    selected: bool,
    status: EffortWizardProviderStatus,
    current_efforts: EffortsArgs,
) -> EffortWizardProviderRunState {
    EffortWizardProviderRunState {
        provider,
        selected,
        status,
        current_efforts,
        recommendation: None,
        probe_results: Vec::new(),
        error: None,
    }
}

fn efforts_for_provider(settings: &PedelecSettings, provider: WizardProviderCode) -> EffortsArgs {
    match provider {
        WizardProviderCode::Codex => settings.provider_settings.codex.efforts_args.clone(),
        WizardProviderCode::Claude => settings.provider_settings.claude.efforts_args.clone(),
        WizardProviderCode::Cursor => settings.provider_settings.cursor.efforts_args.clone(),
        WizardProviderCode::Antigravity => {
            settings.provider_settings.antigravity.efforts_args.clone()
        }
    }
}

fn is_terminal_provider_state(state: &EffortWizardRunState, provider: WizardProviderCode) -> bool {
    state
        .providers
        .iter()
        .find(|current| current.provider == provider)
        .is_some_and(|current| {
            matches!(
                current.status,
                EffortWizardProviderStatus::ReviewReady
                    | EffortWizardProviderStatus::ProbeError
                    | EffortWizardProviderStatus::NoRecommendation
                    | EffortWizardProviderStatus::Unavailable
                    | EffortWizardProviderStatus::Skipped
            )
        })
}

fn current_run_state(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    run_id: &str,
) -> Option<EffortWizardRunState> {
    let state = coordinator
        .lock()
        .unwrap()
        .current_run
        .as_ref()
        .filter(|run| run.state.run_id == run_id)
        .map(|run| run.state.clone());
    state
}

fn mark_fatal(
    coordinator: &Arc<Mutex<EffortWizardCoordinator>>,
    app: &AppHandle,
    run_id: &str,
    error: PedelecError,
) {
    let state = {
        let mut guard = coordinator.lock().unwrap();
        let Some(run) = guard
            .current_run
            .as_mut()
            .filter(|run| run.state.run_id == run_id)
        else {
            return;
        };
        run.state.status = EffortWizardRunStatus::FatalError;
        run.state.error = Some(EffortWizardFatalError {
            code: error.code,
            message: error.message,
        });
        run.state.clone()
    };
    emit_state(app, Some(state));
}

fn emit_state(app: &AppHandle, state: Option<EffortWizardRunState>) {
    let _ = app.emit("effort_wizard_state_changed", state);
}

fn wizard_validation_error(message: &str, details: serde_json::Value) -> PedelecError {
    PedelecError::with_details(error_codes::EFFORT_WIZARD_APPLY_INVALID, message, details)
}

fn wizard_fatal_error(message: &str, details: serde_json::Value) -> PedelecError {
    PedelecError::with_details(error_codes::EFFORT_WIZARD_PRESET_INVALID, message, details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_core::{EffortWizardHomeReminder, EffortWizardProviderBootstrap};
    use std::sync::Mutex;

    struct FakeExecutor {
        outputs: Mutex<Vec<Result<CapturedProviderProcessOutput, PedelecError>>>,
        commands: Mutex<Vec<pedelec_core::CommandSpec>>,
    }

    impl FakeExecutor {
        fn new(outputs: Vec<Result<CapturedProviderProcessOutput, PedelecError>>) -> Self {
            Self {
                outputs: Mutex::new(outputs),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl EffortProbeExecutor for FakeExecutor {
        fn execute(
            &self,
            command: pedelec_core::CommandSpec,
            _timeout: Duration,
            _cancellation: Arc<AtomicBool>,
        ) -> Result<CapturedProviderProcessOutput, PedelecError> {
            self.commands.lock().unwrap().push(command);
            self.outputs.lock().unwrap().remove(0)
        }
    }

    struct RecordingDispatcher {
        providers: Mutex<Vec<WizardProviderCode>>,
    }

    impl RecordingDispatcher {
        fn new() -> Self {
            Self {
                providers: Mutex::new(Vec::new()),
            }
        }
    }

    impl EffortProbeDispatcher for RecordingDispatcher {
        fn dispatch(
            &self,
            provider: WizardProviderCode,
            _snapshot: ProviderProbeSnapshot,
        ) -> Result<(), PedelecError> {
            self.providers.lock().unwrap().push(provider);
            Ok(())
        }
    }

    struct FakeApplyBackend {
        patches: Mutex<Vec<EffortWizardApplyPatch>>,
        apply_error: Option<PedelecError>,
        bootstrap: Mutex<EffortWizardBootstrap>,
        bootstrap_calls: Mutex<usize>,
        applied_revisions: Mutex<BTreeMap<WizardProviderCode, u32>>,
    }

    impl FakeApplyBackend {
        fn new(bootstrap: EffortWizardBootstrap, apply_error: Option<PedelecError>) -> Self {
            let applied_revisions = bootstrap
                .providers
                .iter()
                .filter_map(|provider| {
                    provider
                        .applied_preset_revision
                        .map(|revision| (provider.provider, revision))
                })
                .collect();
            Self {
                patches: Mutex::new(Vec::new()),
                apply_error,
                bootstrap: Mutex::new(bootstrap),
                bootstrap_calls: Mutex::new(0),
                applied_revisions: Mutex::new(applied_revisions),
            }
        }

        fn refreshed_bootstrap(&self) -> EffortWizardBootstrap {
            let mut bootstrap = self.bootstrap.lock().unwrap().clone();
            let applied_revisions = self.applied_revisions.lock().unwrap();
            let mut outdated = Vec::new();
            for provider in &mut bootstrap.providers {
                provider.applied_preset_revision =
                    applied_revisions.get(&provider.provider).copied();
                provider.preset_update_available = provider.available
                    && provider
                        .applied_preset_revision
                        .is_some_and(|applied| provider.current_preset_revision > applied);
                if provider.preset_update_available {
                    outdated.push(provider.provider);
                }
            }
            bootstrap.home_reminder =
                (!outdated.is_empty()).then_some(EffortWizardHomeReminder::PresetUpdate {
                    providers: outdated,
                });
            bootstrap
        }
    }

    impl EffortWizardApplyBackend for FakeApplyBackend {
        fn apply_patch(
            &self,
            patch: EffortWizardApplyPatch,
        ) -> Result<PedelecSettings, PedelecError> {
            self.patches.lock().unwrap().push(patch.clone());
            if let Some(error) = &self.apply_error {
                return Err(error.clone());
            }
            let mut applied_revisions = self.applied_revisions.lock().unwrap();
            for provider in patch.providers {
                applied_revisions.insert(provider.provider, provider.preset_revision);
            }
            Ok(PedelecSettings::default())
        }

        fn bootstrap(&self) -> Result<EffortWizardBootstrap, PedelecError> {
            *self.bootstrap_calls.lock().unwrap() += 1;
            let bootstrap = self.refreshed_bootstrap();
            *self.bootstrap.lock().unwrap() = bootstrap.clone();
            Ok(bootstrap)
        }
    }

    fn output(exit_code: i32, stdout: &str, stderr: &str) -> CapturedProviderProcessOutput {
        CapturedProviderProcessOutput {
            exit_code: Some(exit_code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
            cancelled: false,
        }
    }

    fn snapshot(provider: WizardProviderCode) -> ProviderProbeSnapshot {
        ProviderProbeSnapshot {
            plan: build_effort_wizard_probe_plan(provider).unwrap(),
            executable: PathBuf::from(if cfg!(windows) {
                r"C:\provider.exe"
            } else {
                "/provider"
            }),
            workspace: PathBuf::from(if cfg!(windows) { r"C:\probe" } else { "/probe" }),
            current_efforts: EffortsArgs::default(),
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }

    fn run_state(selected: &[WizardProviderCode]) -> EffortWizardRunState {
        let selected = selected.to_vec();
        let selected_set = selected.iter().copied().collect::<HashSet<_>>();
        EffortWizardRunState {
            run_id: "test-run".to_string(),
            status: EffortWizardRunStatus::Checking,
            selected_providers: selected,
            providers: WizardProviderCode::all()
                .iter()
                .map(|provider| {
                    let selected = selected_set.contains(provider);
                    provider_state(
                        *provider,
                        selected,
                        if selected {
                            EffortWizardProviderStatus::SelectedPending
                        } else {
                            EffortWizardProviderStatus::Skipped
                        },
                        EffortsArgs::default(),
                    )
                })
                .collect(),
            review: None,
            completion: None,
            error: None,
            bootstrap: None,
        }
    }

    fn recommendation(provider: WizardProviderCode) -> EffortWizardProviderRecommendation {
        EffortWizardProviderRecommendation {
            provider,
            preset_revision: 2,
            confirmed: EffortWizardConfirmedProfiles {
                low: Some(vec!["--model".to_string(), "low-model".to_string()]),
                default: None,
                high: None,
            },
            deterministic_complete: true,
        }
    }

    fn apply_bootstrap_fixture() -> EffortWizardBootstrap {
        let provider =
            |provider, available, applied_preset_revision| EffortWizardProviderBootstrap {
                provider,
                available,
                version: Some("test-provider".to_string()),
                current_preset_revision: 2,
                applied_preset_revision,
                has_any_effort_setting: true,
                preset_update_available: available
                    && applied_preset_revision.is_some_and(|applied| 2 > applied),
            };
        EffortWizardBootstrap {
            providers: vec![
                provider(WizardProviderCode::Codex, true, Some(1)),
                provider(WizardProviderCode::Claude, true, Some(1)),
                provider(WizardProviderCode::Cursor, false, Some(2)),
                provider(WizardProviderCode::Antigravity, false, Some(2)),
            ],
            home_reminder: Some(EffortWizardHomeReminder::PresetUpdate {
                providers: vec![WizardProviderCode::Codex, WizardProviderCode::Claude],
            }),
        }
    }

    fn review_ready_apply_coordinator() -> Arc<Mutex<EffortWizardCoordinator>> {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        state
            .providers
            .iter_mut()
            .find(|provider| provider.provider == WizardProviderCode::Cursor)
            .unwrap()
            .status = EffortWizardProviderStatus::NoRecommendation;
        state
            .providers
            .iter_mut()
            .find(|provider| provider.provider == WizardProviderCode::Antigravity)
            .unwrap()
            .status = EffortWizardProviderStatus::Unavailable;
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Recommendation {
                recommendation: recommendation(WizardProviderCode::Codex),
                results: vec![],
            },
        );
        mark_review_ready_if_complete(&mut state);
        Arc::new(Mutex::new(EffortWizardCoordinator {
            current_run: Some(EffortWizardRun {
                state,
                snapshots: BTreeMap::from([(
                    WizardProviderCode::Codex,
                    snapshot(WizardProviderCode::Codex),
                )]),
                cancellation: Arc::new(AtomicBool::new(false)),
            }),
        }))
    }

    fn codex_apply_input() -> ApplyEffortWizardInput {
        ApplyEffortWizardInput {
            run_id: "test-run".to_string(),
            providers: vec![EffortWizardProviderDecision {
                provider: WizardProviderCode::Codex,
                tiers: EffortWizardTierDecisions::default(),
            }],
        }
    }

    #[test]
    fn successful_probe_does_not_require_exact_ok_text() {
        let executor = FakeExecutor::new(vec![
            Ok(output(0, "I can't help with that.", "")),
            Ok(output(0, "done", "")),
        ]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Codex), &executor);
        let ProviderProbeExecution::Recommendation { recommendation, .. } = result else {
            panic!("expected recommendation");
        };
        assert!(recommendation.confirmed.high.is_some());
        assert!(recommendation.confirmed.low.is_some());
        assert_eq!(executor.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn transient_premium_failure_does_not_run_baseline() {
        let executor = FakeExecutor::new(vec![Ok(output(1, "network unavailable", ""))]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Codex), &executor);
        assert!(matches!(result, ProviderProbeExecution::Error { .. }));
        assert_eq!(executor.commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn premium_not_entitled_then_baseline_supported_is_partial() {
        let executor = FakeExecutor::new(vec![
            Ok(output(
                1,
                "requested model is not available for this account",
                "",
            )),
            Ok(output(0, "accepted", "")),
        ]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Claude), &executor);
        let ProviderProbeExecution::Recommendation { recommendation, .. } = result else {
            panic!("expected partial recommendation");
        };
        assert!(recommendation.confirmed.high.is_none());
        assert!(recommendation.confirmed.low.is_some());
        assert_eq!(executor.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn all_not_entitled_is_a_terminal_no_recommendation_state() {
        let executor = FakeExecutor::new(vec![
            Ok(output(
                1,
                "requested model is not available for this account",
                "",
            )),
            Ok(output(
                1,
                "requested model is not available for this account",
                "",
            )),
        ]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Codex), &executor);
        assert!(matches!(
            result,
            ProviderProbeExecution::NoRecommendation { .. }
        ));

        let mut state = run_state(&[WizardProviderCode::Codex]);
        assert!(!apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            result,
        ));
        mark_review_ready_if_complete(&mut state);

        let provider = state
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Codex)
            .unwrap();
        assert_eq!(
            provider.status,
            EffortWizardProviderStatus::NoRecommendation
        );
        assert!(provider.recommendation.is_none());
        assert_eq!(state.status, EffortWizardRunStatus::ReviewReady);
        assert_eq!(state.review.as_ref().unwrap().reviewable_provider_count, 0);
        assert_eq!(
            serde_json::to_string(&provider.status).unwrap(),
            "\"no_recommendation\""
        );
    }

    #[test]
    fn review_waits_for_every_selected_provider_and_excludes_no_recommendation() {
        let mut state = run_state(&[WizardProviderCode::Codex, WizardProviderCode::Claude]);
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Error {
                results: Vec::new(),
                reason: "transient".to_string(),
            },
        );
        mark_review_ready_if_complete(&mut state);
        assert_eq!(state.status, EffortWizardRunStatus::Checking);

        apply_provider_execution(
            &mut state,
            WizardProviderCode::Claude,
            ProviderProbeExecution::NoRecommendation {
                results: Vec::new(),
            },
        );
        mark_review_ready_if_complete(&mut state);
        assert_eq!(state.status, EffortWizardRunStatus::ReviewReady);
        assert_eq!(state.review.as_ref().unwrap().reviewable_provider_count, 0);
        assert!(state
            .providers
            .iter()
            .any(|provider| provider.status == EffortWizardProviderStatus::ProbeError));
        assert!(state
            .providers
            .iter()
            .any(|provider| provider.status == EffortWizardProviderStatus::NoRecommendation));
    }

    #[test]
    fn successful_provider_is_reviewable_while_transient_provider_is_not() {
        let mut state = run_state(&[WizardProviderCode::Codex, WizardProviderCode::Claude]);
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Error {
                results: Vec::new(),
                reason: "transient".to_string(),
            },
        );
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Claude,
            ProviderProbeExecution::Recommendation {
                recommendation: recommendation(WizardProviderCode::Claude),
                results: Vec::new(),
            },
        );
        mark_review_ready_if_complete(&mut state);

        let review = state.review.as_ref().unwrap();
        assert_eq!(review.reviewable_provider_count, 1);
        assert_eq!(
            review
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Claude)
                .unwrap()
                .status,
            EffortWizardProviderStatus::ReviewReady
        );
    }

    #[test]
    fn selected_subset_reaches_review_only_after_selected_providers_finish() {
        let selected = [
            WizardProviderCode::Codex,
            WizardProviderCode::Claude,
            WizardProviderCode::Cursor,
        ];
        let mut state = run_state(&selected);

        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Recommendation {
                recommendation: recommendation(WizardProviderCode::Codex),
                results: vec![],
            },
        );
        mark_review_ready_if_complete(&mut state);
        assert_eq!(state.status, EffortWizardRunStatus::Checking);

        apply_provider_execution(
            &mut state,
            WizardProviderCode::Claude,
            ProviderProbeExecution::Error {
                results: vec![],
                reason: "transient auth/network failure".to_string(),
            },
        );
        mark_review_ready_if_complete(&mut state);
        assert_eq!(state.status, EffortWizardRunStatus::Checking);

        apply_provider_execution(
            &mut state,
            WizardProviderCode::Cursor,
            ProviderProbeExecution::NoRecommendation { results: vec![] },
        );
        mark_review_ready_if_complete(&mut state);

        assert_eq!(state.status, EffortWizardRunStatus::ReviewReady);
        assert_eq!(state.selected_providers, selected);
        assert_eq!(state.review.as_ref().unwrap().reviewable_provider_count, 1);
        assert_eq!(
            state
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Claude)
                .unwrap()
                .status,
            EffortWizardProviderStatus::ProbeError
        );
        assert_eq!(
            state
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Cursor)
                .unwrap()
                .status,
            EffortWizardProviderStatus::NoRecommendation
        );

        let skipped = state
            .providers
            .iter()
            .filter(|provider| !selected.contains(&provider.provider))
            .collect::<Vec<_>>();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].status, EffortWizardProviderStatus::Skipped);
        assert!(skipped[0].probe_results.is_empty());
    }

    #[test]
    fn start_dispatches_only_selected_available_providers() {
        let selected = vec![WizardProviderCode::Codex, WizardProviderCode::Cursor];
        let snapshots = BTreeMap::from([
            (
                WizardProviderCode::Codex,
                snapshot(WizardProviderCode::Codex),
            ),
            (
                WizardProviderCode::Claude,
                snapshot(WizardProviderCode::Claude),
            ),
            (
                WizardProviderCode::Cursor,
                snapshot(WizardProviderCode::Cursor),
            ),
        ]);
        let dispatcher = RecordingDispatcher::new();

        dispatch_selected_probe_work(&selected, &snapshots, &dispatcher).unwrap();

        assert_eq!(
            *dispatcher.providers.lock().unwrap(),
            vec![WizardProviderCode::Codex, WizardProviderCode::Cursor]
        );

        let mut state = run_state(&selected);
        state
            .providers
            .iter_mut()
            .find(|provider| provider.provider == WizardProviderCode::Antigravity)
            .unwrap()
            .status = EffortWizardProviderStatus::Unavailable;
        assert_eq!(state.selected_providers, selected);
        assert_eq!(
            state
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Claude)
                .unwrap()
                .status,
            EffortWizardProviderStatus::Skipped
        );
        assert!(state
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Claude)
            .unwrap()
            .probe_results
            .is_empty());
        assert_eq!(
            state
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Antigravity)
                .unwrap()
                .status,
            EffortWizardProviderStatus::Unavailable
        );
        assert!(state
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Antigravity)
            .unwrap()
            .probe_results
            .is_empty());

        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Recommendation {
                recommendation: recommendation(WizardProviderCode::Codex),
                results: vec![],
            },
        );
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Cursor,
            ProviderProbeExecution::NoRecommendation { results: vec![] },
        );
        mark_review_ready_if_complete(&mut state);
        assert_eq!(
            reviewable_provider_set(&state),
            HashSet::from([WizardProviderCode::Codex])
        );
        assert!(validate_apply_decisions(
            &state,
            &[EffortWizardProviderDecision {
                provider: WizardProviderCode::Claude,
                tiers: EffortWizardTierDecisions::default(),
            }]
        )
        .is_err());
    }

    #[test]
    fn start_validation_rejects_checking_and_applying_but_allows_terminal_runs() {
        for status in [
            EffortWizardRunStatus::Checking,
            EffortWizardRunStatus::Applying,
        ] {
            let mut state = run_state(&[WizardProviderCode::Codex]);
            state.status = status;
            let coordinator = EffortWizardCoordinator {
                current_run: Some(EffortWizardRun {
                    state,
                    snapshots: BTreeMap::new(),
                    cancellation: Arc::new(AtomicBool::new(false)),
                }),
            };
            let error = validate_start_against_current_run(&coordinator).unwrap_err();
            assert_eq!(error.code, error_codes::EFFORT_WIZARD_APPLY_INVALID);
        }

        for status in [
            EffortWizardRunStatus::Completed,
            EffortWizardRunStatus::Cancelled,
            EffortWizardRunStatus::FatalError,
        ] {
            let mut state = run_state(&[WizardProviderCode::Codex]);
            state.status = status;
            let coordinator = EffortWizardCoordinator {
                current_run: Some(EffortWizardRun {
                    state,
                    snapshots: BTreeMap::new(),
                    cancellation: Arc::new(AtomicBool::new(false)),
                }),
            };
            assert!(validate_start_against_current_run(&coordinator).is_ok());
        }
    }

    #[test]
    fn apply_validation_rejects_non_reviewable_and_missing_provider_decisions() {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Recommendation {
                recommendation: recommendation(WizardProviderCode::Codex),
                results: vec![],
            },
        );
        mark_review_ready_if_complete(&mut state);
        let tiers = EffortWizardTierDecisions::default();
        let missing = validate_apply_decisions(&state, &[]).unwrap_err();
        assert_eq!(missing.code, error_codes::EFFORT_WIZARD_APPLY_INVALID);

        let non_reviewable = validate_apply_decisions(
            &state,
            &[EffortWizardProviderDecision {
                provider: WizardProviderCode::Claude,
                tiers,
            }],
        )
        .unwrap_err();
        assert_eq!(
            non_reviewable.code,
            error_codes::EFFORT_WIZARD_APPLY_INVALID
        );
    }

    #[test]
    fn apply_patches_use_stored_recommendation_and_snapshot_only() {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        let current = EffortsArgs {
            low: vec!["--model".to_string(), "persisted-model".to_string()],
            ..EffortsArgs::default()
        };
        state
            .providers
            .iter_mut()
            .find(|provider| provider.provider == WizardProviderCode::Codex)
            .unwrap()
            .current_efforts = current.clone();
        let stored_recommendation = recommendation(WizardProviderCode::Codex);
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::Recommendation {
                recommendation: stored_recommendation.clone(),
                results: vec![],
            },
        );
        mark_review_ready_if_complete(&mut state);
        let run = EffortWizardRun {
            state,
            snapshots: BTreeMap::from([(WizardProviderCode::Codex, {
                let mut snapshot = snapshot(WizardProviderCode::Codex);
                snapshot.current_efforts = current.clone();
                snapshot
            })]),
            cancellation: Arc::new(AtomicBool::new(false)),
        };
        let patch = build_apply_patches(
            &run,
            &[EffortWizardProviderDecision {
                provider: WizardProviderCode::Codex,
                tiers: EffortWizardTierDecisions::default(),
            }],
        )
        .unwrap()
        .remove(0);

        assert_eq!(patch.expected_current_efforts, current);
        assert_eq!(patch.confirmed_recommendation, stored_recommendation);
        assert!(patch.can_advance_revision);
    }

    #[test]
    fn failed_apply_restores_review_without_completion_or_in_memory_bootstrap() {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        state.status = EffortWizardRunStatus::Applying;
        let error = PedelecError::new(
            error_codes::EFFORT_WIZARD_SETTINGS_CHANGED,
            "settings changed",
        );
        restore_apply_failure_state(&mut state, &error);

        assert_eq!(state.status, EffortWizardRunStatus::ReviewReady);
        assert!(state.completion.is_none());
        assert!(state.bootstrap.is_none());
        assert_eq!(state.error.as_ref().unwrap().code, error.code);
    }

    #[test]
    fn successful_apply_transition_publishes_refreshed_bootstrap() {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        let bootstrap = EffortWizardBootstrap {
            providers: vec![],
            home_reminder: None,
        };
        let completion = EffortWizardCompletionState {
            providers: vec![],
            confirmed_provider_count: 1,
        };
        transition_after_apply(&mut state, completion.clone(), bootstrap.clone());

        assert_eq!(state.status, EffortWizardRunStatus::Completed);
        assert_eq!(state.completion, Some(completion));
        assert_eq!(state.bootstrap, Some(bootstrap));
        assert!(state.error.is_none());
    }

    #[test]
    fn apply_orchestration_persists_selected_provider_and_refreshes_home_reminder() {
        let coordinator = review_ready_apply_coordinator();
        let backend = FakeApplyBackend::new(apply_bootstrap_fixture(), None);
        let input = codex_apply_input();
        let serialized_input = serde_json::to_value(&input).unwrap();
        assert!(serialized_input["providers"][0].get("args").is_none());
        assert!(serialized_input["providers"][0]
            .get("presetRevision")
            .is_none());

        let emitted = Mutex::new(Vec::<EffortWizardRunStatus>::new());
        let emit = |state: Option<EffortWizardRunState>| {
            if let Some(state) = state {
                emitted.lock().unwrap().push(state.status);
            }
        };
        let output =
            apply_run_with_backend(Arc::clone(&coordinator), &backend, &emit, input).unwrap();

        let patches = backend.patches.lock().unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].providers.len(), 1);
        assert_eq!(patches[0].providers[0].provider, WizardProviderCode::Codex);
        assert_eq!(patches[0].providers[0].preset_revision, 2);
        assert_eq!(
            patches[0].providers[0].expected_current_efforts,
            EffortsArgs::default()
        );
        assert_eq!(
            patches[0].providers[0].confirmed_recommendation,
            recommendation(WizardProviderCode::Codex)
        );
        assert_eq!(
            backend
                .applied_revisions
                .lock()
                .unwrap()
                .get(&WizardProviderCode::Codex),
            Some(&2)
        );
        assert_eq!(
            backend
                .applied_revisions
                .lock()
                .unwrap()
                .get(&WizardProviderCode::Claude),
            Some(&1)
        );
        assert_eq!(*backend.bootstrap_calls.lock().unwrap(), 1);

        let codex_bootstrap = output
            .bootstrap
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Codex)
            .unwrap();
        let claude_bootstrap = output
            .bootstrap
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Claude)
            .unwrap();
        assert!(!codex_bootstrap.preset_update_available);
        assert!(claude_bootstrap.preset_update_available);
        assert_eq!(
            output.bootstrap.home_reminder,
            Some(EffortWizardHomeReminder::PresetUpdate {
                providers: vec![WizardProviderCode::Claude],
            })
        );
        assert_eq!(output.state.status, EffortWizardRunStatus::Completed);
        assert!(output.state.completion.is_some());
        assert_eq!(output.state.bootstrap, Some(output.bootstrap.clone()));
        assert_eq!(
            *emitted.lock().unwrap(),
            vec![
                EffortWizardRunStatus::Applying,
                EffortWizardRunStatus::Completed
            ]
        );

        let state = coordinator
            .lock()
            .unwrap()
            .current_run
            .as_ref()
            .unwrap()
            .state
            .clone();
        assert_eq!(state.status, EffortWizardRunStatus::Completed);
        assert_eq!(state.bootstrap, Some(output.bootstrap));
    }

    fn assert_apply_persistence_failure(error: PedelecError) {
        let coordinator = review_ready_apply_coordinator();
        let backend = FakeApplyBackend::new(apply_bootstrap_fixture(), Some(error.clone()));
        let input = codex_apply_input();
        let emitted = Mutex::new(Vec::<EffortWizardRunStatus>::new());
        let emit = |state: Option<EffortWizardRunState>| {
            if let Some(state) = state {
                emitted.lock().unwrap().push(state.status);
            }
        };
        let result =
            apply_run_with_backend(Arc::clone(&coordinator), &backend, &emit, input.clone())
                .unwrap_err();

        assert_eq!(result, error);
        assert_eq!(backend.patches.lock().unwrap().len(), 1);
        assert_eq!(*backend.bootstrap_calls.lock().unwrap(), 0);
        assert_eq!(
            backend
                .applied_revisions
                .lock()
                .unwrap()
                .get(&WizardProviderCode::Codex),
            Some(&1)
        );
        assert_eq!(
            *emitted.lock().unwrap(),
            vec![
                EffortWizardRunStatus::Applying,
                EffortWizardRunStatus::ReviewReady
            ]
        );

        let state = coordinator
            .lock()
            .unwrap()
            .current_run
            .as_ref()
            .unwrap()
            .state
            .clone();
        assert_eq!(state.status, EffortWizardRunStatus::ReviewReady);
        assert!(state.completion.is_none());
        assert!(state.bootstrap.is_none());
        assert_eq!(state.error.as_ref().unwrap().code, error.code);
        assert!(state
            .providers
            .iter()
            .find(|provider| provider.provider == WizardProviderCode::Codex)
            .unwrap()
            .recommendation
            .is_some());
        assert_eq!(input.providers.len(), 1);
    }

    #[test]
    fn apply_orchestration_restores_review_after_stale_settings_failure() {
        assert_apply_persistence_failure(PedelecError::new(
            error_codes::EFFORT_WIZARD_SETTINGS_CHANGED,
            "settings changed",
        ));
    }

    #[test]
    fn apply_orchestration_restores_review_after_settings_write_failure() {
        assert_apply_persistence_failure(PedelecError::new(
            error_codes::SETTINGS_WRITE_FAILED,
            "settings write failed",
        ));
    }

    #[test]
    fn no_recommendation_completion_is_distinct_from_unavailable() {
        let mut state = run_state(&[WizardProviderCode::Codex]);
        apply_provider_execution(
            &mut state,
            WizardProviderCode::Codex,
            ProviderProbeExecution::NoRecommendation {
                results: Vec::new(),
            },
        );
        mark_review_ready_if_complete(&mut state);
        let coordinator = Arc::new(Mutex::new(EffortWizardCoordinator {
            current_run: Some(EffortWizardRun {
                state,
                snapshots: BTreeMap::new(),
                cancellation: Arc::new(AtomicBool::new(false)),
            }),
        }));

        let completion = build_completion_state(&coordinator, "test-run", &[]).unwrap();
        assert_eq!(completion.confirmed_provider_count, 0);
        assert_eq!(
            completion
                .providers
                .iter()
                .find(|provider| provider.provider == WizardProviderCode::Codex)
                .unwrap()
                .status,
            EffortWizardCompletionStatus::NoRecommendation
        );
    }

    #[test]
    fn supported_then_baseline_transient_discards_tentative_coverage() {
        let executor = FakeExecutor::new(vec![
            Ok(output(0, "accepted", "")),
            Ok(output(1, "unexpected failure", "")),
        ]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Cursor), &executor);
        assert!(matches!(result, ProviderProbeExecution::Error { .. }));
    }

    #[test]
    fn classifier_requires_model_context_and_prioritizes_auth_errors() {
        assert!(matches!(
            classify_probe_failure(WizardProviderCode::Codex, Some(1), "access denied", ""),
            ProbeExecutionResult::TransientError { .. }
        ));
        assert!(matches!(
            classify_probe_failure(
                WizardProviderCode::Codex,
                Some(1),
                "401 unauthorized; model not available",
                ""
            ),
            ProbeExecutionResult::TransientError { .. }
        ));
        assert!(matches!(
            classify_probe_failure(
                WizardProviderCode::Codex,
                Some(1),
                "requested model is not available",
                ""
            ),
            ProbeExecutionResult::NotEntitled { .. }
        ));
        assert!(matches!(
            classify_probe_failure(
                WizardProviderCode::Antigravity,
                Some(1),
                "malformed json",
                ""
            ),
            ProbeExecutionResult::TransientError { .. }
        ));
    }

    #[test]
    fn every_supported_provider_uses_conservative_entitlement_matching() {
        for provider in WizardProviderCode::all() {
            assert!(matches!(
                classify_probe_failure(
                    *provider,
                    Some(1),
                    "requested model is not available to this account",
                    ""
                ),
                ProbeExecutionResult::NotEntitled { .. }
            ));
            assert!(matches!(
                classify_probe_failure(*provider, Some(1), "401 unauthorized", ""),
                ProbeExecutionResult::TransientError { .. }
            ));
        }
    }

    #[test]
    fn capture_overflow_is_transient_and_does_not_produce_recommendation() {
        let mut captured = output(0, "accepted", "");
        captured.stdout_truncated = true;
        let executor = FakeExecutor::new(vec![Ok(captured)]);
        let result =
            execute_provider_probe_with_executor(&snapshot(WizardProviderCode::Codex), &executor);
        assert!(matches!(result, ProviderProbeExecution::Error { .. }));
    }

    #[test]
    fn probe_command_uses_exact_executable_and_isolated_workspace() {
        let snapshot = snapshot(WizardProviderCode::Codex);
        let probe = snapshot.plan.probe_definition("premium").unwrap();
        let command = build_probe_command(
            WizardProviderCode::Codex,
            &snapshot.executable,
            &snapshot.workspace,
            probe,
        );
        assert_eq!(command.program, path_for_external_use(&snapshot.executable));
        assert_eq!(command.cwd, snapshot.workspace);
        assert!(command.args.contains(&"read-only".to_string()));
        assert_eq!(command.stdin, PROBE_PROMPT);
        assert!(!command.args.iter().any(|arg| arg.contains("pedelec")));
    }
}
