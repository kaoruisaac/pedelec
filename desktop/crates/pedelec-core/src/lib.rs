use chrono::{DateTime, Utc};
use pedelec_shared::paths::path_for_external_use;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;

pub mod effort_wizard;
pub use effort_wizard::*;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_WORKSPACE_RUN_TIMEOUT_MS: u64 = 60_000;
// Keep conservative headroom for the Core IPC response envelope. The exact
// response-size check belongs to the transport layer, but this guard prevents
// Core traversal from accumulating an unbounded result before it gets there.
const WORKSPACE_LIST_EARLY_LIMIT_BYTES: usize = 900 * 1024;
const DEFAULT_MAX_SKILL_SIZE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_TIMEOUT_MS: u64 = 120_000;
const OLLAMA_CONNECTION_CHECK_TIMEOUT_MS: u64 = 3_000;
const CODEX_SKILLS_INCLUDE_INSTRUCTIONS_KEY: &str = "skills.include_instructions";
const CODEX_PROJECT_DOC_MAX_BYTES_KEY: &str = "project_doc_max_bytes";
const CODEX_INCLUDE_PERMISSIONS_INSTRUCTIONS_KEY: &str = "include_permissions_instructions";
const CODEX_INCLUDE_APPS_INSTRUCTIONS_KEY: &str = "include_apps_instructions";
const CODEX_INCLUDE_COLLABORATION_MODE_INSTRUCTIONS_KEY: &str =
    "include_collaboration_mode_instructions";
const CODEX_FEATURES_PLUGINS_KEY: &str = "features.plugins";
const CODEX_FEATURES_APPS_KEY: &str = "features.apps";
const PEDELEC_ANTIGRAVITY_AGENT_DIR: &str = ".agents/agents/pedelec-runtime";
const PEDELEC_ANTIGRAVITY_AGENT_FILE: &str = "agent.md";
pub const PEDELEC_RUNTIME_DATA_DIR: &str = ".pedelec-runtime";
pub const PEDELEC_WORKSPACE_FILE: &str = ".pedelec-workspace.json";
const TOOL_TIMEOUT_OVERRIDE_FIELD: &str = "timeoutMs";
pub const TOOL_RESULT_REPLAY_WINDOW: Duration = Duration::from_secs(10);
pub const TOOL_RESULT_REPLAY_MAX_ENTRIES: usize = 256;
const THREAD_ID_BASE36_MIN_WIDTH: usize = 6;
const THREAD_ID_BASE36_MAX_WIDTH: usize = 7;
const THREAD_ID_MAX_COUNTER: u64 = 78_364_164_095;
pub const MAX_ASSET_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
const ASSET_UPLOAD_TICKET_SECONDS: i64 = 5 * 60;
const WORKSPACE_REMOVE_MAX_ATTEMPTS: usize = 10;
const WORKSPACE_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(50);
const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROVIDER_PROBE_MAX_OUTPUT_BYTES: u64 = 64 * 1024;

/// Returns the root of Pedelec-owned runtime data inside a session workspace.
pub fn workspace_runtime_data_root(workspace_path: &Path) -> PathBuf {
    workspace_path.join(PEDELEC_RUNTIME_DATA_DIR)
}

/// Returns the physical root used for App/Agent-shared assets.
pub fn workspace_assets_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("assets")
}

/// Returns the legacy workspace-global skills root for callers inspecting
/// existing data.
///
/// This path is retained as a legacy filesystem helper for callers that need
/// to inspect an existing workspace.  Core no longer creates or reads this
/// workspace-global directory; generated skills are stored below
/// [`thread_skills_root`].
pub fn workspace_skills_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("skills")
}

/// Returns the runtime root containing private state for all threads in a
/// workspace.
pub fn workspace_threads_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("threads")
}

/// Returns the private runtime root for one thread.
pub fn workspace_thread_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_threads_root(workspace_path).join(thread_id)
}

/// Returns the generated skill/tool-spec directory for one thread.
pub fn thread_skills_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_thread_root(workspace_path, thread_id).join("skills")
}

/// Returns the physical root used for thread/session event logs.
pub fn workspace_logs_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("logs")
}

/// Returns the physical root used for upload temporary files.
pub fn workspace_tmp_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("tmp")
}

/// Returns the private root used for thread-scoped Deno module snapshots.
pub fn workspace_deno_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("deno")
}

/// Returns the private root used for thread-scoped Deno module snapshots.
pub fn workspace_deno_threads_root(workspace_path: &Path) -> PathBuf {
    workspace_deno_root(workspace_path).join("threads")
}

/// Returns the private root for Workspace-run Deno Module packages. This is
/// deliberately separate from the Thread snapshot tree.
pub fn workspace_deno_workspace_root(workspace_path: &Path) -> PathBuf {
    workspace_deno_root(workspace_path).join("workspace")
}

/// Returns the private package scope for one Workspace + SDK-origin runtime
/// capability. `scope_id` is generated by Core and is never a raw origin.
pub fn workspace_deno_workspace_scope_root(workspace_path: &Path, scope_id: &str) -> PathBuf {
    workspace_deno_workspace_root(workspace_path).join(scope_id)
}

pub fn workspace_deno_workspace_modules_root(workspace_path: &Path, scope_id: &str) -> PathBuf {
    workspace_deno_workspace_scope_root(workspace_path, scope_id).join("modules")
}

pub fn workspace_deno_workspace_run_root(
    workspace_path: &Path,
    scope_id: &str,
    run_id: &str,
) -> PathBuf {
    workspace_deno_workspace_scope_root(workspace_path, scope_id)
        .join("runs")
        .join(run_id)
}

pub fn workspace_deno_workspace_import_map_path(
    workspace_path: &Path,
    scope_id: &str,
    run_id: &str,
) -> PathBuf {
    workspace_deno_workspace_run_root(workspace_path, scope_id, run_id).join("import-map.json")
}

/// Returns the private Deno state root for one thread.
pub fn workspace_deno_thread_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_threads_root(workspace_path).join(thread_id)
}

/// Alias using the shorter helper naming used by the Deno runtime contract.
pub fn thread_deno_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_thread_root(workspace_path, thread_id)
}

/// Returns the package root containing the materialized Deno modules for one
/// thread.  The package names below this directory are validated before use.
pub fn workspace_deno_modules_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_thread_root(workspace_path, thread_id).join("modules")
}

/// Alias using the shorter helper naming used by the Deno runtime contract.
pub fn thread_deno_modules_root(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_modules_root(workspace_path, thread_id)
}

/// Returns the Pedelec-owned import map path for one thread.
pub fn workspace_deno_import_map_path(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_thread_root(workspace_path, thread_id).join("import-map.json")
}

/// Returns whether a custom workspace already has a Pedelec-owned Deno
/// thread-runtime root for `thread_id`.  Arbitrary user files are not part of
/// thread-ID reservation; only this private runtime path is.
pub fn workspace_deno_thread_root_occupied(workspace_path: &Path, thread_id: &str) -> bool {
    fs::symlink_metadata(workspace_deno_thread_root(workspace_path, thread_id)).is_ok()
}

/// Alias using the shorter helper naming used by the Deno runtime contract.
pub fn thread_deno_import_map_path(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_deno_import_map_path(workspace_path, thread_id)
}

/// Returns the workspace marker path.
pub fn workspace_metadata_path(workspace_path: &Path) -> PathBuf {
    workspace_path.join(PEDELEC_WORKSPACE_FILE)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PedelecError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAssetUploadInput {
    pub thread_id: String,
    #[serde(default)]
    pub target_path: Option<String>,
    pub filename: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAssetUploadOutput {
    pub upload_id: String,
    pub upload_url: String,
    pub token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAssetDownloadInput {
    pub thread_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAssetDownloadOutput {
    pub download_id: String,
    pub download_url: String,
    pub token: String,
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: i64,
    pub mime_type: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListAssetsInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListAssetsOutput {
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct AssetUploadTicket {
    pub thread_id: String,
    pub workspace_path: PathBuf,
    pub public_path: String,
    pub relative_path: PathBuf,
    pub filename: String,
    pub safe_filename: String,
    pub expected_size_bytes: u64,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub state: AssetUploadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetUploadState {
    Pending,
    Uploading,
    Completed,
    Failed,
    Expired,
}

#[derive(Debug, Clone)]
pub struct AssetDownloadTicket {
    pub thread_id: String,
    pub workspace_path: PathBuf,
    pub public_path: String,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub state: AssetDownloadState,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateDenoModuleUploadInput {
    pub thread_id: String,
    pub module_name: String,
    pub expected_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareWorkspaceDenoModulesInput {
    pub workspace_id: String,
    pub module_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareWorkspaceDenoModulesOutput {
    pub missing_module_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceDenoModuleUploadInput {
    pub workspace_id: String,
    pub module_name: String,
    pub expected_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateDenoModuleUploadOutput {
    pub upload_id: String,
    pub upload_url: String,
    pub token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenoModuleUploadCompletion {
    pub module_name: String,
    pub ready: bool,
}

#[derive(Debug, Clone)]
pub struct DenoModuleUploadTicket {
    pub owner: DenoModuleUploadOwner,
    pub thread_id: String,
    pub workspace_path: PathBuf,
    pub module_name: String,
    pub expected_size_bytes: u64,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub state: DenoModuleUploadState,
}

#[derive(Debug, Clone)]
pub enum DenoModuleUploadOwner {
    Thread {
        thread_id: String,
    },
    Workspace {
        workspace_id: String,
        sdk_origin: String,
        scope_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoModuleUploadState {
    Pending,
    Uploading,
    Completed,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadDenoModuleInput {
    pub name: String,
    pub description: String,
    pub usage: String,
    #[serde(default)]
    pub prefer_stdin_execution: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DenoModuleSetupState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenoModuleState {
    pub name: String,
    pub description: String,
    pub usage: String,
    #[serde(default)]
    pub prefer_stdin_execution: bool,
    pub state: DenoModuleSetupState,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct WorkspaceDenoModuleScopeKey {
    pub workspace_id: String,
    pub sdk_origin: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceDenoModuleScopeState {
    pub scope_id: String,
    pub modules: HashMap<String, DenoModuleSetupState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AbortSessionSetupInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DenoModuleArtifactEnvelope {
    version: u8,
    format: String,
    runtime_source: String,
    types_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DenoModuleImportMap {
    imports: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetDownloadState {
    Pending,
    Downloading,
    Completed,
    Failed,
    Expired,
}

impl PedelecError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Some(details),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceKind {
    Managed,
    Custom,
}

/// Core-owned capability and filesystem identity for one Workspace.
///
/// The canonical path is intentionally kept in this resource rather than in
/// [`ThreadState`].  Browser-facing protocol responses should use the opaque
/// `workspace_id` and must not serialize this state directly for managed
/// workspaces.
#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub workspace_id: String,
    pub canonical_path: PathBuf,
    pub kind: WorkspaceKind,
    authorized_sdk_origins: HashSet<String>,
}

impl WorkspaceState {
    fn new(workspace_id: String, canonical_path: PathBuf, kind: WorkspaceKind) -> Self {
        Self {
            workspace_id: workspace_id.clone(),
            canonical_path,
            kind,
            authorized_sdk_origins: HashSet::new(),
        }
    }

    pub fn is_origin_authorized(&self, origin: &str) -> bool {
        self.authorized_sdk_origins.contains(origin)
    }

    pub fn authorized_origins(&self) -> impl Iterator<Item = &String> {
        self.authorized_sdk_origins.iter()
    }
}

/// Runtime registry of Workspace capabilities.  Custom Workspaces are
/// deduplicated by canonical physical path so repeated opens coordinate on
/// one resource and one thread membership domain.
#[derive(Debug, Default)]
pub struct WorkspaceRegistry {
    workspaces: HashMap<String, WorkspaceState>,
    by_canonical_identity: HashMap<String, String>,
}

impl WorkspaceRegistry {
    fn insert(&mut self, state: WorkspaceState) -> Result<(), PedelecError> {
        let identity = workspace_canonical_identity(&state.canonical_path);
        if self.workspaces.contains_key(&state.workspace_id)
            || self.by_canonical_identity.contains_key(&identity)
        {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_CREATE_FAILED,
                "workspace resource already exists",
            ));
        }
        self.by_canonical_identity
            .insert(identity, state.workspace_id.clone());
        self.workspaces.insert(state.workspace_id.clone(), state);
        Ok(())
    }

    fn insert_or_get_custom(
        &mut self,
        workspace_id: String,
        canonical_path: PathBuf,
    ) -> Result<(String, bool), PedelecError> {
        let identity = workspace_canonical_identity(&canonical_path);
        if let Some(existing_id) = self.by_canonical_identity.get(&identity) {
            return Ok((existing_id.clone(), false));
        }

        self.insert(WorkspaceState::new(
            workspace_id.clone(),
            canonical_path,
            WorkspaceKind::Custom,
        ))?;
        Ok((workspace_id, true))
    }

    fn workspace(&self, workspace_id: &str) -> Result<&WorkspaceState, PedelecError> {
        self.workspaces.get(workspace_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_NOT_FOUND,
                "workspace was not found",
                serde_json::json!({ "workspaceId": workspace_id }),
            )
        })
    }

    fn workspace_mut(&mut self, workspace_id: &str) -> Result<&mut WorkspaceState, PedelecError> {
        self.workspaces.get_mut(workspace_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_NOT_FOUND,
                "workspace was not found",
                serde_json::json!({ "workspaceId": workspace_id }),
            )
        })
    }

    fn remove(&mut self, workspace_id: &str) -> Option<WorkspaceState> {
        let state = self.workspaces.remove(workspace_id)?;
        self.by_canonical_identity
            .remove(&workspace_canonical_identity(&state.canonical_path));
        Some(state)
    }

    fn clear(&mut self) {
        self.workspaces.clear();
        self.by_canonical_identity.clear();
    }
}

fn workspace_canonical_identity(path: &Path) -> String {
    let value = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        value.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadState {
    pub thread_id: String,
    pub workspace_id: String,
    pub provider: ProviderCode,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    pub effort_args: Vec<String>,
    pub skills: Vec<SkillFile>,
    pub status: ThreadStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing)]
    pub sdk_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionState {
    pub provider_session_id: Option<String>,
    pub active_provider_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    Starting,
    Running,
    WaitingToolResult,
    Stopping,
    Ended,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThreadOperationKind {
    User,
    Prepare,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveOperationSnapshot {
    pub operation_id: String,
    pub operation_kind: ThreadOperationKind,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompletedOperationSnapshot {
    pub operation_id: String,
    pub operation_kind: ThreadOperationKind,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderCode {
    Codex,
    Antigravity,
    OpenCode,
    Cursor,
    Claude,
    Ollama,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Default,
    Low,
    High,
}

/// Reasoning values accepted by the Codex App Server protocol. This is a
/// typed representation of the persisted `-c model_reasoning_effort=...`
/// setting; raw CLI fragments must not cross the persistent-runtime boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl CodexReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Native reasoning values accepted by Antigravity's persistent stream
/// transport. Persisted effort tiers are parsed into this semantic value
/// before crossing the provider-runtime boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AntigravityReasoningEffort {
    Low,
    Medium,
    High,
}

impl AntigravityReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Native reasoning values accepted by Claude Code's persistent stream
/// transport. Persisted `--effort` settings are parsed into this semantic
/// value before crossing the provider-runtime boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ClaudeReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistentApprovalPolicy {
    Never,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistentSandboxPolicy {
    ReadOnly,
}

impl Default for EffortLevel {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortsArgs {
    #[serde(default)]
    pub default: Vec<String>,
    #[serde(default)]
    pub low: Vec<String>,
    #[serde(default)]
    pub high: Vec<String>,
}

impl Default for EffortsArgs {
    fn default() -> Self {
        Self {
            default: Vec::new(),
            low: Vec::new(),
            high: Vec::new(),
        }
    }
}

impl EffortsArgs {
    fn get(&self, level: EffortLevel) -> &[String] {
        match level {
            EffortLevel::Default => &self.default,
            EffortLevel::Low => &self.low,
            EffortLevel::High => &self.high,
        }
    }
}

/// The responsibility domain for a thread error. The tagged representation
/// prevents provider errors from being serialized without their provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum ThreadErrorSource {
    Core,
    Provider { provider: ProviderCode },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub name: String,
    pub code: ProviderCode,
    pub scanned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub path: Option<String>,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PedelecSettings {
    pub default_provider: Option<ProviderCode>,
    pub provider_settings: ProviderSettings,
    #[serde(default)]
    pub wizard_metadata: EffortWizardMetadata,
}

/// The settings contract exposed through the SDK/Core IPC boundary. Keep this
/// deliberately separate from `PedelecSettings`, which is used by the desktop
/// application and contains provider credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SdkSettings {
    pub default_provider: Option<ProviderCode>,
}

impl From<PedelecSettings> for SdkSettings {
    fn from(settings: PedelecSettings) -> Self {
        Self {
            default_provider: settings.default_provider,
        }
    }
}

/// The provider contract exposed through the SDK/Core IPC boundary. Desktop
/// callers retain `ProviderInfo` and its diagnostic metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SdkProviderInfo {
    pub name: String,
    pub code: ProviderCode,
    pub available: bool,
    pub is_default: bool,
    pub error: Option<String>,
}

impl SdkProviderInfo {
    fn from_provider(provider: ProviderInfo, default_provider: Option<&ProviderCode>) -> Self {
        let is_default = default_provider == Some(&provider.code);
        Self {
            name: provider.name,
            code: provider.code,
            available: provider.available,
            is_default,
            error: provider.error,
        }
    }
}

impl Default for PedelecSettings {
    fn default() -> Self {
        Self {
            default_provider: None,
            provider_settings: ProviderSettings::default(),
            wizard_metadata: EffortWizardMetadata::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsInput {
    pub default_provider: ProviderCode,
    pub provider_settings: ProviderSettingsInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettings {
    #[serde(default)]
    pub codex: CommonProviderSettings,
    #[serde(default)]
    pub antigravity: CommonProviderSettings,
    #[serde(default)]
    pub opencode: CommonProviderSettings,
    #[serde(default)]
    pub cursor: CommonProviderSettings,
    #[serde(default)]
    pub claude: CommonProviderSettings,
    #[serde(default)]
    pub ollama: OllamaProviderSettings,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            codex: CommonProviderSettings::default(),
            antigravity: CommonProviderSettings::default(),
            opencode: CommonProviderSettings::default(),
            cursor: CommonProviderSettings::default(),
            claude: CommonProviderSettings::default(),
            ollama: OllamaProviderSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommonProviderSettings {
    #[serde(default)]
    pub efforts_args: EffortsArgs,
}

impl Default for CommonProviderSettings {
    fn default() -> Self {
        Self {
            efforts_args: EffortsArgs::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OllamaProviderSettings {
    pub base_url: String,
    pub timeout_ms: u64,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub tavily_api_key: String,
    #[serde(default)]
    pub efforts_args: EffortsArgs,
}

impl Default for OllamaProviderSettings {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            timeout_ms: DEFAULT_OLLAMA_TIMEOUT_MS,
            api_key: String::new(),
            tavily_api_key: String::new(),
            efforts_args: EffortsArgs::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettingsInput {
    #[serde(default)]
    pub codex: CommonProviderSettingsInput,
    #[serde(default)]
    pub antigravity: CommonProviderSettingsInput,
    #[serde(default)]
    pub opencode: CommonProviderSettingsInput,
    #[serde(default)]
    pub cursor: CommonProviderSettingsInput,
    #[serde(default)]
    pub claude: CommonProviderSettingsInput,
    #[serde(default)]
    pub ollama: OllamaProviderSettingsInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommonProviderSettingsInput {
    #[serde(default)]
    pub efforts_args: EffortsArgs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct OllamaProviderSettingsInput {
    pub base_url: Option<String>,
    pub timeout_ms: Option<u64>,
    pub api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    #[serde(default)]
    pub efforts_args: EffortsArgs,
}

impl Default for ProviderSettingsInput {
    fn default() -> Self {
        Self {
            codex: CommonProviderSettingsInput::default(),
            antigravity: CommonProviderSettingsInput::default(),
            opencode: CommonProviderSettingsInput::default(),
            cursor: CommonProviderSettingsInput::default(),
            claude: CommonProviderSettingsInput::default(),
            ollama: OllamaProviderSettingsInput {
                base_url: Some(DEFAULT_OLLAMA_BASE_URL.to_string()),
                timeout_ms: Some(DEFAULT_OLLAMA_TIMEOUT_MS),
                api_key: Some("ollama".to_string()),
                tavily_api_key: Some(String::new()),
                efforts_args: EffortsArgs::default(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListOllamaModelsInput {
    pub base_url: Option<String>,
    pub timeout_ms: Option<u64>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckOllamaConnectionInput {
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckOllamaConnectionOutput {
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OllamaModelOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillFile {
    pub original_url: String,
    pub original_filename: String,
    pub local_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ThreadEvent {
    Created {
        seq: u64,
        thread_id: String,
    },
    StatusChanged {
        seq: u64,
        thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        status: ThreadStatus,
    },
    AssistantDelta {
        seq: u64,
        thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        text: String,
    },
    AssistantMessage {
        seq: u64,
        thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        text: String,
    },
    ToolCall {
        seq: u64,
        thread_id: String,
        operation_id: String,
        request_id: String,
        tool_name: String,
        args: Value,
    },
    ToolResult {
        seq: u64,
        thread_id: String,
        operation_id: String,
        request_id: String,
        tool_name: String,
        result: Value,
    },
    ProviderSessionIdUpdated {
        seq: u64,
        thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        provider_session_id: String,
    },
    UsageUpdated {
        seq: u64,
        thread_id: String,
        total_tokens: u64,
    },
    OperationCompleted {
        seq: u64,
        thread_id: String,
        operation_id: String,
        operation_kind: ThreadOperationKind,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<PedelecError>,
    },
    Error {
        seq: u64,
        thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
        #[serde(flatten)]
        source: ThreadErrorSource,
        error: PedelecError,
    },
    Ended {
        seq: u64,
        thread_id: String,
    },
}

impl ThreadEvent {
    pub fn seq(&self) -> u64 {
        match self {
            ThreadEvent::Created { seq, .. }
            | ThreadEvent::StatusChanged { seq, .. }
            | ThreadEvent::AssistantDelta { seq, .. }
            | ThreadEvent::AssistantMessage { seq, .. }
            | ThreadEvent::ToolCall { seq, .. }
            | ThreadEvent::ToolResult { seq, .. }
            | ThreadEvent::ProviderSessionIdUpdated { seq, .. }
            | ThreadEvent::UsageUpdated { seq, .. }
            | ThreadEvent::OperationCompleted { seq, .. }
            | ThreadEvent::Error { seq, .. }
            | ThreadEvent::Ended { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadInput {
    pub provider: ProviderCode,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub skills: Option<CreateThreadSkillsInput>,
    /// An existing Core Workspace capability.  `None` asks Core to create a
    /// new managed Workspace before creating the Thread.
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadSkillsInput {
    pub guidance: String,
    pub tools: Vec<CreateThreadToolInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deno_modules: Vec<CreateThreadDenoModuleInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadToolInput {
    pub name: String,
    pub description: String,
    pub args_schema: Value,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadOutput {
    pub thread_id: String,
    pub workspace_id: String,
    /// Custom Workspace paths are safe to expose to the SDK after Core has
    /// validated them. Managed Workspace paths remain intentionally opaque.
    pub workspace_path: Option<String>,
    pub explicit_model_config_applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OpenWorkspaceInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OpenWorkspaceOutput {
    pub workspace_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendTextInput {
    pub thread_id: String,
    pub message: String,
    #[serde(default)]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendTextOutput {
    pub thread_id: String,
    pub operation_id: String,
}

/// Public helper input for the local `pedelec-deno` command.  This type is
/// deliberately kept in Core rather than the browser SDK: a Deno run is an
/// internal provider/runtime operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DenoRunTarget {
    WorkspaceFile { entrypoint: String },
    StdinSource { source: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenoRunInput {
    pub thread_id: String,
    pub target: DenoRunTarget,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListInput {
    pub workspace_id: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListOutput {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRunInput {
    pub workspace_id: String,
    pub script: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub deno_modules: Vec<String>,
}

/// Bounded result returned by one raw Deno child process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenoRunOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DenoExecutionTarget {
    WorkspaceFile { entrypoint: PathBuf },
    StdinSource { source: String },
}

/// Ownership identity for one raw Deno process. Agent executions are
/// thread-scoped; Workspace executions are independent run identities and do
/// not borrow a Thread or its Deno Module snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DenoExecutionOwner {
    Thread {
        thread_id: String,
    },
    Workspace {
        workspace_id: String,
        run_id: String,
    },
}

/// The validated, authoritative execution intent passed from Core to the
/// Desktop-owned runtime. `workspace_path` always comes from an authoritative
/// Core Workspace resource, while file entrypoints are resolved by Core before
/// dispatch. `thread_id` remains populated for Agent executions for existing
/// diagnostics; Workspace executions use an empty value because no Thread
/// owns them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenoExecutionIntent {
    pub thread_id: String,
    pub owner: DenoExecutionOwner,
    pub workspace_path: PathBuf,
    pub target: DenoExecutionTarget,
    pub args: Vec<String>,
    /// Core-resolved timeout for this execution. Zero is reserved for legacy
    /// direct runtime tests; all production Core admission paths provide a
    /// positive value.
    #[serde(default)]
    pub timeout_ms: u64,
    /// Core-owned import map for a ready Deno Module snapshot.  This is never
    /// sourced from the Agent CLI request; threads without registered modules
    /// keep the existing `None` behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_map_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRunStart {
    pub workspace_id: String,
    pub run_id: String,
    pub intent: DenoExecutionIntent,
}

/// Runtime seam used by Core IPC.  The default lifecycle methods keep test
/// and non-Desktop callers source-compatible while allowing Desktop to cancel
/// Deno children during thread end and application shutdown.
pub trait DenoRuntimeDispatcher: Send + Sync + 'static {
    fn dispatch(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError>;

    fn cancel_thread(&self, _thread_id: &str) {}

    fn shutdown(&self) {}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareThreadInput {
    pub thread_id: String,
    #[serde(default)]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareThreadOutput {
    pub thread_id: String,
    pub operation_id: String,
    pub prepared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_prepared: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EndThreadInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResumeThreadInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubmitToolResultInput {
    pub thread_id: String,
    pub request_id: String,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInput {
    pub thread_id: String,
    pub tool_name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolInvocationOutcome {
    Result(Value),
    CoreError(PedelecError),
}

#[derive(Debug)]
pub struct ToolInvocationWait {
    pub request_id: String,
    pub timeout_ms: u64,
    pub remaining_timeout: Duration,
    pub result_rx: mpsc::Receiver<ToolInvocationOutcome>,
}

#[derive(Debug)]
pub enum ToolInvocationRegistration {
    Created(ToolInvocationWait),
    Joined(ToolInvocationWait),
    Replayed(ToolInvocationWait),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpecInput {
    pub thread_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeThreadInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub thread_id: String,
    /// Internal transport identity used by SDK resume hydration.  It is not
    /// part of the public Session event context.
    pub workspace_id: String,
    pub status: ThreadStatus,
    pub latest_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<ActiveOperationSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_completed_operation: Option<CompletedOperationSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_tool_request: Option<PendingToolRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    pub total_tokens: u64,
}

#[derive(Debug)]
pub struct ThreadSubscription {
    pub events: mpsc::Receiver<ThreadEvent>,
    pub snapshot: ThreadSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResumeThreadOutput {
    pub snapshot: ThreadSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolRequest {
    pub request_id: String,
    pub thread_id: String,
    pub operation_id: String,
    pub tool_name: String,
    pub args: Value,
    pub created_at: DateTime<Utc>,
    pub timeout_ms: u64,
}

/// A generic process specification. Thread send/prepare/end execution is
/// persistent-only and never constructs one of these; it remains for the
/// Effort Wizard captured probe runner and other one-shot command needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub prompt: String,
    pub stdin: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderExecutionOperationKind {
    UserTurn,
    Prepare,
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentProviderSessionIntent {
    pub thread_id: String,
    pub provider: ProviderCode,
    pub provider_session_id: Option<String>,
    pub workspace_path: PathBuf,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    pub model: Option<String>,
    pub reasoning_effort: Option<CodexReasoningEffort>,
    #[serde(default)]
    pub antigravity_reasoning_effort: Option<AntigravityReasoningEffort>,
    #[serde(default)]
    pub claude_reasoning_effort: Option<ClaudeReasoningEffort>,
    pub approval_policy: PersistentApprovalPolicy,
    pub sandbox_policy: PersistentSandboxPolicy,
    pub host_instructions: String,
    pub config: HashMap<String, Value>,
    pub core_ipc_runtime_file_path: PathBuf,
    pub tools: Vec<ToolDefinition>,
    pub guidance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentProviderTurnIntent {
    pub thread_id: String,
    pub local_turn_id: String,
    pub provider_session_id: Option<String>,
    pub message: String,
    pub session: PersistentProviderSessionIntent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentProviderEndIntent {
    pub thread_id: String,
    pub provider: ProviderCode,
    pub provider_session_id: Option<String>,
    pub active_provider_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PersistentRuntimeOperation {
    EnsureSession {
        session: PersistentProviderSessionIntent,
    },
    StartTurn {
        turn: PersistentProviderTurnIntent,
    },
    EndSession {
        session: PersistentProviderEndIntent,
    },
}

impl PersistentRuntimeOperation {
    pub fn provider(&self) -> &ProviderCode {
        match self {
            Self::EnsureSession { session } => &session.provider,
            Self::StartTurn { turn } => &turn.session.provider,
            Self::EndSession { session } => &session.provider,
        }
    }

    pub fn kind(&self) -> ProviderExecutionOperationKind {
        match self {
            Self::EnsureSession { .. } => ProviderExecutionOperationKind::Prepare,
            Self::StartTurn { .. } => ProviderExecutionOperationKind::UserTurn,
            Self::EndSession { .. } => ProviderExecutionOperationKind::End,
        }
    }

    pub fn thread_id(&self) -> &str {
        match self {
            Self::EnsureSession { session } => &session.thread_id,
            Self::StartTurn { turn } => &turn.thread_id,
            Self::EndSession { session } => &session.thread_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderExecutionStart {
    pub output: SendTextOutput,
    pub intent: PersistentRuntimeOperation,
}

#[derive(Debug, Clone)]
pub struct PrepareExecutionStart {
    pub output: PrepareThreadOutput,
    pub intent: Option<PersistentRuntimeOperation>,
}

#[derive(Debug, Clone)]
pub struct EndThreadStart {
    pub thread_id: String,
    pub execution: PersistentRuntimeOperation,
}

/// Protocol-neutral events emitted by a persistent provider runtime.
/// Provider-specific JSON-RPC method names must not cross this boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderRuntimeEvent {
    SessionReady {
        thread_id: String,
        provider_session_id: String,
    },
    TurnStarted {
        thread_id: String,
        provider_turn_id: String,
    },
    AssistantDelta {
        thread_id: String,
        provider_turn_id: Option<String>,
        text: String,
    },
    AssistantMessage {
        thread_id: String,
        provider_turn_id: Option<String>,
        text: String,
    },
    UsageUpdated {
        thread_id: String,
        provider_turn_id: Option<String>,
        usage: Value,
    },
    TurnCompleted {
        thread_id: String,
        provider_turn_id: Option<String>,
        success: bool,
        error: Option<PedelecError>,
    },
    ProviderError {
        thread_id: String,
        provider_turn_id: Option<String>,
        error: PedelecError,
    },
    RuntimeDisconnected {
        thread_id: Option<String>,
        error: PedelecError,
    },
}

/// Desktop-only raw provider protocol traffic for persistent runtimes. This
/// provider-neutral observability stream carries bare RPC, JSON-RPC, and
/// non-RPC event protocols without promoting them into `ThreadEvent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProtocolTraffic {
    #[serde(rename = "type")]
    pub event_type: String,
    pub provider: ProviderCode,
    pub runtime_generation: u64,
    pub process_id: u32,
    pub thread_id: Option<String>,
    pub ts: String,
    pub direction: String,
    pub kind: String,
    pub message: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<bool>,
}

/// Desktop-only diagnostics for a provider runtime. These events deliberately
/// live outside `ThreadEvent`: a shared App Server has process-lifetime state
/// and process-global diagnostics must not be attributed to an arbitrary
/// Pedelec thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProviderRuntimeDiagnostic {
    ProviderRuntimeStarted {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
    },
    ProviderRuntimeStopped {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        reason: String,
    },
    ProviderRuntimeDisconnected {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        thread_id: Option<String>,
        provider_thread_id: Option<String>,
        reason: String,
    },
    ProviderRuntimeAttached {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        thread_id: String,
        provider_thread_id: String,
        resumed: bool,
    },
    ProviderRuntimeTurnStarted {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        thread_id: String,
        provider_thread_id: String,
        provider_turn_id: String,
    },
    ProviderRuntimeTurnCompleted {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        thread_id: String,
        provider_thread_id: String,
        provider_turn_id: Option<String>,
        status: String,
    },
    ProviderRuntimeStderr {
        provider: ProviderCode,
        runtime_generation: u64,
        process_id: u32,
        text: String,
    },
    ProviderRuntimeError {
        provider: ProviderCode,
        runtime_generation: Option<u64>,
        process_id: Option<u32>,
        thread_id: Option<String>,
        provider_thread_id: Option<String>,
        provider_turn_id: Option<String>,
        code: String,
        message: String,
        details: Option<Value>,
    },
}

impl ProviderRuntimeDiagnostic {
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::ProviderRuntimeDisconnected { thread_id, .. }
            | Self::ProviderRuntimeError { thread_id, .. } => thread_id.as_deref(),
            Self::ProviderRuntimeAttached { thread_id, .. }
            | Self::ProviderRuntimeTurnStarted { thread_id, .. }
            | Self::ProviderRuntimeTurnCompleted { thread_id, .. } => Some(thread_id),
            Self::ProviderRuntimeStarted { .. }
            | Self::ProviderRuntimeStopped { .. }
            | Self::ProviderRuntimeStderr { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingProviderOperationKind {
    UserTurn,
    Prepare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProviderOperation {
    pub operation_id: String,
    pub kind: PendingProviderOperationKind,
    pub started_at: DateTime<Utc>,
}

impl PendingProviderOperation {
    fn user(operation_id: String) -> Self {
        Self {
            operation_id,
            kind: PendingProviderOperationKind::UserTurn,
            started_at: Utc::now(),
        }
    }

    fn prepare(operation_id: String) -> Self {
        Self {
            operation_id,
            kind: PendingProviderOperationKind::Prepare,
            started_at: Utc::now(),
        }
    }

    fn kind(&self) -> PendingProviderOperationKind {
        self.kind
    }
}

#[derive(Debug, Clone)]
enum ProviderReadinessState {
    Uninitialized,
    InitialScanning,
    Ready,
    Failed(PedelecError),
}

#[derive(Debug)]
struct ProviderReadinessInner {
    state: Mutex<ProviderReadinessState>,
    changed: Condvar,
}

/// Synchronization for the provider snapshot lifecycle.
///
/// This is deliberately separate from `CoreRuntime`'s shared mutex. Waiting
/// on this value must never keep the runtime mutex held, because the scan
/// needs that mutex to install its completed snapshot.
#[derive(Debug, Clone)]
pub struct ProviderReadiness {
    inner: Arc<ProviderReadinessInner>,
}

impl Default for ProviderReadiness {
    fn default() -> Self {
        Self::new_uninitialized()
    }
}

impl ProviderReadiness {
    fn new_uninitialized() -> Self {
        Self::new(ProviderReadinessState::Uninitialized)
    }

    fn new(state: ProviderReadinessState) -> Self {
        Self {
            inner: Arc::new(ProviderReadinessInner {
                state: Mutex::new(state),
                changed: Condvar::new(),
            }),
        }
    }

    fn is_uninitialized(&self) -> bool {
        matches!(
            *self.inner.state.lock().unwrap(),
            ProviderReadinessState::Uninitialized
        )
    }

    fn is_initial_scanning(&self) -> bool {
        matches!(
            *self.inner.state.lock().unwrap(),
            ProviderReadinessState::InitialScanning
        )
    }

    fn mark_initial_scanning(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if matches!(*state, ProviderReadinessState::Uninitialized) {
            *state = ProviderReadinessState::InitialScanning;
        }
    }

    fn mark_ready(&self) {
        let mut state = self.inner.state.lock().unwrap();
        *state = ProviderReadinessState::Ready;
        self.inner.changed.notify_all();
    }

    fn mark_failed(&self, error: PedelecError) {
        let mut state = self.inner.state.lock().unwrap();
        *state = ProviderReadinessState::Failed(error);
        self.inner.changed.notify_all();
    }

    fn wait(&self) -> Result<(), PedelecError> {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            match &*state {
                ProviderReadinessState::Ready => return Ok(()),
                ProviderReadinessState::Failed(error) => return Err(error.clone()),
                ProviderReadinessState::Uninitialized | ProviderReadinessState::InitialScanning => {
                    state = self.inner.changed.wait(state).unwrap();
                }
            }
        }
    }

    /// Test-only synchronization seam used by downstream crate tests to hold
    /// requests behind an in-progress initial scan without starting a real
    /// provider discovery process.
    #[doc(hidden)]
    pub fn mark_initial_scanning_for_test(&self) {
        self.mark_initial_scanning();
    }

    /// Test-only synchronization seam used by downstream crate tests to
    /// release requests after a synthetic provider snapshot is ready.
    #[doc(hidden)]
    pub fn mark_ready_for_test(&self) {
        self.mark_ready();
    }
}

#[derive(Debug, Default)]
pub struct CoreRuntime {
    pub thread_manager: ThreadManager,
    pub workspace_manager: WorkspaceManager,
    /// Authoritative Core Workspace resources.  `workspace_manager` owns
    /// managed-root filesystem policy; this registry owns Workspace identity,
    /// canonical paths, kind, and SDK capabilities.
    pub workspace_registry: WorkspaceRegistry,
    pub skill_manager: SkillManager,
    pub tool_registry: ToolRegistryStore,
    pub tool_request_broker: ToolRequestBroker,
    pub event_bus: EventBus,
    pub provider_runtime_diagnostics: ProviderRuntimeDiagnosticBus,
    pub provider_protocol_traffic: ProviderProtocolTrafficBus,
    pub core_ipc_endpoint: Option<String>,
    pub core_ipc_runtime_file_path: Option<PathBuf>,
    pub settings_file_path: Option<PathBuf>,
    pub provider_scan: HashMap<ProviderCode, ProviderCli>,
    pub provider_resolved_path: Option<OsString>,
    pub provider_refresh_in_progress: bool,
    pub provider_readiness: ProviderReadiness,
    pub asset_upload_port: Option<u16>,
    pub asset_upload_tickets: HashMap<String, AssetUploadTicket>,
    pub asset_download_tickets: HashMap<String, AssetDownloadTicket>,
    /// Authoritative metadata and setup state for the modules declared by
    /// each thread.  This intentionally remains separate from ThreadState so
    /// older diagnostic/test thread constructors remain source-compatible.
    pub deno_modules: HashMap<String, Vec<DenoModuleState>>,
    pub deno_module_upload_tickets: HashMap<String, DenoModuleUploadTicket>,
    /// Workspace-run Deno packages are keyed by the authoritative Workspace
    /// and authenticated SDK origin. They never share Thread snapshots.
    pub workspace_deno_module_scopes:
        HashMap<WorkspaceDenoModuleScopeKey, WorkspaceDenoModuleScopeState>,
    /// A Workspace-run cache root is reset once per Workspace per Core
    /// lifetime, before any stale on-disk package can be considered ready.
    pub workspace_deno_roots_initialized: HashSet<String>,
    pub provider_path_value_override: Option<OsString>,
    pub pending_provider_operations: HashMap<String, PendingProviderOperation>,
    pub last_completed_operations: HashMap<String, CompletedOperationSnapshot>,
    /// The authoritative normalized cumulative token total for each thread.
    /// This is intentionally separate from `provider_usage`, which retains
    /// opaque provider payloads for diagnostics.
    pub session_usage: HashMap<String, SessionUsage>,
    /// Baselines for providers that report cumulative usage within one turn.
    pub session_usage_turn_baselines: HashMap<(String, String), u64>,
    /// Operation identities already included in the normalized total.
    pub session_usage_operations: HashSet<(String, String)>,
    pub provider_usage: HashMap<String, Value>,
    /// Workspace-owned Deno reservations. Multiple run IDs may be active in
    /// one Workspace, while provider admission is excluded for the whole
    /// Workspace until every reservation is released.
    pub active_workspace_runs: HashMap<String, HashSet<String>>,
    pub workspace_run_import_maps: HashMap<(String, String), PathBuf>,
    /// Threads in this set have restored ended-thread diagnostic resources
    /// and are waiting for the trusted dispatch boundary to admit the turn.
    /// It lets a pre-admission failure restore the original Ended semantics.
    pub debug_reactivating_threads: HashSet<String>,
}

impl CoreRuntime {
    pub fn new() -> Self {
        let mut runtime = Self::default();
        runtime.provider_readiness = ProviderReadiness::new_uninitialized();
        runtime
    }

    /// Runtime used by the desktop application. All supported providers
    /// dispatch through the persistent runtime path, so this is currently
    /// equivalent to [`Self::new`]; it remains a distinct entry point for
    /// the production owner.
    pub fn new_for_application() -> Self {
        Self::new()
    }

    /// Returns an authoritative Workspace resource by opaque Core identity.
    pub fn workspace(&self, workspace_id: &str) -> Result<&WorkspaceState, PedelecError> {
        self.workspace_registry.workspace(workspace_id)
    }

    /// Returns a mutable authoritative Workspace resource by opaque Core
    /// identity.  This is kept internal-facing so capability mutation remains
    /// inside Core.
    pub fn workspace_mut(
        &mut self,
        workspace_id: &str,
    ) -> Result<&mut WorkspaceState, PedelecError> {
        self.workspace_registry.workspace_mut(workspace_id)
    }

    /// Returns managed Workspace paths for the trusted SDK error-sanitization
    /// boundary. These paths never leave Core; the IPC layer uses them only to
    /// redact Desktop-owned filesystem details from browser-facing errors.
    pub fn managed_workspace_paths_for_error_sanitization(&self) -> Vec<PathBuf> {
        let mut paths = self
            .workspace_manager
            .managed_workspace_root_for_internal_use()
            .ok()
            .into_iter()
            .collect::<Vec<_>>();
        paths.extend(
            self.workspace_registry
                .workspaces
                .values()
                .filter(|workspace| matches!(workspace.kind, WorkspaceKind::Managed))
                .map(|workspace| workspace.canonical_path.clone()),
        );
        paths
    }

    /// Resolves a Thread to the Workspace capability that owns its path.
    pub fn thread_workspace(&self, thread_id: &str) -> Result<&WorkspaceState, PedelecError> {
        let workspace_id = self.thread_manager.thread(thread_id)?.workspace_id.clone();
        self.workspace(&workspace_id)
    }

    /// Resolves a Thread's authoritative Workspace path.  The path is cloned
    /// at this boundary so callers cannot retain a mutable alias to registry
    /// state.
    pub fn thread_workspace_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.thread_workspace(thread_id)
            .ok()
            .map(|workspace| workspace.canonical_path.clone())
    }

    /// Returns all currently registered Threads bound to a Workspace without
    /// scanning its filesystem.
    pub fn threads_in_workspace(&self, workspace_id: &str) -> Result<Vec<String>, PedelecError> {
        self.workspace(workspace_id)?;
        Ok(self.thread_manager.threads_in_workspace(workspace_id))
    }

    /// Authorizes a normalized SDK origin for a Workspace capability.
    pub fn authorize_workspace_access(
        &self,
        workspace_id: &str,
        caller_origin: &str,
    ) -> Result<(), PedelecError> {
        let caller_origin = normalize_workspace_origin(caller_origin)?;
        let workspace = self.workspace(workspace_id)?;
        if workspace.is_origin_authorized(&caller_origin) {
            Ok(())
        } else {
            Err(PedelecError::with_details(
                error_codes::WORKSPACE_ACCESS_DENIED,
                "workspace is not accessible to this caller",
                serde_json::json!({ "workspaceId": workspace_id }),
            ))
        }
    }

    /// Lists regular files below an authoritative Workspace directory. The
    /// caller-origin check belongs to the IPC boundary; this Core method only
    /// operates on the already-resolved capability.
    pub fn list_files(
        &self,
        input: WorkspaceListInput,
    ) -> Result<WorkspaceListOutput, PedelecError> {
        let workspace = self.workspace(&input.workspace_id)?;
        list_workspace_descendants(workspace, input.path.as_deref(), WorkspaceListKind::Files)
    }

    /// Lists regular directories below an authoritative Workspace directory.
    pub fn list_folders(
        &self,
        input: WorkspaceListInput,
    ) -> Result<WorkspaceListOutput, PedelecError> {
        let workspace = self.workspace(&input.workspace_id)?;
        list_workspace_descendants(workspace, input.path.as_deref(), WorkspaceListKind::Folders)
    }

    /// Explicit aliases make the Workspace ownership visible to callers that
    /// also have Thread-oriented list methods in scope.
    pub fn list_workspace_files(
        &self,
        input: WorkspaceListInput,
    ) -> Result<WorkspaceListOutput, PedelecError> {
        self.list_files(input)
    }

    pub fn list_workspace_folders(
        &self,
        input: WorkspaceListInput,
    ) -> Result<WorkspaceListOutput, PedelecError> {
        self.list_folders(input)
    }

    /// Verifies that no provider operation is active in any Thread bound to a
    /// Workspace. This is deliberately Workspace-wide so the caller never
    /// has to pick an arbitrary Thread to report as busy.
    pub fn ensure_workspace_provider_idle(&self, workspace_id: &str) -> Result<(), PedelecError> {
        self.workspace(workspace_id)?;
        for thread_id in self.thread_manager.threads_in_workspace(workspace_id) {
            let provider_turn_active = self
                .thread_manager
                .provider_state(&thread_id)
                .and_then(|state| state.active_provider_turn_id.as_deref())
                .is_some_and(|turn_id| !turn_id.trim().is_empty());
            let pending_operation = self.pending_provider_operations.contains_key(&thread_id);
            let provider_status_active = self
                .thread_manager
                .thread(&thread_id)
                .map(|thread| {
                    matches!(
                        thread.status,
                        ThreadStatus::Starting
                            | ThreadStatus::Running
                            | ThreadStatus::WaitingToolResult
                            | ThreadStatus::Stopping
                    )
                })
                .unwrap_or(false);
            if provider_turn_active || pending_operation || provider_status_active {
                return Err(workspace_busy_error(workspace_id));
            }
        }
        Ok(())
    }

    fn ensure_workspace_runs_idle_for_thread(&self, thread_id: &str) -> Result<(), PedelecError> {
        let workspace_id = self.thread_manager.thread(thread_id)?.workspace_id.clone();
        if self
            .active_workspace_runs
            .get(&workspace_id)
            .is_some_and(|runs| !runs.is_empty())
        {
            return Err(workspace_busy_error(&workspace_id));
        }
        Ok(())
    }

    /// Atomically admits a Workspace-owned Deno execution and reserves its
    /// run ID. No Thread status, provider session, or Thread Deno modules are
    /// consulted by this operation.
    pub fn begin_workspace_run(
        &mut self,
        input: WorkspaceRunInput,
    ) -> Result<WorkspaceRunStart, PedelecError> {
        self.begin_workspace_run_internal(input, None)
    }

    fn begin_workspace_run_internal(
        &mut self,
        input: WorkspaceRunInput,
        caller_origin: Option<&str>,
    ) -> Result<WorkspaceRunStart, PedelecError> {
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_WORKSPACE_RUN_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "workspace run timeoutMs must be a positive integer",
            ));
        }

        let workspace = self.workspace(&input.workspace_id)?.clone();
        validate_workspace_deno_module_names(&input.deno_modules)?;
        let scope_id = if input.deno_modules.is_empty() {
            None
        } else {
            let caller_origin = caller_origin.ok_or_else(|| {
                PedelecError::new(
                    error_codes::IPC_UNAUTHORIZED,
                    "Workspace Deno Modules require an authenticated caller origin",
                )
            })?;
            let caller_origin = normalize_workspace_origin(caller_origin)?;
            self.authorize_workspace_access(&input.workspace_id, &caller_origin)?;
            Some(self.workspace_deno_scope_for_run(
                &input.workspace_id,
                &caller_origin,
                &input.deno_modules,
            )?)
        };
        self.ensure_workspace_provider_idle(&input.workspace_id)?;
        let workspace_path = resolve_workspace_root(&workspace)?;
        let run_id = loop {
            let candidate = format!("wr_{}", Uuid::new_v4().simple());
            let occupied = self
                .active_workspace_runs
                .get(&input.workspace_id)
                .is_some_and(|runs| runs.contains(&candidate));
            if !occupied {
                break candidate;
            }
        };
        self.active_workspace_runs
            .entry(input.workspace_id.clone())
            .or_default()
            .insert(run_id.clone());

        let import_map_path = match scope_id.as_deref() {
            Some(scope_id) => match materialize_workspace_deno_run_import_map(
                &workspace_path,
                scope_id,
                &run_id,
                &input.deno_modules,
            ) {
                Ok(path) => {
                    self.workspace_run_import_maps
                        .insert((input.workspace_id.clone(), run_id.clone()), path.clone());
                    Some(path)
                }
                Err(error) => {
                    self.finish_workspace_run(&input.workspace_id, &run_id);
                    return Err(error);
                }
            },
            None => None,
        };

        Ok(WorkspaceRunStart {
            workspace_id: input.workspace_id.clone(),
            run_id: run_id.clone(),
            intent: DenoExecutionIntent {
                thread_id: String::new(),
                owner: DenoExecutionOwner::Workspace {
                    workspace_id: input.workspace_id,
                    run_id,
                },
                workspace_path,
                target: DenoExecutionTarget::StdinSource {
                    source: input.script,
                },
                args: Vec::new(),
                timeout_ms,
                import_map_path,
            },
        })
    }

    /// Releases a Workspace run reservation. It is intentionally idempotent
    /// so dispatch/error cleanup can use a finally-style path safely.
    pub fn finish_workspace_run(&mut self, workspace_id: &str, run_id: &str) {
        if let Some(path) = self
            .workspace_run_import_maps
            .remove(&(workspace_id.to_string(), run_id.to_string()))
        {
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
        let remove_workspace = if let Some(runs) = self.active_workspace_runs.get_mut(workspace_id)
        {
            runs.remove(run_id);
            runs.is_empty()
        } else {
            false
        };
        if remove_workspace {
            self.active_workspace_runs.remove(workspace_id);
        }
    }

    pub fn active_workspace_run_count(&self, workspace_id: &str) -> usize {
        self.active_workspace_runs
            .get(workspace_id)
            .map_or(0, HashSet::len)
    }

    /// Opens (or reuses) a custom Workspace capability after the selected
    /// path has passed the existing custom-workspace validation rules.
    ///
    /// Workspace metadata and runtime directories are initialized here, not
    /// as a side effect of Thread creation.
    pub fn open_workspace(
        &mut self,
        input: OpenWorkspaceInput,
        caller_origin: &str,
        caller_sdk_version: Option<&str>,
    ) -> Result<OpenWorkspaceOutput, PedelecError> {
        let caller_origin = normalize_workspace_origin(caller_origin)?;
        let sdk_version = caller_sdk_version
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .unwrap_or(env!("CARGO_PKG_VERSION"));
        let canonical_path = self
            .workspace_manager
            .prepare_custom_workspace(&input.path)?;
        self.workspace_manager.ensure_custom_workspace_config(
            &canonical_path,
            sdk_version,
            &caller_origin,
        )?;

        let candidate_id = format!("ws_{}", Uuid::new_v4().simple());
        let (workspace_id, inserted) = self
            .workspace_registry
            .insert_or_get_custom(candidate_id, canonical_path.clone())?;
        if inserted {
            // The state was created with an empty capability set.  Keep the
            // mutation in one place so a future caller cannot forget to add
            // the origin on a newly opened resource.
            self.workspace_registry
                .workspace_mut(&workspace_id)?
                .authorized_sdk_origins
                .insert(caller_origin);
        } else {
            self.workspace_registry
                .workspace_mut(&workspace_id)?
                .authorized_sdk_origins
                .insert(caller_origin);
        }

        let workspace = self.workspace(&workspace_id)?;
        Ok(OpenWorkspaceOutput {
            workspace_id,
            path: path_for_external_use(&workspace.canonical_path),
        })
    }

    /// Explicit name for SDK/transport callers that want to distinguish this
    /// operation from the later managed-workspace convenience flow.
    pub fn open_custom_workspace(
        &mut self,
        input: OpenWorkspaceInput,
        caller_origin: &str,
        caller_sdk_version: Option<&str>,
    ) -> Result<OpenWorkspaceOutput, PedelecError> {
        self.open_workspace(input, caller_origin, caller_sdk_version)
    }

    /// Test/integration seam for constructing synthetic Thread states without
    /// bypassing the Workspace registry. Production callers should use
    /// [`Self::open_workspace`] or managed Thread creation.
    #[doc(hidden)]
    pub fn register_workspace_for_test(
        &mut self,
        workspace_id: impl Into<String>,
        canonical_path: impl Into<PathBuf>,
        kind: WorkspaceKind,
    ) -> Result<(), PedelecError> {
        let canonical_path = canonical_path.into();
        self.workspace_registry.insert(WorkspaceState::new(
            workspace_id.into(),
            canonical_path,
            kind,
        ))
    }

    fn create_managed_workspace_resource(
        &mut self,
        caller_origin: Option<&str>,
    ) -> Result<(String, PathBuf), PedelecError> {
        let caller_origin = caller_origin.map(normalize_workspace_origin).transpose()?;
        let workspace_id = format!("ws_{}", Uuid::new_v4().simple());
        let workspace_path = self
            .workspace_manager
            .create_managed_workspace(&workspace_id)?;
        let canonical_path = workspace_path.canonicalize().map_err(|err| {
            workspace_io_error(
                error_codes::WORKSPACE_CREATE_FAILED,
                "cannot canonicalize managed workspace",
                &workspace_path,
                err,
            )
        })?;
        let mut state = WorkspaceState::new(
            workspace_id.clone(),
            canonical_path.clone(),
            WorkspaceKind::Managed,
        );
        if let Some(origin) = caller_origin {
            state.authorized_sdk_origins.insert(origin);
        }
        if let Err(error) = self.workspace_registry.insert(state) {
            let _ = self
                .workspace_manager
                .remove_managed_workspace_with_retry(&canonical_path);
            return Err(error);
        }
        Ok((workspace_id, canonical_path))
    }

    fn remove_workspace_resource(&mut self, workspace_id: &str) {
        self.workspace_registry.remove(workspace_id);
        self.workspace_deno_roots_initialized.remove(workspace_id);
        self.workspace_deno_module_scopes
            .retain(|key, _| key.workspace_id != workspace_id);
        self.active_workspace_runs.remove(workspace_id);
        self.workspace_run_import_maps
            .retain(|(candidate_workspace_id, _), _| candidate_workspace_id != workspace_id);
        self.deno_module_upload_tickets.retain(|_, ticket| {
            !matches!(
                &ticket.owner,
                DenoModuleUploadOwner::Workspace { workspace_id: candidate, .. }
                    if candidate == workspace_id
            )
        });
    }

    pub fn set_core_ipc_runtime(
        &mut self,
        endpoint: impl Into<String>,
        runtime_file_path: impl Into<PathBuf>,
    ) {
        self.core_ipc_endpoint = Some(endpoint.into());
        self.core_ipc_runtime_file_path = Some(runtime_file_path.into());
    }

    pub fn set_asset_upload_port(&mut self, port: u16) {
        self.asset_upload_port = Some(port);
    }

    pub fn prepare_workspace_deno_modules(
        &mut self,
        input: PrepareWorkspaceDenoModulesInput,
        caller_origin: &str,
    ) -> Result<PrepareWorkspaceDenoModulesOutput, PedelecError> {
        let caller_origin = normalize_workspace_origin(caller_origin)?;
        self.authorize_workspace_access(&input.workspace_id, &caller_origin)?;
        validate_workspace_deno_module_names(&input.module_names)?;
        if input.module_names.is_empty() {
            return Ok(PrepareWorkspaceDenoModulesOutput {
                missing_module_names: Vec::new(),
            });
        }
        self.ensure_workspace_provider_idle(&input.workspace_id)?;
        if self.active_workspace_run_count(&input.workspace_id) > 0 {
            return Err(workspace_busy_error(&input.workspace_id));
        }

        let workspace_path = self.workspace(&input.workspace_id)?.canonical_path.clone();
        if !self
            .workspace_deno_roots_initialized
            .contains(&input.workspace_id)
        {
            reset_workspace_deno_workspace_root(&workspace_path)?;
            self.workspace_deno_roots_initialized
                .insert(input.workspace_id.clone());
        }

        let key = WorkspaceDenoModuleScopeKey {
            workspace_id: input.workspace_id.clone(),
            sdk_origin: caller_origin.clone(),
        };
        let scope_id = self
            .workspace_deno_module_scopes
            .entry(key.clone())
            .or_insert_with(|| WorkspaceDenoModuleScopeState {
                scope_id: format!("scope_{}", Uuid::new_v4().simple()),
                modules: HashMap::new(),
            })
            .scope_id
            .clone();
        let roots = ensure_workspace_deno_scope_roots(&workspace_path, &scope_id)?;

        let active_uploads = self
            .deno_module_upload_tickets
            .values()
            .filter_map(|ticket| match &ticket.owner {
                DenoModuleUploadOwner::Workspace {
                    workspace_id,
                    sdk_origin,
                    scope_id: candidate_scope_id,
                } if workspace_id == &input.workspace_id
                    && sdk_origin == &key.sdk_origin
                    && candidate_scope_id == &scope_id
                    && matches!(
                        ticket.state,
                        DenoModuleUploadState::Pending | DenoModuleUploadState::Uploading
                    ) =>
                {
                    Some(ticket.module_name.clone())
                }
                _ => None,
            })
            .collect::<HashSet<_>>();

        let mut missing = Vec::new();
        let scope = self.workspace_deno_module_scopes.get_mut(&key).unwrap();
        for module_name in &input.module_names {
            match scope.modules.get(module_name) {
                Some(DenoModuleSetupState::Ready) => {}
                Some(DenoModuleSetupState::Pending) if active_uploads.contains(module_name) => {
                    return Err(PedelecError::with_details(
                        error_codes::WORKSPACE_BUSY,
                        "Workspace Deno Module setup is already in progress",
                        serde_json::json!({ "workspaceId": input.workspace_id, "moduleName": module_name }),
                    ));
                }
                Some(DenoModuleSetupState::Pending) | Some(DenoModuleSetupState::Failed) => {
                    remove_workspace_deno_module_package(&roots, module_name)?;
                    scope
                        .modules
                        .insert(module_name.clone(), DenoModuleSetupState::Pending);
                    missing.push(module_name.clone());
                }
                None => {
                    scope
                        .modules
                        .insert(module_name.clone(), DenoModuleSetupState::Pending);
                    missing.push(module_name.clone());
                }
            }
        }

        Ok(PrepareWorkspaceDenoModulesOutput {
            missing_module_names: missing,
        })
    }

    pub fn create_workspace_deno_module_upload(
        &mut self,
        input: CreateWorkspaceDenoModuleUploadInput,
        caller_origin: &str,
    ) -> Result<CreateDenoModuleUploadOutput, PedelecError> {
        let caller_origin = normalize_workspace_origin(caller_origin)?;
        self.authorize_workspace_access(&input.workspace_id, &caller_origin)?;
        validate_deno_module_name(&input.module_name)?;
        validate_deno_module_upload_size(input.expected_size_bytes, &input.module_name)?;
        self.ensure_workspace_provider_idle(&input.workspace_id)?;
        if self.active_workspace_run_count(&input.workspace_id) > 0 {
            return Err(workspace_busy_error(&input.workspace_id));
        }
        let workspace = self.workspace(&input.workspace_id)?.clone();
        if !self
            .workspace_deno_roots_initialized
            .contains(&input.workspace_id)
        {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup has not been prepared",
            ));
        }
        let key = WorkspaceDenoModuleScopeKey {
            workspace_id: input.workspace_id.clone(),
            sdk_origin: caller_origin.clone(),
        };
        let scope = self.workspace_deno_module_scopes.get(&key).ok_or_else(|| {
            PedelecError::new(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup has not been prepared",
            )
        })?;
        let state = scope.modules.get(&input.module_name).ok_or_else(|| {
            PedelecError::new(
                error_codes::DENO_MODULE_NOT_FOUND,
                "Deno Module was not declared for this Workspace run",
            )
        })?;
        if *state == DenoModuleSetupState::Ready {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_ALREADY_READY,
                "Deno Module is already ready and cannot be replaced",
            ));
        }
        if *state != DenoModuleSetupState::Pending {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Deno Module is not pending upload",
            ));
        }
        let scope_id = scope.scope_id.clone();
        self.expire_deno_module_uploads();
        if self.deno_module_upload_tickets.values().any(|ticket| {
            matches!(
                &ticket.owner,
                DenoModuleUploadOwner::Workspace { workspace_id, sdk_origin, scope_id: candidate_scope_id }
                    if workspace_id == &input.workspace_id
                        && sdk_origin == &caller_origin
                        && candidate_scope_id == &scope_id
            ) && ticket.module_name == input.module_name
                && matches!(ticket.state, DenoModuleUploadState::Pending | DenoModuleUploadState::Uploading)
        }) {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_BUSY,
                "a Workspace Deno Module upload is already in progress",
            ));
        }
        let port = self.asset_upload_port.ok_or_else(|| {
            PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_SERVER_UNAVAILABLE,
                "Deno Module upload server is unavailable",
            )
        })?;
        let upload_id = loop {
            let candidate = format!("dmp_{}", &Uuid::new_v4().simple().to_string()[..8]);
            if !self.deno_module_upload_tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        let token = (0..8)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>();
        let expires_at = Utc::now() + chrono::Duration::seconds(ASSET_UPLOAD_TICKET_SECONDS);
        self.deno_module_upload_tickets.insert(
            upload_id.clone(),
            DenoModuleUploadTicket {
                owner: DenoModuleUploadOwner::Workspace {
                    workspace_id: input.workspace_id.clone(),
                    sdk_origin: caller_origin,
                    scope_id,
                },
                thread_id: String::new(),
                workspace_path: workspace.canonical_path,
                module_name: input.module_name,
                expected_size_bytes: input.expected_size_bytes,
                token_hash: format!("{:x}", Sha256::digest(token.as_bytes())),
                expires_at,
                state: DenoModuleUploadState::Pending,
            },
        );
        Ok(CreateDenoModuleUploadOutput {
            upload_id: upload_id.clone(),
            upload_url: format!("http://127.0.0.1:{port}/deno-modules/{upload_id}"),
            token,
            expires_at: expires_at.timestamp_millis(),
        })
    }

    fn workspace_deno_scope_for_run(
        &self,
        workspace_id: &str,
        caller_origin: &str,
        module_names: &[String],
    ) -> Result<String, PedelecError> {
        let key = WorkspaceDenoModuleScopeKey {
            workspace_id: workspace_id.to_string(),
            sdk_origin: caller_origin.to_string(),
        };
        let scope = self.workspace_deno_module_scopes.get(&key).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup is incomplete",
                serde_json::json!({ "workspaceId": workspace_id, "modules": module_names }),
            )
        })?;
        let missing = module_names
            .iter()
            .filter(|name| scope.modules.get(*name) != Some(&DenoModuleSetupState::Ready))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup is incomplete",
                serde_json::json!({ "workspaceId": workspace_id, "modules": missing }),
            ));
        }
        Ok(scope.scope_id.clone())
    }

    pub fn begin_workspace_run_for_origin(
        &mut self,
        input: WorkspaceRunInput,
        caller_origin: &str,
    ) -> Result<WorkspaceRunStart, PedelecError> {
        self.begin_workspace_run_internal(input, Some(caller_origin))
    }

    pub fn create_deno_module_upload(
        &mut self,
        input: CreateDenoModuleUploadInput,
    ) -> Result<CreateDenoModuleUploadOutput, PedelecError> {
        validate_deno_module_name(&input.module_name)?;
        if input.expected_size_bytes == 0 {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
                "Deno Module artifact size must be positive",
            ));
        }
        if input.expected_size_bytes > MAX_ASSET_UPLOAD_BYTES {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_ARTIFACT_TOO_LARGE,
                "Deno Module artifact exceeds the 100 MiB limit",
                serde_json::json!({
                    "moduleName": input.module_name,
                    "expectedSizeBytes": input.expected_size_bytes,
                    "maxSizeBytes": MAX_ASSET_UPLOAD_BYTES,
                }),
            ));
        }

        let port = self.asset_upload_port.ok_or_else(|| {
            PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_SERVER_UNAVAILABLE,
                "Deno Module upload server is unavailable",
            )
        })?;
        let thread = self.thread_manager.thread(&input.thread_id)?.clone();
        let workspace_path = self
            .thread_workspace(&input.thread_id)?
            .canonical_path
            .clone();
        match thread.status {
            ThreadStatus::Idle => {}
            ThreadStatus::Ended | ThreadStatus::Stopping => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_ENDED,
                    "thread has ended",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
            _ => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is not available for Deno Module setup",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
        }

        let module = self
            .deno_modules
            .get(&input.thread_id)
            .and_then(|modules| {
                modules
                    .iter()
                    .find(|module| module.name == input.module_name)
            })
            .cloned()
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::DENO_MODULE_NOT_FOUND,
                    "Deno Module was not declared for this thread",
                    serde_json::json!({
                        "threadId": input.thread_id,
                        "moduleName": input.module_name,
                    }),
                )
            })?;
        self.expire_deno_module_uploads();
        if module.state == DenoModuleSetupState::Ready {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_ALREADY_READY,
                "Deno Module is already ready and cannot be replaced",
                serde_json::json!({ "moduleName": input.module_name }),
            ));
        }
        if self.deno_module_upload_tickets.values().any(|ticket| {
            ticket.thread_id == input.thread_id
                && ticket.module_name == input.module_name
                && matches!(
                    ticket.state,
                    DenoModuleUploadState::Pending | DenoModuleUploadState::Uploading
                )
        }) {
            return Err(PedelecError::with_details(
                error_codes::THREAD_BUSY,
                "a Deno Module upload is already in progress",
                serde_json::json!({ "moduleName": input.module_name }),
            ));
        }

        let upload_id = loop {
            let candidate = format!("dmp_{}", &Uuid::new_v4().simple().to_string()[..8]);
            if !self.deno_module_upload_tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        let token = (0..8)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>();
        let expires_at = Utc::now() + chrono::Duration::seconds(ASSET_UPLOAD_TICKET_SECONDS);
        self.deno_module_upload_tickets.insert(
            upload_id.clone(),
            DenoModuleUploadTicket {
                owner: DenoModuleUploadOwner::Thread {
                    thread_id: input.thread_id.clone(),
                },
                thread_id: input.thread_id,
                workspace_path,
                module_name: input.module_name,
                expected_size_bytes: input.expected_size_bytes,
                token_hash: format!("{:x}", Sha256::digest(token.as_bytes())),
                expires_at,
                state: DenoModuleUploadState::Pending,
            },
        );

        Ok(CreateDenoModuleUploadOutput {
            upload_id: upload_id.clone(),
            upload_url: format!("http://127.0.0.1:{port}/deno-modules/{upload_id}"),
            token,
            expires_at: expires_at.timestamp_millis(),
        })
    }

    pub fn expire_deno_module_uploads(&mut self) {
        let now = Utc::now();
        for ticket in self.deno_module_upload_tickets.values_mut() {
            if ticket.state == DenoModuleUploadState::Pending && ticket.expires_at <= now {
                ticket.state = DenoModuleUploadState::Expired;
            }
        }
    }

    pub fn mark_deno_module_upload_failed(&mut self, upload_id: &str) {
        let Some(ticket) = self.deno_module_upload_tickets.get(upload_id).cloned() else {
            return;
        };
        if let DenoModuleUploadOwner::Workspace {
            workspace_id,
            sdk_origin,
            scope_id,
        } = ticket.owner
        {
            let key = WorkspaceDenoModuleScopeKey {
                workspace_id,
                sdk_origin,
            };
            if let Some(scope) = self.workspace_deno_module_scopes.get_mut(&key) {
                scope
                    .modules
                    .insert(ticket.module_name.clone(), DenoModuleSetupState::Failed);
            }
            if let Ok(roots) = ensure_workspace_deno_scope_roots(&ticket.workspace_path, &scope_id)
            {
                let _ = remove_workspace_deno_module_package(&roots, &ticket.module_name);
            }
            if let Some(ticket) = self.deno_module_upload_tickets.get_mut(upload_id) {
                ticket.state = DenoModuleUploadState::Failed;
            }
            return;
        }
        let thread_id = ticket.thread_id.clone();
        let module_name = ticket.module_name.clone();
        if let Some(ticket) = self.deno_module_upload_tickets.get_mut(upload_id) {
            ticket.state = DenoModuleUploadState::Failed;
        }
        if let Some(modules) = self.deno_modules.get_mut(&thread_id) {
            if let Some(module) = modules.iter_mut().find(|module| module.name == module_name) {
                module.state = DenoModuleSetupState::Failed;
            }
        }
    }

    /// Validates and atomically commits one uploaded envelope.  The transfer
    /// server owns the bounded byte stream; Core owns the envelope validation,
    /// package shape, and authoritative ready transition.
    pub fn complete_deno_module_upload(
        &mut self,
        upload_id: &str,
        temporary_path: &Path,
    ) -> Result<DenoModuleUploadCompletion, PedelecError> {
        let owner = self
            .deno_module_upload_tickets
            .get(upload_id)
            .map(|ticket| ticket.owner.clone())
            .ok_or_else(|| {
                PedelecError::new(
                    error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                    "Deno Module upload ticket is invalid",
                )
            })?;
        if matches!(owner, DenoModuleUploadOwner::Workspace { .. }) {
            return self.complete_workspace_deno_module_upload(upload_id, temporary_path);
        }
        let ticket = self
            .deno_module_upload_tickets
            .get(upload_id)
            .cloned()
            .ok_or_else(|| {
                PedelecError::new(
                    error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                    "Deno Module upload ticket is invalid",
                )
            })?;
        if ticket.state != DenoModuleUploadState::Uploading {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                "Deno Module upload ticket is not active",
            ));
        }
        if !self
            .deno_modules
            .get(&ticket.thread_id)
            .is_some_and(|modules| {
                modules.iter().any(|module| {
                    module.name == ticket.module_name && module.state != DenoModuleSetupState::Ready
                })
            })
        {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_NOT_FOUND,
                "Deno Module setup state was not found",
                serde_json::json!({
                    "threadId": ticket.thread_id,
                    "moduleName": ticket.module_name,
                }),
            ));
        }

        let mut package_committed = false;
        let result = (|| {
            let metadata = fs::metadata(temporary_path).map_err(|err| {
                PedelecError::with_details(
                    error_codes::DENO_MODULE_UPLOAD_FAILED,
                    "cannot read uploaded Deno Module artifact",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            if !metadata.is_file() || metadata.len() != ticket.expected_size_bytes {
                return Err(PedelecError::with_details(
                    error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
                    "Deno Module upload size does not match its ticket",
                    serde_json::json!({
                        "expectedSizeBytes": ticket.expected_size_bytes,
                        "actualSizeBytes": metadata.len(),
                    }),
                ));
            }
            let bytes = fs::read(temporary_path).map_err(|err| {
                PedelecError::with_details(
                    error_codes::DENO_MODULE_UPLOAD_FAILED,
                    "cannot read uploaded Deno Module artifact",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            let envelope: DenoModuleArtifactEnvelope =
                serde_json::from_slice(&bytes).map_err(|err| {
                    PedelecError::with_details(
                        error_codes::DENO_MODULE_ARTIFACT_INVALID,
                        "Deno Module artifact envelope is invalid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    )
                })?;
            validate_deno_module_artifact_envelope(&envelope)?;
            materialize_deno_module_package(
                &ticket.workspace_path,
                &ticket.thread_id,
                &ticket.module_name,
                &envelope,
                upload_id,
            )?;
            package_committed = true;

            // The final upload owns the import-map commit.  Build the
            // candidate snapshot with this module already marked ready, but
            // do not publish that state to Core until the import map and all
            // package files have been validated successfully.
            let modules_after_upload = self
                .deno_modules
                .get(&ticket.thread_id)
                .cloned()
                .ok_or_else(|| {
                    PedelecError::with_details(
                        error_codes::DENO_MODULE_NOT_FOUND,
                        "Deno Module setup state was not found",
                        serde_json::json!({ "threadId": ticket.thread_id }),
                    )
                })?;
            let mut modules_after_upload = modules_after_upload;
            let module = modules_after_upload
                .iter_mut()
                .find(|module| module.name == ticket.module_name)
                .ok_or_else(|| {
                    PedelecError::with_details(
                        error_codes::DENO_MODULE_NOT_FOUND,
                        "Deno Module was not declared for this thread",
                        serde_json::json!({ "moduleName": ticket.module_name }),
                    )
                })?;
            module.state = DenoModuleSetupState::Ready;
            if modules_after_upload
                .iter()
                .all(|module| module.state == DenoModuleSetupState::Ready)
            {
                materialize_deno_module_import_map(
                    &ticket.workspace_path,
                    &ticket.thread_id,
                    &modules_after_upload,
                    upload_id,
                )?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                let modules = self
                    .deno_modules
                    .get_mut(&ticket.thread_id)
                    .ok_or_else(|| {
                        PedelecError::with_details(
                            error_codes::DENO_MODULE_NOT_FOUND,
                            "Deno Module setup state was not found",
                            serde_json::json!({ "threadId": ticket.thread_id }),
                        )
                    })?;
                let module = modules
                    .iter_mut()
                    .find(|module| module.name == ticket.module_name)
                    .ok_or_else(|| {
                        PedelecError::with_details(
                            error_codes::DENO_MODULE_NOT_FOUND,
                            "Deno Module was not declared for this thread",
                            serde_json::json!({ "moduleName": ticket.module_name }),
                        )
                    })?;
                module.state = DenoModuleSetupState::Ready;
                if let Some(upload_ticket) = self.deno_module_upload_tickets.get_mut(upload_id) {
                    upload_ticket.state = DenoModuleUploadState::Completed;
                }
                Ok(DenoModuleUploadCompletion {
                    module_name: ticket.module_name,
                    ready: true,
                })
            }
            Err(error) => {
                if package_committed {
                    let _ = remove_materialized_deno_module_package(
                        &ticket.workspace_path,
                        &ticket.thread_id,
                        &ticket.module_name,
                    );
                }
                self.mark_deno_module_upload_failed(upload_id);
                Err(error)
            }
        }
    }

    fn complete_workspace_deno_module_upload(
        &mut self,
        upload_id: &str,
        temporary_path: &Path,
    ) -> Result<DenoModuleUploadCompletion, PedelecError> {
        let ticket = self
            .deno_module_upload_tickets
            .get(upload_id)
            .cloned()
            .ok_or_else(|| {
                PedelecError::new(
                    error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                    "Deno Module upload ticket is invalid",
                )
            })?;
        let DenoModuleUploadOwner::Workspace {
            workspace_id,
            sdk_origin,
            scope_id,
        } = ticket.owner.clone()
        else {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                "Deno Module upload ticket owner is invalid",
            ));
        };
        if ticket.state != DenoModuleUploadState::Uploading {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                "Deno Module upload ticket is not active",
            ));
        }
        let key = WorkspaceDenoModuleScopeKey {
            workspace_id: workspace_id.clone(),
            sdk_origin,
        };
        let module_state = self
            .workspace_deno_module_scopes
            .get(&key)
            .and_then(|scope| scope.modules.get(&ticket.module_name))
            .cloned();
        if module_state != Some(DenoModuleSetupState::Pending) {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup state is not pending",
            ));
        }

        // The upload ticket only authorizes the transfer.  The Workspace may
        // have become busy while the HTTP body was in flight, so repeat the
        // authoritative admission checks immediately before reading or
        // materializing the artifact.
        let workspace_path = match self.workspace(&workspace_id) {
            Ok(workspace) => workspace.canonical_path.clone(),
            Err(error) => {
                self.mark_deno_module_upload_failed(upload_id);
                return Err(error);
            }
        };
        let scope_matches = self
            .workspace_deno_module_scopes
            .get(&key)
            .is_some_and(|scope| scope.scope_id == scope_id);
        if !scope_matches {
            let error = PedelecError::new(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Workspace Deno Module setup scope is no longer available",
            );
            self.mark_deno_module_upload_failed(upload_id);
            return Err(error);
        }
        if let Err(error) = self
            .ensure_workspace_provider_idle(&workspace_id)
            .and_then(|_| {
                if self.active_workspace_run_count(&workspace_id) > 0 {
                    Err(workspace_busy_error(&workspace_id))
                } else {
                    Ok(())
                }
            })
        {
            self.mark_deno_module_upload_failed(upload_id);
            return Err(error);
        }

        let result = (|| {
            let metadata = fs::metadata(temporary_path).map_err(|err| {
                PedelecError::with_details(
                    error_codes::DENO_MODULE_UPLOAD_FAILED,
                    "cannot read uploaded Deno Module artifact",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            if !metadata.is_file() || metadata.len() != ticket.expected_size_bytes {
                return Err(PedelecError::new(
                    error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
                    "Deno Module upload size does not match its ticket",
                ));
            }
            let bytes = fs::read(temporary_path).map_err(|err| {
                PedelecError::with_details(
                    error_codes::DENO_MODULE_UPLOAD_FAILED,
                    "cannot read uploaded Deno Module artifact",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            let envelope: DenoModuleArtifactEnvelope =
                serde_json::from_slice(&bytes).map_err(|err| {
                    PedelecError::with_details(
                        error_codes::DENO_MODULE_ARTIFACT_INVALID,
                        "Deno Module artifact envelope is invalid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    )
                })?;
            validate_deno_module_artifact_envelope(&envelope)?;
            let roots = ensure_workspace_deno_scope_roots(&workspace_path, &scope_id)?;
            let canonical_modules_root = roots.canonical_modules_root.clone();
            materialize_deno_module_package_at(
                &canonical_modules_root,
                &ticket.module_name,
                &envelope,
                upload_id,
            )?;
            validate_deno_module_package(&canonical_modules_root, &scope_id, &ticket.module_name)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.workspace_deno_module_scopes
                    .get_mut(&key)
                    .expect("Workspace Deno Module scope disappeared during upload")
                    .modules
                    .insert(ticket.module_name.clone(), DenoModuleSetupState::Ready);
                if let Some(ticket_state) = self.deno_module_upload_tickets.get_mut(upload_id) {
                    ticket_state.state = DenoModuleUploadState::Completed;
                }
                Ok(DenoModuleUploadCompletion {
                    module_name: ticket.module_name,
                    ready: true,
                })
            }
            Err(error) => {
                if let Ok(roots) = ensure_workspace_deno_scope_roots(&workspace_path, &scope_id) {
                    let _ = remove_workspace_deno_module_package(&roots, &ticket.module_name);
                }
                self.mark_deno_module_upload_failed(upload_id);
                Err(error)
            }
        }
    }

    pub fn create_asset_upload(
        &mut self,
        input: CreateAssetUploadInput,
    ) -> Result<CreateAssetUploadOutput, PedelecError> {
        if input.filename.trim().is_empty() || input.filename == "." || input.filename == ".." {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "filename is invalid",
            ));
        }
        if input.size_bytes > MAX_ASSET_UPLOAD_BYTES {
            return Err(PedelecError::new(
                error_codes::ASSET_TOO_LARGE,
                "asset exceeds the 100 MiB limit",
            ));
        }
        let port = self.asset_upload_port.ok_or_else(|| {
            PedelecError::new(
                error_codes::ASSET_UPLOAD_SERVER_UNAVAILABLE,
                "asset upload server is unavailable",
            )
        })?;
        let thread = self.thread_manager.thread(&input.thread_id)?;
        if matches!(thread.status, ThreadStatus::Stopping | ThreadStatus::Ended) {
            return Err(PedelecError::new(
                error_codes::THREAD_ENDED,
                "thread has ended",
            ));
        }
        let workspace_path = self
            .thread_workspace(&input.thread_id)?
            .canonical_path
            .clone();
        self.expire_asset_uploads();
        if self.asset_upload_tickets.values().any(|ticket| {
            ticket.thread_id == input.thread_id
                && matches!(
                    ticket.state,
                    AssetUploadState::Pending | AssetUploadState::Uploading
                )
        }) {
            return Err(PedelecError::new(
                error_codes::THREAD_BUSY,
                "an asset upload is already in progress",
            ));
        }
        // Keep asset names readable while using the separate 256-bit token
        // for authorization. The collision check covers all tickets in this runtime.
        let upload_id = loop {
            let candidate = format!("upl_{}", &Uuid::new_v4().simple().to_string()[..8]);
            if !self.asset_upload_tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        let token = (0..8)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>();
        let token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let expires_at = Utc::now() + chrono::Duration::seconds(ASSET_UPLOAD_TICKET_SECONDS);
        let safe_filename = safe_asset_filename(&input.filename);
        let (public_path, relative_path) = match input.target_path.as_deref() {
            Some(path) => parse_public_asset_path(path).map_err(|_| {
                PedelecError::with_details(
                    error_codes::ASSET_PATH_INVALID,
                    "asset path is invalid",
                    serde_json::json!({"threadId": input.thread_id, "path": path}),
                )
            })?,
            None => {
                let filename = format!("{upload_id}-{safe_filename}");
                (format!("/{filename}"), PathBuf::from(filename))
            }
        };
        self.asset_upload_tickets.insert(
            upload_id.clone(),
            AssetUploadTicket {
                thread_id: input.thread_id,
                workspace_path,
                public_path,
                relative_path,
                filename: input.filename,
                safe_filename,
                expected_size_bytes: input.size_bytes,
                token_hash,
                expires_at,
                state: AssetUploadState::Pending,
            },
        );
        Ok(CreateAssetUploadOutput {
            upload_id: upload_id.clone(),
            upload_url: format!("http://127.0.0.1:{port}/uploads/{upload_id}"),
            token,
            expires_at: expires_at.timestamp_millis(),
        })
    }

    pub fn list_assets(&self, input: ListAssetsInput) -> Result<ListAssetsOutput, PedelecError> {
        if input.thread_id.trim().is_empty() {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "threadId is required",
            ));
        }
        let thread = self.thread_manager.thread(&input.thread_id)?;
        if matches!(thread.status, ThreadStatus::Stopping | ThreadStatus::Ended) {
            return Err(PedelecError::new(
                error_codes::THREAD_ENDED,
                "thread has ended",
            ));
        }
        let workspace_path = self
            .thread_workspace(&input.thread_id)?
            .canonical_path
            .clone();
        let input_path = workspace_assets_root(&workspace_path);
        if !input_path.exists() {
            return Ok(ListAssetsOutput { assets: Vec::new() });
        }
        let mut assets = Vec::new();
        collect_assets(&input_path, &input_path, &mut assets)?;
        assets.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(ListAssetsOutput { assets })
    }

    pub fn expire_asset_uploads(&mut self) {
        let now = Utc::now();
        for ticket in self.asset_upload_tickets.values_mut() {
            if ticket.state == AssetUploadState::Pending && ticket.expires_at <= now {
                ticket.state = AssetUploadState::Expired;
            }
        }
    }

    pub(crate) fn invalidate_asset_uploads_for_thread(&mut self, thread_id: &str) {
        for ticket in self.asset_upload_tickets.values_mut() {
            if ticket.thread_id == thread_id
                && matches!(
                    ticket.state,
                    AssetUploadState::Pending | AssetUploadState::Uploading
                )
            {
                ticket.state = AssetUploadState::Failed;
            }
        }
    }

    pub fn create_asset_download(
        &mut self,
        input: CreateAssetDownloadInput,
    ) -> Result<CreateAssetDownloadOutput, PedelecError> {
        let thread = self.thread_manager.thread(&input.thread_id)?;
        if matches!(thread.status, ThreadStatus::Stopping | ThreadStatus::Ended) {
            return Err(PedelecError::new(
                error_codes::THREAD_ENDED,
                "thread has ended",
            ));
        }
        let workspace_path = self
            .thread_workspace(&input.thread_id)?
            .canonical_path
            .clone();
        let (target, name, size_bytes, modified_at) =
            resolve_asset_file(thread, &workspace_path, &input.path)?;
        let port = self.asset_upload_port.ok_or_else(|| {
            PedelecError::new(
                error_codes::ASSET_UPLOAD_SERVER_UNAVAILABLE,
                "asset transfer server is unavailable",
            )
        })?;
        self.expire_asset_downloads();
        let download_id = loop {
            let candidate = format!("dnl_{}", &Uuid::new_v4().simple().to_string()[..8]);
            if !self.asset_download_tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        let token = (0..8)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>();
        let expires_at = Utc::now() + chrono::Duration::seconds(ASSET_UPLOAD_TICKET_SECONDS);
        self.asset_download_tickets.insert(
            download_id.clone(),
            AssetDownloadTicket {
                thread_id: input.thread_id,
                workspace_path,
                public_path: input.path.clone(),
                token_hash: format!("{:x}", Sha256::digest(token.as_bytes())),
                expires_at,
                state: AssetDownloadState::Pending,
            },
        );
        Ok(CreateAssetDownloadOutput {
            download_id: download_id.clone(),
            download_url: format!("http://127.0.0.1:{port}/downloads/{download_id}"),
            token,
            path: input.path,
            name,
            size_bytes,
            modified_at,
            mime_type: asset_mime_type(&target),
            expires_at: expires_at.timestamp_millis(),
        })
    }

    pub fn expire_asset_downloads(&mut self) {
        let now = Utc::now();
        for ticket in self.asset_download_tickets.values_mut() {
            if ticket.state == AssetDownloadState::Pending && ticket.expires_at <= now {
                ticket.state = AssetDownloadState::Expired;
            }
        }
    }
    pub(crate) fn invalidate_asset_downloads_for_thread(&mut self, thread_id: &str) {
        for ticket in self.asset_download_tickets.values_mut() {
            if ticket.thread_id == thread_id
                && matches!(
                    ticket.state,
                    AssetDownloadState::Pending | AssetDownloadState::Downloading
                )
            {
                ticket.state = AssetDownloadState::Failed;
            }
        }
    }

    pub fn create_thread(
        &mut self,
        input: CreateThreadInput,
    ) -> Result<CreateThreadOutput, PedelecError> {
        self.create_thread_with_sdk_origin(input, None, None)
    }

    pub fn create_sdk_thread(
        &mut self,
        input: CreateThreadInput,
        caller_origin: &str,
        caller_sdk_version: Option<&str>,
    ) -> Result<CreateThreadOutput, PedelecError> {
        let origin = normalize_sdk_origin(caller_origin)?;
        let sdk_version = caller_sdk_version
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(ToOwned::to_owned);
        self.create_thread_with_sdk_origin(input, Some(origin), sdk_version)
    }

    fn create_thread_with_sdk_origin(
        &mut self,
        input: CreateThreadInput,
        sdk_origin: Option<String>,
        _sdk_version: Option<String>,
    ) -> Result<CreateThreadOutput, PedelecError> {
        let model = normalize_explicit_model(input.model)?;
        if model.is_some() && input.effort_level.is_some() {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "effortLevel cannot be combined with model",
            ));
        }
        if model.is_none() && input.effort.is_some() {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "effort requires model",
            ));
        }
        let deno_modules = normalize_deno_module_inputs(input.skills.as_ref())?;
        let (effort_level, effort_args) = if let Some(model) = model.as_deref() {
            (
                None,
                resolve_explicit_session_args(&input.provider, model, input.effort.as_deref())?,
            )
        } else {
            let settings = self.get_settings()?;
            let effort_level = input.effort_level.unwrap_or_default();
            (
                Some(effort_level),
                resolve_profile_session_args(&settings, &input.provider, effort_level)?,
            )
        };
        let skills_input = input.skills.clone();

        // A Thread may only bind to an already-registered Workspace. SDK
        // callers must have opened/authorized custom Workspaces first.
        let (workspace_id, workspace_path, managed_workspace_created) =
            if let Some(workspace_id) = input.workspace_id.clone() {
                let workspace = self.workspace(&workspace_id)?.clone();
                if let Some(origin) = sdk_origin.as_deref() {
                    self.authorize_workspace_access(&workspace_id, origin)?;
                }
                (workspace_id, workspace.canonical_path, false)
            } else {
                let (workspace_id, workspace_path) =
                    self.create_managed_workspace_resource(sdk_origin.as_deref())?;
                (workspace_id, workspace_path, true)
            };

        // Custom workspaces persist across Core restarts, so allocate the
        // short thread ID only after the selected Workspace is known and can
        // be checked for a leftover Deno thread root.
        let thread_id = match self.next_available_thread_id(
            (!managed_workspace_created).then_some(workspace_path.as_path()),
        ) {
            Ok(thread_id) => thread_id,
            Err(error) => {
                if managed_workspace_created {
                    let _ = self
                        .workspace_manager
                        .remove_managed_workspace_with_retry(&workspace_path);
                    self.remove_workspace_resource(&workspace_id);
                }
                return Err(error);
            }
        };
        let initialized =
            initialize_generated_skills(&workspace_path, &thread_id, skills_input.as_ref());
        let (skills, registry) = match initialized {
            Ok(value) => value,
            Err(error) => {
                if managed_workspace_created {
                    let _ = self
                        .workspace_manager
                        .remove_managed_workspace_with_retry(&workspace_path);
                    self.remove_workspace_resource(&workspace_id);
                } else {
                    let _ = cleanup_thread_private_runtime_artifacts(&workspace_path, &thread_id);
                }
                return Err(error);
            }
        };

        let now = Utc::now();
        let state = ThreadState {
            thread_id: thread_id.clone(),
            workspace_id: workspace_id.clone(),
            provider: input.provider,
            effort_level,
            effort_args,
            skills,
            status: ThreadStatus::Idle,
            created_at: now,
            updated_at: now,
            sdk_origin,
        };

        self.thread_manager.insert_thread(
            state,
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        self.tool_registry.insert(thread_id.clone(), registry);
        self.deno_modules.insert(thread_id.clone(), deno_modules);
        self.event_bus.register_thread_log(
            &thread_id,
            thread_event_log_path(&workspace_path, &thread_id),
        );
        self.event_bus.emit_created(&thread_id);
        self.event_bus
            .emit_status_changed(&thread_id, ThreadStatus::Idle);

        Ok(CreateThreadOutput {
            thread_id,
            workspace_id: workspace_id.clone(),
            workspace_path: self
                .workspace(&workspace_id)
                .ok()
                .filter(|workspace| workspace.kind == WorkspaceKind::Custom)
                .map(|workspace| path_for_external_use(&workspace.canonical_path)),
            explicit_model_config_applied: model.is_some(),
        })
    }

    pub fn authorize_thread_access(
        &self,
        thread_id: &str,
        caller_origin: Option<&str>,
    ) -> Result<(), PedelecError> {
        let thread = self.thread_manager.thread(thread_id)?;
        let Some(owner) = thread.sdk_origin.as_deref() else {
            return Ok(());
        };
        let caller = caller_origin.and_then(|origin| normalize_sdk_origin(origin).ok());
        if caller.as_deref() == Some(owner) {
            Ok(())
        } else {
            Err(PedelecError::with_details(
                error_codes::THREAD_ACCESS_DENIED,
                "thread is not accessible to this caller",
                serde_json::json!({ "threadId": thread_id }),
            ))
        }
    }

    pub fn deno_module_setup_ready(&self, thread_id: &str) -> Result<bool, PedelecError> {
        self.thread_manager.thread(thread_id)?;
        Ok(self.deno_modules.get(thread_id).map_or(true, |modules| {
            modules
                .iter()
                .all(|module| module.state == DenoModuleSetupState::Ready)
        }))
    }

    pub fn ensure_deno_module_setup_ready(&self, thread_id: &str) -> Result<(), PedelecError> {
        if self.deno_module_setup_ready(thread_id)? {
            return Ok(());
        }
        let pending = self
            .deno_modules
            .get(thread_id)
            .into_iter()
            .flatten()
            .filter(|module| module.state != DenoModuleSetupState::Ready)
            .map(|module| module.name.clone())
            .collect::<Vec<_>>();
        Err(PedelecError::with_details(
            error_codes::DENO_MODULE_SETUP_INCOMPLETE,
            "Deno Module setup is incomplete",
            serde_json::json!({ "threadId": thread_id, "modules": pending }),
        ))
    }

    /// Validates the immutable private Deno snapshot owned by a thread and
    /// returns its canonical import-map path when modules are registered.
    /// This deliberately only inspects existing files: resume and execution
    /// admission must never recreate a missing snapshot from current app
    /// state.
    pub fn validate_deno_module_runtime_snapshot(
        &self,
        thread_id: &str,
    ) -> Result<Option<PathBuf>, PedelecError> {
        self.ensure_deno_module_setup_ready(thread_id)?;
        let modules = self
            .deno_modules
            .get(thread_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if modules.is_empty() {
            return Ok(None);
        }

        let workspace_path = self.thread_workspace_path(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_NOT_FOUND,
                "thread workspace was not found",
                serde_json::json!({ "threadId": thread_id }),
            )
        })?;
        validate_deno_module_runtime_snapshot_files(&workspace_path, thread_id, modules)
    }

    /// Aborts only the SDK initialization phase.  This deliberately does not
    /// use normal end-thread semantics: an aborted setup must not leave a
    /// resumable ended thread behind.
    pub fn abort_session_setup(
        &mut self,
        input: AbortSessionSetupInput,
    ) -> Result<(), PedelecError> {
        let Some(thread) = self.thread_manager.threads.get(&input.thread_id).cloned() else {
            return Ok(());
        };
        if thread.sdk_origin.is_none() {
            return Err(PedelecError::with_details(
                error_codes::THREAD_ACCESS_DENIED,
                "session setup abort is only valid for SDK-created threads",
                serde_json::json!({ "threadId": input.thread_id }),
            ));
        }
        if thread.status != ThreadStatus::Idle
            || self
                .pending_provider_operations
                .contains_key(&input.thread_id)
            || self
                .thread_manager
                .provider_state(&input.thread_id)
                .and_then(|state| state.active_provider_turn_id.as_ref())
                .is_some()
        {
            return Err(PedelecError::with_details(
                error_codes::THREAD_BUSY,
                "session setup cannot be aborted after provider work has started",
                serde_json::json!({ "threadId": input.thread_id }),
            ));
        }

        let event_log_path = self.event_bus.event_log_path(&input.thread_id);
        let module_temp_paths = self
            .deno_module_upload_tickets
            .iter()
            .filter(|(_, ticket)| ticket.thread_id == input.thread_id)
            .map(|(upload_id, ticket)| {
                workspace_tmp_root(&ticket.workspace_path)
                    .join(format!("{upload_id}.deno-module.upload"))
            })
            .collect::<Vec<_>>();

        self.pending_provider_operations.remove(&input.thread_id);
        self.last_completed_operations.remove(&input.thread_id);
        self.clear_active_provider_turn(&input.thread_id);
        self.tool_request_broker.clear_thread(&input.thread_id);
        self.tool_registry.remove(&input.thread_id);
        self.provider_usage.remove(&input.thread_id);
        self.session_usage.remove(&input.thread_id);
        self.session_usage_turn_baselines
            .retain(|(thread_id, _), _| thread_id != &input.thread_id);
        self.session_usage_operations
            .retain(|(thread_id, _)| thread_id != &input.thread_id);
        self.debug_reactivating_threads.remove(&input.thread_id);
        self.asset_upload_tickets
            .retain(|_, ticket| ticket.thread_id != input.thread_id);
        self.asset_download_tickets
            .retain(|_, ticket| ticket.thread_id != input.thread_id);
        self.deno_module_upload_tickets
            .retain(|_, ticket| ticket.thread_id != input.thread_id);
        self.deno_modules.remove(&input.thread_id);
        self.event_bus.remove_thread(&input.thread_id);
        self.thread_manager.remove_thread(&input.thread_id);

        for path in module_temp_paths {
            let _ = fs::remove_file(path);
        }
        if let Some(path) = event_log_path {
            let _ = fs::remove_file(path);
        }

        let workspace = self.workspace(&thread.workspace_id)?.clone();
        let cleanup_result = if workspace.kind == WorkspaceKind::Managed {
            self.workspace_manager
                .remove_managed_workspace_with_retry(&workspace.canonical_path)
                .map(|_| self.remove_workspace_resource(&workspace.workspace_id))
        } else {
            cleanup_thread_private_runtime_artifacts(&workspace.canonical_path, &thread.thread_id)
        };
        cleanup_result
    }

    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        list_provider_infos_with_scan(&self.provider_scan, self.provider_path_value())
    }

    pub fn list_sdk_providers(&self) -> Result<Vec<SdkProviderInfo>, PedelecError> {
        let default_provider = self.get_settings()?.default_provider;
        Ok(self
            .list_providers()
            .into_iter()
            .map(|provider| SdkProviderInfo::from_provider(provider, default_provider.as_ref()))
            .collect())
    }

    /// Returns only the executable selected and version-validated by the latest
    /// external provider scan. This intentionally does not resolve PATH again.
    pub fn provider_executable_path(
        &self,
        provider: &ProviderCode,
    ) -> Result<PathBuf, PedelecError> {
        if *provider == ProviderCode::Ollama {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_TERMINAL_UNSUPPORTED,
                "Ollama does not support opening a provider CLI Terminal.",
                serde_json::json!({"provider": "ollama", "platform": std::env::consts::OS}),
            ));
        }
        let Some(scan) = self.provider_scan.get(provider) else {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_TERMINAL_UNAVAILABLE,
                "The provider scan has not completed.",
                serde_json::json!({"provider": provider_code_as_str(provider), "platform": std::env::consts::OS}),
            ));
        };
        let Some(path) = scan
            .path
            .clone()
            .filter(|_| scan.version.is_some())
            .filter(|_| required_runtime_capability_is_available(provider, scan))
        else {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_TERMINAL_UNAVAILABLE,
                scan.error
                    .as_deref()
                    .unwrap_or("The provider CLI is not available from the latest scan."),
                serde_json::json!({
                    "provider": provider_code_as_str(provider),
                    "platform": std::env::consts::OS,
                    "requiredRuntimeCapability": required_runtime_capability(provider),
                    "runtimeCapabilityAvailable": runtime_capability_available(provider, scan),
                    "appServerCapability": (*provider == ProviderCode::Codex)
                        .then_some(scan.app_server_capability)
                        .flatten(),
                    "acpCapability": matches!(provider, ProviderCode::OpenCode | ProviderCode::Cursor)
                        .then_some(scan.acp_capability)
                        .flatten(),
                    "streamJsonCapability": matches!(
                        provider,
                        ProviderCode::Antigravity | ProviderCode::Claude
                    )
                    .then_some(scan.stream_json_capability)
                    .flatten(),
                    "workspaceCustomAgentCapability": (*provider == ProviderCode::Antigravity)
                        .then(|| antigravity_custom_agent_capability_available(scan))
                        .flatten(),
                }),
            ));
        };
        if !is_provider_executable(&path) {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_TERMINAL_UNAVAILABLE,
                "The scanned provider executable is no longer available.",
                serde_json::json!({"provider": provider_code_as_str(provider), "platform": std::env::consts::OS, "executablePath": path}),
            ));
        }
        Ok(path)
    }

    /// Materializes the static Antigravity workspace agent required by the
    /// persistent `--agent pedelec-runtime` launch contract. Dispatchers call
    /// this before spawning a fresh AGY process so a clean workspace cannot
    /// race process startup against bootstrap asset creation.
    pub fn prepare_antigravity_persistent_workspace(
        &self,
        workspace_path: &Path,
    ) -> Result<(), PedelecError> {
        ensure_antigravity_custom_agent(workspace_path)
    }

    /// Replaces the complete external-provider scan only after every provider has
    /// been inspected, so concurrent callers never observe a partial refresh.
    pub fn refresh_providers(&mut self) {
        let is_initial_scan = self.provider_readiness.is_uninitialized();
        if is_initial_scan {
            self.provider_readiness.mark_initial_scanning();
        }
        let path_value = self.resolve_provider_path_value();
        self.provider_resolved_path = Some(path_value.clone());
        self.provider_scan = scan_external_providers(Some(path_value));
        if is_initial_scan {
            self.provider_readiness.mark_ready();
        }
    }

    pub fn get_settings(&self) -> Result<PedelecSettings, PedelecError> {
        read_settings_file(&self.resolved_settings_file_path()?)
    }

    pub fn get_sdk_settings(&self) -> Result<SdkSettings, PedelecError> {
        self.get_settings().map(SdkSettings::from)
    }

    pub fn update_settings(
        &mut self,
        input: UpdateSettingsInput,
    ) -> Result<PedelecSettings, PedelecError> {
        let existing_settings = read_settings_file(&self.resolved_settings_file_path()?)?;
        #[cfg(test)]
        let settings = if self.provider_readiness.is_uninitialized() {
            normalize_update_settings_for_test(input, self.provider_path_value().as_ref())?
        } else {
            normalize_update_settings(input, &self.provider_scan)?
        };
        #[cfg(not(test))]
        let settings = normalize_update_settings(input, &self.provider_scan)?;
        let mut settings = settings;
        settings.wizard_metadata = existing_settings.wizard_metadata;
        write_settings_file(&self.resolved_settings_file_path()?, &settings)?;
        Ok(settings)
    }

    pub fn list_ollama_models(
        &self,
        input: ListOllamaModelsInput,
    ) -> Result<Vec<OllamaModelOption>, PedelecError> {
        list_ollama_models(input)
    }

    pub fn check_ollama_connection(
        &self,
        input: CheckOllamaConnectionInput,
    ) -> CheckOllamaConnectionOutput {
        check_ollama_connection(input)
    }

    fn provider_path_value(&self) -> Option<OsString> {
        if let Some(path) = &self.provider_path_value_override {
            return Some(path.clone());
        }

        if let Some(path) = &self.provider_resolved_path {
            return Some(path.clone());
        }

        Some(merged_provider_path(env::var_os("PATH")))
    }

    fn resolve_provider_path_value(&self) -> OsString {
        self.provider_path_value_override
            .clone()
            .unwrap_or_else(resolve_provider_path_value)
    }

    fn resolved_settings_file_path(&self) -> Result<PathBuf, PedelecError> {
        if let Some(path) = &self.settings_file_path {
            return Ok(path.clone());
        }
        default_settings_file_path()
    }

    fn next_available_thread_id(
        &mut self,
        custom_workspace: Option<&Path>,
    ) -> Result<String, PedelecError> {
        loop {
            let thread_id = self.thread_manager.next_thread_id()?;
            if self.thread_manager.contains_thread(&thread_id) {
                continue;
            }
            if let Some(workspace) = custom_workspace {
                if workspace_deno_thread_root_occupied(workspace, &thread_id) {
                    continue;
                }
            }
            return Ok(thread_id);
        }
    }

    /// Admits a user turn and returns the persistent runtime operation for
    /// it. This is the Core boundary used by IPC and provider runtimes.
    pub fn begin_send_text_intent(
        &mut self,
        input: SendTextInput,
    ) -> Result<ProviderExecutionStart, PedelecError> {
        self.validate_normal_send_text_status(&input.thread_id)?;
        self.begin_send_text_intent_start(input, None)
    }

    /// Diagnostic variant of [`Self::begin_send_text_intent`]. It preserves
    /// the trusted ended-thread reactivation and event-log behavior while
    /// allowing a persistent provider to receive a semantic turn intent.
    pub fn begin_debug_send_text_intent(
        &mut self,
        input: SendTextInput,
    ) -> Result<ProviderExecutionStart, PedelecError> {
        let reactivate = self.validate_debug_send_text_status(&input.thread_id)?;
        if !reactivate {
            return self.begin_send_text_intent(input);
        }

        let thread_id = input.thread_id.clone();
        let event_log_path = self.restore_ended_thread_runtime(&thread_id)?;
        let result = self.begin_send_text_intent_start(input, Some(event_log_path));
        if result.is_err() {
            self.tool_registry.remove(&thread_id);
            self.debug_reactivating_threads.remove(&thread_id);
        }
        result
    }

    fn validate_normal_send_text_status(&self, thread_id: &str) -> Result<(), PedelecError> {
        self.ensure_workspace_runs_idle_for_thread(thread_id)?;
        self.ensure_deno_module_setup_ready(thread_id)?;
        let thread = self.thread_manager.thread(thread_id)?;
        match thread.status {
            ThreadStatus::Running | ThreadStatus::WaitingToolResult => {
                Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is already running",
                    serde_json::json!({ "threadId": thread_id }),
                ))
            }
            ThreadStatus::Ended => Err(PedelecError::with_details(
                error_codes::THREAD_ENDED,
                "thread has ended",
                serde_json::json!({ "threadId": thread_id }),
            )),
            ThreadStatus::Error => Err(PedelecError::with_details(
                error_codes::PROVIDER_COMMAND_FAILED,
                "thread is in error state",
                serde_json::json!({ "threadId": thread_id }),
            )),
            ThreadStatus::Stopping => Err(PedelecError::with_details(
                error_codes::THREAD_BUSY,
                "thread is stopping",
                serde_json::json!({ "threadId": thread_id }),
            )),
            _ => Ok(()),
        }
    }

    fn validate_debug_send_text_status(&self, thread_id: &str) -> Result<bool, PedelecError> {
        self.ensure_workspace_runs_idle_for_thread(thread_id)?;
        self.ensure_deno_module_setup_ready(thread_id)?;
        let thread = self.thread_manager.thread(thread_id)?;
        match thread.status {
            ThreadStatus::Idle => Ok(false),
            ThreadStatus::Ended => Ok(true),
            ThreadStatus::Running | ThreadStatus::WaitingToolResult => {
                Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is already running",
                    serde_json::json!({ "threadId": thread_id }),
                ))
            }
            ThreadStatus::Stopping => Err(PedelecError::with_details(
                error_codes::THREAD_BUSY,
                "thread is stopping",
                serde_json::json!({ "threadId": thread_id }),
            )),
            ThreadStatus::Error => Err(PedelecError::with_details(
                error_codes::PROVIDER_COMMAND_FAILED,
                "thread is in error state",
                serde_json::json!({ "threadId": thread_id }),
            )),
            ThreadStatus::Starting => Err(PedelecError::with_details(
                error_codes::THREAD_BUSY,
                "thread is already starting",
                serde_json::json!({ "threadId": thread_id }),
            )),
        }
    }

    fn restore_ended_thread_runtime(&mut self, thread_id: &str) -> Result<PathBuf, PedelecError> {
        let (registry, event_log_path) = self.load_thread_runtime(thread_id, false)?;
        self.tool_registry.insert(thread_id.to_string(), registry);
        self.debug_reactivating_threads
            .insert(thread_id.to_string());
        Ok(event_log_path)
    }

    fn load_thread_runtime(
        &self,
        thread_id: &str,
        validate_workspace: bool,
    ) -> Result<(ToolRegistry, PathBuf), PedelecError> {
        let workspace_path = self.thread_workspace_path(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_NOT_FOUND,
                "thread workspace was not found",
                serde_json::json!({ "threadId": thread_id }),
            )
        })?;

        if validate_workspace {
            let metadata = fs::metadata(&workspace_path).map_err(|err| {
                workspace_open_error(
                    thread_id,
                    &workspace_path,
                    "cannot open recorded workspace",
                    err,
                )
            })?;
            if !metadata.is_dir() {
                return Err(workspace_open_error(
                    thread_id,
                    &workspace_path,
                    "recorded workspace is not a directory",
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "recorded workspace path is not a directory",
                    ),
                ));
            }
        }

        let skills_root = thread_skills_root(&workspace_path, thread_id);
        if validate_workspace {
            let metadata = fs::symlink_metadata(&skills_root).map_err(|err| {
                workspace_open_error(
                    thread_id,
                    &workspace_path,
                    "cannot open recorded thread skills",
                    err,
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(workspace_open_error(
                    thread_id,
                    &workspace_path,
                    "recorded thread skills path is not a directory",
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "recorded thread skills path is not a directory",
                    ),
                ));
            }
        }
        let registry = ToolRegistry::load_from_skills_dir(skills_root)?;
        Ok((registry, thread_event_log_path(&workspace_path, thread_id)))
    }

    fn begin_send_text_intent_start(
        &mut self,
        input: SendTextInput,
        reactivated_event_log_path: Option<PathBuf>,
    ) -> Result<ProviderExecutionStart, PedelecError> {
        let session = match self.build_persistent_session_intent(&input.thread_id) {
            Ok(session) => session,
            Err(error) => {
                if reactivated_event_log_path.is_some() {
                    self.tool_registry.remove(&input.thread_id);
                    self.debug_reactivating_threads.remove(&input.thread_id);
                }
                return Err(error);
            }
        };
        let provider_session_id = session.provider_session_id.clone();
        let operation_id = resolve_operation_id(input.operation_id)?;
        let local_turn_id = self.mark_user_turn_started(
            &input.thread_id,
            operation_id.clone(),
            reactivated_event_log_path,
        )?;
        Ok(ProviderExecutionStart {
            output: SendTextOutput {
                thread_id: input.thread_id.clone(),
                operation_id,
            },
            intent: PersistentRuntimeOperation::StartTurn {
                turn: PersistentProviderTurnIntent {
                    thread_id: input.thread_id,
                    local_turn_id,
                    provider_session_id,
                    message: input.message,
                    session,
                },
            },
        })
    }

    fn mark_user_turn_started(
        &mut self,
        thread_id: &str,
        operation_id: String,
        reactivated_event_log_path: Option<PathBuf>,
    ) -> Result<String, PedelecError> {
        {
            let thread = self.thread_manager.thread_mut(thread_id)?;
            thread.status = ThreadStatus::Running;
            thread.updated_at = Utc::now();
        }
        if let Some(event_log_path) = reactivated_event_log_path {
            self.event_bus
                .register_thread_log(thread_id, event_log_path);
        }
        let local_turn_id = new_provider_turn_id();
        if let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) {
            provider_state.active_provider_turn_id = Some(local_turn_id.clone());
        }
        self.pending_provider_operations.insert(
            thread_id.to_string(),
            PendingProviderOperation::user(operation_id.clone()),
        );
        self.event_bus.emit_status_changed_for_operation(
            thread_id,
            ThreadStatus::Running,
            Some(&operation_id),
        );
        Ok(local_turn_id)
    }

    /// Marks a trusted diagnostic turn as admitted by its provider dispatcher.
    /// Until this point a dispatch failure must roll the thread back to Ended.
    pub fn complete_provider_execution_dispatch(&mut self, thread_id: &str) {
        self.debug_reactivating_threads.remove(thread_id);
    }

    /// Admits a prepare operation. Persistent providers receive an
    /// `EnsureSession` intent rather than a synthetic user turn.
    pub fn begin_prepare_thread_intent(
        &mut self,
        input: PrepareThreadInput,
    ) -> Result<PrepareExecutionStart, PedelecError> {
        self.ensure_workspace_runs_idle_for_thread(&input.thread_id)?;
        self.ensure_deno_module_setup_ready(&input.thread_id)?;
        {
            let thread = self.thread_manager.thread(&input.thread_id)?;
            match thread.status {
                ThreadStatus::Running
                | ThreadStatus::WaitingToolResult
                | ThreadStatus::Starting => {
                    return Err(PedelecError::with_details(
                        error_codes::THREAD_BUSY,
                        "thread is already running",
                        serde_json::json!({ "threadId": input.thread_id }),
                    ));
                }
                ThreadStatus::Ended => {
                    return Err(PedelecError::with_details(
                        error_codes::THREAD_ENDED,
                        "thread has ended",
                        serde_json::json!({ "threadId": input.thread_id }),
                    ));
                }
                ThreadStatus::Stopping => {
                    return Err(PedelecError::with_details(
                        error_codes::THREAD_BUSY,
                        "thread is stopping",
                        serde_json::json!({ "threadId": input.thread_id }),
                    ));
                }
                _ => {}
            }
        }

        let operation_id = resolve_operation_id(input.operation_id)?;
        let session = self.build_persistent_session_intent(&input.thread_id)?;
        let thread_id = input.thread_id;
        let thread = self.thread_manager.thread_mut(&thread_id)?;
        thread.status = ThreadStatus::Running;
        thread.updated_at = Utc::now();
        self.pending_provider_operations.insert(
            thread_id.clone(),
            PendingProviderOperation::prepare(operation_id.clone()),
        );
        self.event_bus.emit_status_changed_for_operation(
            &thread_id,
            ThreadStatus::Running,
            Some(&operation_id),
        );

        Ok(PrepareExecutionStart {
            output: PrepareThreadOutput {
                thread_id: thread_id.clone(),
                operation_id,
                prepared: true,
                already_prepared: Some(false),
            },
            intent: Some(PersistentRuntimeOperation::EnsureSession { session }),
        })
    }

    fn build_persistent_session_intent(
        &self,
        thread_id: &str,
    ) -> Result<PersistentProviderSessionIntent, PedelecError> {
        // Provider startup/reconnect must observe the same immutable module
        // snapshot as `pedelec-deno`; a Ready state alone is not sufficient if
        // the package or import map was removed or corrupted meanwhile.
        self.validate_deno_module_runtime_snapshot(thread_id)?;
        let thread = self.thread_manager.thread(thread_id)?.clone();
        let provider_state = self
            .thread_manager
            .provider_state(thread_id)
            .cloned()
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_NOT_FOUND,
                    "provider session state was not found for thread",
                    serde_json::json!({ "threadId": thread_id }),
                )
            })?;
        let registry = self.tool_registry.get(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOL_NOT_FOUND,
                "tool registry was not found for thread",
                serde_json::json!({ "threadId": thread_id }),
            )
        })?;
        let workspace_path = self.thread_workspace_path(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_NOT_FOUND,
                "thread workspace was not found",
                serde_json::json!({ "threadId": thread_id }),
            )
        })?;
        let (model, reasoning_effort, antigravity_reasoning_effort, claude_reasoning_effort) =
            match &thread.provider {
                ProviderCode::Codex => {
                    let (model, effort) =
                        parse_codex_session_settings(&thread.effort_args, thread_id)?;
                    (model, effort, None, None)
                }
                ProviderCode::Antigravity => {
                    let (model, effort) =
                        parse_antigravity_session_settings(&thread.effort_args, thread_id)?;
                    (model, None, effort, None)
                }
                ProviderCode::Claude => {
                    let (model, effort) =
                        parse_claude_session_settings(&thread.effort_args, thread_id)?;
                    (model, None, None, effort)
                }
                ProviderCode::Ollama => (Some(required_ollama_model(&thread)?), None, None, None),
                _ => (
                    provider_model_from_effort_args(&thread.provider, &thread.effort_args),
                    None,
                    None,
                    None,
                ),
            };
        let host_instructions = build_persistent_host_instructions_with_modules(
            &thread,
            &workspace_path,
            registry,
            self.deno_modules
                .get(thread_id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        let core_ipc_runtime_file_path = self
            .core_ipc_runtime_file_path
            .clone()
            .unwrap_or_else(default_runtime_file_path_for_provider);

        Ok(PersistentProviderSessionIntent {
            thread_id: thread.thread_id,
            provider: thread.provider.clone(),
            provider_session_id: provider_state.provider_session_id,
            workspace_path,
            effort_level: thread.effort_level,
            model,
            reasoning_effort,
            antigravity_reasoning_effort,
            claude_reasoning_effort,
            approval_policy: PersistentApprovalPolicy::Never,
            sandbox_policy: PersistentSandboxPolicy::ReadOnly,
            host_instructions,
            config: if thread.provider == ProviderCode::Codex {
                HashMap::from([
                    (
                        CODEX_SKILLS_INCLUDE_INSTRUCTIONS_KEY.to_string(),
                        Value::Bool(false),
                    ),
                    (
                        CODEX_PROJECT_DOC_MAX_BYTES_KEY.to_string(),
                        Value::from(0_u64),
                    ),
                    (
                        CODEX_INCLUDE_PERMISSIONS_INSTRUCTIONS_KEY.to_string(),
                        Value::Bool(false),
                    ),
                    (
                        CODEX_INCLUDE_APPS_INSTRUCTIONS_KEY.to_string(),
                        Value::Bool(false),
                    ),
                    (
                        CODEX_INCLUDE_COLLABORATION_MODE_INSTRUCTIONS_KEY.to_string(),
                        Value::Bool(false),
                    ),
                    (CODEX_FEATURES_PLUGINS_KEY.to_string(), Value::Bool(false)),
                    (CODEX_FEATURES_APPS_KEY.to_string(), Value::Bool(false)),
                ])
            } else {
                HashMap::new()
            },
            core_ipc_runtime_file_path,
            tools: registry.tools().cloned().collect(),
            guidance: registry.guidance().map(ToOwned::to_owned),
        })
    }

    /// Reduce a normalized persistent-runtime event into Pedelec state. This
    /// method only performs short state/event mutations; provider I/O belongs
    /// to the runtime executor that produced the event.
    pub fn reduce_provider_runtime_event(
        &mut self,
        event: ProviderRuntimeEvent,
    ) -> Result<(), PedelecError> {
        match event {
            ProviderRuntimeEvent::SessionReady {
                thread_id,
                provider_session_id,
            } => self.reduce_runtime_session_ready(&thread_id, provider_session_id),
            ProviderRuntimeEvent::TurnStarted {
                thread_id,
                provider_turn_id,
            } => self.reduce_runtime_turn_started(&thread_id, provider_turn_id),
            ProviderRuntimeEvent::AssistantDelta {
                thread_id,
                provider_turn_id,
                text,
            } => {
                self.validate_runtime_turn(&thread_id, provider_turn_id.as_deref())?;
                let operation_id = self.active_operation_id(&thread_id);
                self.event_bus.emit_assistant_delta_for_operation(
                    &thread_id,
                    text,
                    operation_id.as_deref(),
                );
                Ok(())
            }
            ProviderRuntimeEvent::AssistantMessage {
                thread_id,
                provider_turn_id,
                text,
            } => {
                self.validate_runtime_turn(&thread_id, provider_turn_id.as_deref())?;
                let operation_id = self.active_operation_id(&thread_id);
                self.event_bus.emit_assistant_message_for_operation(
                    &thread_id,
                    text,
                    operation_id.as_deref(),
                );
                Ok(())
            }
            ProviderRuntimeEvent::UsageUpdated {
                thread_id,
                provider_turn_id,
                usage,
            } => {
                self.validate_runtime_turn(&thread_id, provider_turn_id.as_deref())?;
                self.provider_usage.insert(thread_id, usage);
                Ok(())
            }
            ProviderRuntimeEvent::TurnCompleted {
                thread_id,
                provider_turn_id,
                success,
                error,
            } => {
                let prepare_without_turn = self
                    .pending_provider_operations
                    .get(&thread_id)
                    .is_some_and(|operation| {
                        operation.kind() == PendingProviderOperationKind::Prepare
                    })
                    && self
                        .thread_manager
                        .provider_session_state(&thread_id)
                        .and_then(|state| state.active_provider_turn_id.as_ref())
                        .is_none();
                if !prepare_without_turn {
                    self.validate_runtime_turn_allow_stopping(
                        &thread_id,
                        provider_turn_id.as_deref(),
                    )?;
                }
                if success {
                    self.finish_persistent_operation(&thread_id, true, None)
                } else {
                    let error = error.unwrap_or_else(|| {
                        PedelecError::new(
                            error_codes::PROVIDER_REQUEST_FAILED,
                            "provider turn failed",
                        )
                    });
                    self.finish_persistent_operation(&thread_id, false, Some(error))
                }
            }
            ProviderRuntimeEvent::ProviderError {
                thread_id,
                provider_turn_id,
                error,
            } => {
                let prepare_without_turn = self
                    .pending_provider_operations
                    .get(&thread_id)
                    .is_some_and(|operation| {
                        operation.kind() == PendingProviderOperationKind::Prepare
                    })
                    && self
                        .thread_manager
                        .provider_session_state(&thread_id)
                        .and_then(|state| state.active_provider_turn_id.as_ref())
                        .is_none();
                if !prepare_without_turn {
                    self.validate_runtime_turn(&thread_id, provider_turn_id.as_deref())?;
                }
                self.finish_persistent_operation(&thread_id, false, Some(error))
            }
            ProviderRuntimeEvent::RuntimeDisconnected { thread_id, error } => {
                if let Some(thread_id) = thread_id {
                    self.thread_manager.thread(&thread_id)?;
                    self.fail_persistent_runtime_thread(&thread_id, &error);
                } else {
                    for thread_id in self.thread_manager.thread_ids() {
                        self.fail_persistent_runtime_thread(&thread_id, &error);
                    }
                }
                Ok(())
            }
        }
    }

    fn reduce_runtime_session_ready(
        &mut self,
        thread_id: &str,
        provider_session_id: String,
    ) -> Result<(), PedelecError> {
        if provider_session_id.trim().is_empty() {
            return Err(runtime_protocol_error(
                thread_id,
                "persistent provider returned an empty session id",
            ));
        }
        let status = self.thread_manager.thread(thread_id)?.status.clone();
        if matches!(status, ThreadStatus::Stopping | ThreadStatus::Ended) {
            return Err(runtime_protocol_error(
                thread_id,
                "session ready event targets a thread that is stopping or ended",
            ));
        }
        let operation_id = self.active_operation_id(thread_id);
        self.update_provider_session_id_for_operation(
            thread_id,
            provider_session_id,
            operation_id.as_deref(),
        );

        if self
            .pending_provider_operations
            .get(thread_id)
            .is_some_and(|operation| operation.kind() == PendingProviderOperationKind::Prepare)
        {
            return self.finish_persistent_operation(thread_id, true, None);
        }
        Ok(())
    }

    fn reduce_runtime_turn_started(
        &mut self,
        thread_id: &str,
        provider_turn_id: String,
    ) -> Result<(), PedelecError> {
        if provider_turn_id.trim().is_empty() {
            return Err(runtime_protocol_error(
                thread_id,
                "persistent provider returned an empty turn id",
            ));
        }
        let thread = self.thread_manager.thread(thread_id)?;
        if !matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingToolResult
        ) {
            return Err(runtime_protocol_error(
                thread_id,
                "turn started for a thread that is not running",
            ));
        }
        if self
            .pending_provider_operations
            .get(thread_id)
            .is_none_or(|operation| operation.kind() != PendingProviderOperationKind::UserTurn)
        {
            return Err(runtime_protocol_error(
                thread_id,
                "turn started without an admitted user turn",
            ));
        }
        let active = self
            .thread_manager
            .provider_state(thread_id)
            .and_then(|state| state.active_provider_turn_id.as_deref())
            .ok_or_else(|| runtime_protocol_error(thread_id, "active provider turn is missing"))?;
        if active.starts_with("local_") || active == provider_turn_id {
            if let Some(state) = self.thread_manager.provider_state_mut(thread_id) {
                state.active_provider_turn_id = Some(provider_turn_id);
            }
            Ok(())
        } else {
            Err(runtime_protocol_error(
                thread_id,
                "provider turn id does not match the active turn",
            ))
        }
    }

    fn validate_runtime_turn(
        &self,
        thread_id: &str,
        provider_turn_id: Option<&str>,
    ) -> Result<(), PedelecError> {
        self.thread_manager.thread(thread_id)?;
        let state = self
            .thread_manager
            .provider_state(thread_id)
            .ok_or_else(|| {
                runtime_protocol_error(thread_id, "provider session state is missing")
            })?;
        if !matches!(
            self.thread_manager.thread(thread_id)?.status,
            ThreadStatus::Running | ThreadStatus::WaitingToolResult
        ) {
            return Err(runtime_protocol_error(
                thread_id,
                "runtime event targets an inactive thread",
            ));
        }
        let active = state.active_provider_turn_id.as_deref().ok_or_else(|| {
            runtime_protocol_error(thread_id, "runtime event has no active provider turn")
        })?;
        if let Some(provider_turn_id) = provider_turn_id {
            if active != provider_turn_id && !active.starts_with("local_") {
                return Err(runtime_protocol_error(
                    thread_id,
                    "runtime event turn id does not match the active turn",
                ));
            }
        }
        Ok(())
    }

    fn validate_runtime_turn_allow_stopping(
        &self,
        thread_id: &str,
        provider_turn_id: Option<&str>,
    ) -> Result<(), PedelecError> {
        self.thread_manager.thread(thread_id)?;
        let state = self
            .thread_manager
            .provider_session_state(thread_id)
            .ok_or_else(|| {
                runtime_protocol_error(thread_id, "provider session state is missing")
            })?;
        if !matches!(
            self.thread_manager.thread(thread_id)?.status,
            ThreadStatus::Running | ThreadStatus::WaitingToolResult | ThreadStatus::Stopping
        ) {
            return Err(runtime_protocol_error(
                thread_id,
                "runtime event targets an inactive thread",
            ));
        }
        let active = state.active_provider_turn_id.as_deref().ok_or_else(|| {
            runtime_protocol_error(thread_id, "runtime event has no active provider turn")
        })?;
        if let Some(provider_turn_id) = provider_turn_id {
            if active != provider_turn_id && !active.starts_with("local_") {
                return Err(runtime_protocol_error(
                    thread_id,
                    "runtime event turn id does not match the active turn",
                ));
            }
        }
        Ok(())
    }

    fn finish_persistent_operation(
        &mut self,
        thread_id: &str,
        success: bool,
        error: Option<PedelecError>,
    ) -> Result<(), PedelecError> {
        let operation = self
            .pending_provider_operations
            .remove(thread_id)
            .ok_or_else(|| runtime_protocol_error(thread_id, "provider operation is not active"))?;
        let operation_id = operation.operation_id.clone();
        let operation_kind = match operation.kind() {
            PendingProviderOperationKind::UserTurn => ThreadOperationKind::User,
            PendingProviderOperationKind::Prepare => ThreadOperationKind::Prepare,
        };
        self.clear_active_provider_turn(thread_id);
        if let Some(error) = error.as_ref() {
            self.tool_request_broker
                .clear_thread_with_error(thread_id, error.clone());
        } else {
            self.tool_request_broker.clear_thread(thread_id);
        }

        let stopping = self
            .thread_manager
            .thread(thread_id)
            .map(|thread| thread.status == ThreadStatus::Stopping)
            .unwrap_or(false);
        if stopping {
            let terminal_error = error.or_else(|| {
                Some(PedelecError::new(
                    error_codes::THREAD_ENDED,
                    "thread ended while the operation was active",
                ))
            });
            self.event_bus.emit_operation_completed(
                thread_id,
                &operation_id,
                operation_kind,
                false,
                terminal_error.clone(),
            );
            self.last_completed_operations.insert(
                thread_id.to_string(),
                CompletedOperationSnapshot {
                    operation_id,
                    operation_kind,
                    success: false,
                    error: terminal_error,
                    completed_at: Utc::now(),
                },
            );
            return Ok(());
        }

        let next_status = if success || operation.kind() == PendingProviderOperationKind::Prepare {
            ThreadStatus::Idle
        } else {
            ThreadStatus::Error
        };
        if let Some(error) = error.clone() {
            self.emit_thread_provider_error_for_operation(thread_id, error, Some(&operation_id));
        }
        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            thread.status = next_status.clone();
            thread.updated_at = Utc::now();
        }
        self.event_bus.emit_status_changed_for_operation(
            thread_id,
            next_status.clone(),
            Some(&operation_id),
        );
        self.event_bus.emit_operation_completed(
            thread_id,
            &operation_id,
            operation_kind,
            success,
            error.clone(),
        );
        self.last_completed_operations.insert(
            thread_id.to_string(),
            CompletedOperationSnapshot {
                operation_id,
                operation_kind,
                success,
                error,
                completed_at: Utc::now(),
            },
        );
        Ok(())
    }

    pub fn fail_provider_execution_dispatch(
        &mut self,
        thread_id: &str,
        operation: ProviderExecutionOperationKind,
        error: PedelecError,
    ) {
        if operation == ProviderExecutionOperationKind::End {
            let _ = self.finish_end_thread(thread_id);
            return;
        }
        if self.debug_reactivating_threads.contains(thread_id) {
            let _ = self.finish_persistent_operation(thread_id, false, Some(error));
            let _ = self.rollback_debug_reactivation(thread_id);
            return;
        }
        if operation != ProviderExecutionOperationKind::End {
            let _ = self.finish_persistent_operation(thread_id, false, Some(error));
        }
    }

    /// Applies a fatal failure to the Pedelec threads that belong to one
    /// persistent provider runtime. Idle threads retain their provider
    /// session identity and remain resumable; only active semantic work is
    /// failed. A thread being explicitly ended is finalized locally because
    /// the end operation has already taken ownership of its cleanup.
    pub fn fail_persistent_runtime(&mut self, provider: ProviderCode, error: PedelecError) {
        self.fail_persistent_runtime_except(provider, None, error);
    }

    /// Applies a runtime-generation failure while preserving one operation
    /// that has already been admitted for the replacement generation. The
    /// dispatcher uses this during the small race between observing the old
    /// generation as unhealthy and starting a new one.
    pub fn fail_persistent_runtime_except(
        &mut self,
        provider: ProviderCode,
        excluded_thread_id: Option<&str>,
        error: PedelecError,
    ) {
        let thread_ids = self.thread_manager.thread_ids();
        for thread_id in thread_ids {
            if excluded_thread_id == Some(thread_id.as_str()) {
                continue;
            }
            let belongs_to_runtime = self
                .thread_manager
                .thread(&thread_id)
                .map(|thread| thread.provider == provider)
                .unwrap_or(false);
            if !belongs_to_runtime {
                continue;
            }
            self.fail_persistent_runtime_thread(&thread_id, &error);
        }
    }

    fn fail_persistent_runtime_thread(&mut self, thread_id: &str, error: &PedelecError) {
        let Ok(status) = self
            .thread_manager
            .thread(thread_id)
            .map(|thread| thread.status.clone())
        else {
            return;
        };
        if matches!(status, ThreadStatus::Ended | ThreadStatus::Idle) {
            self.pending_provider_operations.remove(thread_id);
            self.clear_active_provider_turn(thread_id);
            self.tool_request_broker.clear_thread(thread_id);
            return;
        }
        if status == ThreadStatus::Error
            && !self.pending_provider_operations.contains_key(thread_id)
            && self
                .thread_manager
                .provider_session_state(thread_id)
                .and_then(|state| state.active_provider_turn_id.as_ref())
                .is_none()
        {
            return;
        }
        if status == ThreadStatus::Stopping {
            self.tool_request_broker
                .clear_thread_with_error(thread_id, error.clone());
            let _ = self.finish_end_thread(thread_id);
            return;
        }
        if self
            .pending_provider_operations
            .get(thread_id)
            .is_some_and(|operation| operation.kind() == PendingProviderOperationKind::Prepare)
        {
            let _ = self.finish_persistent_operation(thread_id, false, Some(error.clone()));
            return;
        }

        if self.pending_provider_operations.contains_key(thread_id) {
            let _ = self.finish_persistent_operation(thread_id, false, Some(error.clone()));
        } else {
            self.tool_request_broker
                .clear_thread_with_error(thread_id, error.clone());
            if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
                thread.status = ThreadStatus::Error;
                thread.updated_at = Utc::now();
            }
            self.event_bus
                .emit_status_changed(thread_id, ThreadStatus::Error);
            self.emit_thread_provider_error(thread_id, error.clone());
        }
    }

    pub fn provider_usage(&self, thread_id: &str) -> Option<&Value> {
        self.provider_usage.get(thread_id)
    }

    /// Returns the normalized cumulative usage currently known for a thread.
    pub fn session_usage(&self, thread_id: &str) -> Option<&SessionUsage> {
        self.session_usage.get(thread_id)
    }

    /// Returns the current normalized cumulative token total, if one is known.
    pub fn session_total_tokens(&self, thread_id: &str) -> Option<u64> {
        self.session_usage(thread_id)
            .map(|usage| usage.total_tokens)
    }

    /// Publishes an already-normalized cumulative session total.
    ///
    /// The value is monotonic: stale or repeated provider snapshots are
    /// accepted without changing Core state or emitting a duplicate event.
    pub fn set_session_total_tokens(
        &mut self,
        thread_id: &str,
        total_tokens: u64,
    ) -> Result<bool, PedelecError> {
        self.thread_manager.thread(thread_id)?;
        if self
            .session_total_tokens(thread_id)
            .is_some_and(|current| total_tokens <= current)
        {
            return Ok(false);
        }

        self.session_usage
            .insert(thread_id.to_string(), SessionUsage { total_tokens });
        self.event_bus.emit_usage_updated(thread_id, total_tokens);
        Ok(true)
    }

    /// Starts a provider turn whose usage is cumulative within that turn.
    /// The baseline is retained by Core so runtime replacement cannot reset
    /// the accounting state.
    pub fn begin_session_usage_turn(
        &mut self,
        thread_id: &str,
        provider_turn_id: &str,
    ) -> Result<(), PedelecError> {
        self.thread_manager.thread(thread_id)?;
        let baseline = self.session_total_tokens(thread_id).unwrap_or(0);
        self.session_usage_turn_baselines.insert(
            (thread_id.to_string(), provider_turn_id.to_string()),
            baseline,
        );
        Ok(())
    }

    /// Publishes the latest cumulative usage for one active provider turn.
    /// The provider-specific adapter owns the meaning of `turn_total_tokens`.
    pub fn set_session_turn_total_tokens(
        &mut self,
        thread_id: &str,
        provider_turn_id: &str,
        turn_total_tokens: u64,
    ) -> Result<bool, PedelecError> {
        self.thread_manager.thread(thread_id)?;
        let key = (thread_id.to_string(), provider_turn_id.to_string());
        let baseline = if let Some(baseline) = self.session_usage_turn_baselines.get(&key) {
            *baseline
        } else {
            let baseline = self.session_total_tokens(thread_id).unwrap_or(0);
            self.session_usage_turn_baselines.insert(key, baseline);
            baseline
        };
        self.set_session_total_tokens(thread_id, baseline.saturating_add(turn_total_tokens))
    }

    /// Adds a per-operation usage contribution at most once.
    ///
    /// Provider adapters use their operation/turn identity for deduplication;
    /// Core only performs the atomic state update and event emission.
    pub fn add_session_token_delta_once(
        &mut self,
        thread_id: &str,
        operation_id: &str,
        total_tokens: u64,
    ) -> Result<bool, PedelecError> {
        self.thread_manager.thread(thread_id)?;
        let key = (thread_id.to_string(), operation_id.to_string());
        if !self.session_usage_operations.insert(key) {
            return Ok(false);
        }
        let current = self.session_total_tokens(thread_id).unwrap_or(0);
        self.set_session_total_tokens(thread_id, current.saturating_add(total_tokens))
    }

    /// Returns the Core operation currently associated with a thread.
    /// Provider adapters use this to associate terminal usage with the
    /// operation whose contribution is being accounted.
    pub fn current_operation_id(&self, thread_id: &str) -> Option<String> {
        self.active_operation_id(thread_id)
    }

    fn update_provider_session_id_for_operation(
        &mut self,
        thread_id: &str,
        provider_session_id: String,
        operation_id: Option<&str>,
    ) {
        let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) else {
            return;
        };
        if provider_state.provider_session_id.as_deref() == Some(provider_session_id.as_str()) {
            return;
        }
        provider_state.provider_session_id = Some(provider_session_id.clone());
        self.event_bus
            .emit_provider_session_id_updated_for_operation(
                thread_id,
                provider_session_id,
                operation_id,
            );
    }

    fn emit_thread_provider_error(&mut self, thread_id: &str, error: PedelecError) {
        let operation_id = self.active_operation_id(thread_id);
        self.emit_thread_provider_error_for_operation(thread_id, error, operation_id.as_deref());
    }

    fn emit_thread_provider_error_for_operation(
        &mut self,
        thread_id: &str,
        error: PedelecError,
        operation_id: Option<&str>,
    ) {
        let Ok(thread) = self.thread_manager.thread(thread_id) else {
            return;
        };
        self.event_bus.emit_provider_error_for_operation(
            thread_id,
            thread.provider.clone(),
            error,
            operation_id,
        );
    }

    fn active_operation_id(&self, thread_id: &str) -> Option<String> {
        self.pending_provider_operations
            .get(thread_id)
            .map(|operation| operation.operation_id.clone())
    }

    fn clear_active_provider_turn(&mut self, thread_id: &str) {
        if let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) {
            provider_state.active_provider_turn_id = None;
        }
    }

    fn rollback_debug_reactivation(&mut self, thread_id: &str) -> bool {
        if !self.debug_reactivating_threads.remove(thread_id) {
            return false;
        }
        self.pending_provider_operations.remove(thread_id);
        self.clear_active_provider_turn(thread_id);
        self.tool_request_broker.clear_thread(thread_id);
        self.tool_registry.remove(thread_id);
        self.event_bus.unregister_thread_log(thread_id);
        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            thread.status = ThreadStatus::Ended;
            thread.updated_at = Utc::now();
        }
        self.event_bus
            .emit_status_changed(thread_id, ThreadStatus::Ended);
        true
    }

    /// Begins thread termination and returns the persistent-runtime end
    /// operation that must be dispatched after the Core lock is released.
    pub fn begin_end_thread(
        &mut self,
        input: EndThreadInput,
    ) -> Result<EndThreadStart, PedelecError> {
        self.invalidate_asset_uploads_for_thread(&input.thread_id);
        self.invalidate_asset_downloads_for_thread(&input.thread_id);
        let thread = self.thread_manager.thread(&input.thread_id)?.clone();
        {
            let thread = self.thread_manager.thread_mut(&input.thread_id)?;
            if thread.status != ThreadStatus::Ended {
                thread.status = ThreadStatus::Stopping;
                thread.updated_at = Utc::now();
                self.event_bus
                    .emit_status_changed(&input.thread_id, ThreadStatus::Stopping);
            }
        }

        let execution = PersistentRuntimeOperation::EndSession {
            session: PersistentProviderEndIntent {
                thread_id: input.thread_id.clone(),
                provider: thread.provider.clone(),
                provider_session_id: self
                    .thread_manager
                    .provider_state(&input.thread_id)
                    .and_then(|state| state.provider_session_id.clone()),
                active_provider_turn_id: self
                    .thread_manager
                    .provider_state(&input.thread_id)
                    .and_then(|state| state.active_provider_turn_id.clone()),
            },
        };
        Ok(EndThreadStart {
            thread_id: input.thread_id,
            execution,
        })
    }

    /// Finalizes termination after provider unsubscribe/interrupt/kill work
    /// has completed. It is safe to call this after an end dispatch failure so
    /// a thread cannot remain indefinitely in `Stopping`.
    pub fn finish_end_thread(&mut self, thread_id: &str) -> Result<(), PedelecError> {
        if self.thread_manager.thread(thread_id)?.status == ThreadStatus::Ended {
            self.debug_reactivating_threads.remove(thread_id);
            return Ok(());
        }
        self.debug_reactivating_threads.remove(thread_id);
        if let Some(operation) = self.pending_provider_operations.remove(thread_id) {
            let operation_id = operation.operation_id.clone();
            let operation_kind = match operation.kind() {
                PendingProviderOperationKind::UserTurn => ThreadOperationKind::User,
                PendingProviderOperationKind::Prepare => ThreadOperationKind::Prepare,
            };
            let error = PedelecError::new(
                error_codes::THREAD_ENDED,
                "thread ended while the operation was active",
            );
            self.tool_request_broker
                .clear_thread_with_error(thread_id, error.clone());
            self.event_bus.emit_operation_completed(
                thread_id,
                &operation_id,
                operation_kind,
                false,
                Some(error.clone()),
            );
            self.last_completed_operations.insert(
                thread_id.to_string(),
                CompletedOperationSnapshot {
                    operation_id,
                    operation_kind,
                    success: false,
                    error: Some(error),
                    completed_at: Utc::now(),
                },
            );
        }
        self.clear_active_provider_turn(thread_id);
        self.tool_request_broker.clear_thread(thread_id);
        self.tool_registry.remove(thread_id);

        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            thread.status = ThreadStatus::Ended;
            thread.updated_at = Utc::now();
        }
        self.event_bus
            .emit_status_changed(thread_id, ThreadStatus::Ended);
        self.event_bus.emit_ended(thread_id);
        self.event_bus.unregister_thread_log(thread_id);
        Ok(())
    }

    /// Reactivates an ended thread without contacting its provider runtime.
    ///
    /// Workspace and tool-registry restoration happens before the thread is
    /// mutated, so a failed resume leaves the authoritative thread state
    /// ended and does not expose partially restored runtime resources.
    pub fn resume_thread(
        &mut self,
        input: ResumeThreadInput,
    ) -> Result<ResumeThreadOutput, PedelecError> {
        self.ensure_deno_module_setup_ready(&input.thread_id)?;
        let status = self.thread_manager.thread(&input.thread_id)?.status.clone();
        match status {
            ThreadStatus::Idle => {
                // Even a same-handle resume must re-resolve the Core
                // Workspace capability and verify the Thread-private skills
                // snapshot before reporting success.
                let _ = self.load_thread_runtime(&input.thread_id, true)?;
                self.validate_deno_module_runtime_snapshot(&input.thread_id)?;
                return Ok(ResumeThreadOutput {
                    snapshot: self.build_thread_snapshot(&input.thread_id)?,
                });
            }
            ThreadStatus::Ended => {}
            ThreadStatus::Starting
            | ThreadStatus::Running
            | ThreadStatus::WaitingToolResult
            | ThreadStatus::Stopping => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is busy",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
            ThreadStatus::Error => {
                return Err(PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "thread is in error state",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
        }

        let (registry, event_log_path) = self.load_thread_runtime(&input.thread_id, true)?;
        // Validate the private module snapshot only after the existing
        // workspace/tool restoration checks, and before mutating the ended
        // lifecycle.  A missing or corrupt snapshot therefore leaves the
        // authoritative thread ended and is never silently replaced.
        self.validate_deno_module_runtime_snapshot(&input.thread_id)?;

        // An ended thread should already have these cleared. Keep the resume
        // boundary defensive so stale transient state cannot leak into the
        // newly idle lifecycle after an interrupted or older cleanup path.
        self.pending_provider_operations.remove(&input.thread_id);
        self.clear_active_provider_turn(&input.thread_id);
        self.tool_request_broker.clear_thread(&input.thread_id);
        self.tool_registry.insert(input.thread_id.clone(), registry);
        self.event_bus
            .register_thread_log(&input.thread_id, event_log_path);
        self.debug_reactivating_threads.remove(&input.thread_id);

        let thread = self.thread_manager.thread_mut(&input.thread_id)?;
        thread.status = ThreadStatus::Idle;
        thread.updated_at = Utc::now();
        self.event_bus
            .emit_status_changed(&input.thread_id, ThreadStatus::Idle);

        Ok(ResumeThreadOutput {
            snapshot: self.build_thread_snapshot(&input.thread_id)?,
        })
    }

    /// Compatibility wrapper for direct Core users. IPC/Tauri should use
    /// `begin_end_thread`, dispatch the returned persistent operation outside
    /// the mutex, and then call `finish_end_thread`.
    pub fn end_thread(&mut self, input: EndThreadInput) -> Result<(), PedelecError> {
        let start = self.begin_end_thread(input)?;
        self.finish_end_thread(&start.thread_id)
    }

    pub fn cleanup_stale_workspaces_for_app_start(&mut self) -> Vec<PedelecError> {
        let errors = self.workspace_manager.remove_all_managed_workspaces();
        self.workspace_registry.clear();
        self.workspace_deno_roots_initialized.clear();
        self.workspace_deno_module_scopes.clear();
        self.workspace_run_import_maps.clear();
        self.deno_module_upload_tickets
            .retain(|_, ticket| !matches!(&ticket.owner, DenoModuleUploadOwner::Workspace { .. }));
        errors
    }

    pub fn cleanup_for_app_exit(&mut self) -> Vec<PedelecError> {
        let thread_ids = self.thread_manager.thread_ids();
        for thread_id in thread_ids {
            let _ = self.end_thread(EndThreadInput { thread_id });
        }

        let errors = self.workspace_manager.remove_all_managed_workspaces();
        self.workspace_registry.clear();
        self.workspace_deno_roots_initialized.clear();
        self.workspace_deno_module_scopes.clear();
        self.workspace_run_import_maps.clear();
        self.deno_module_upload_tickets
            .retain(|_, ticket| !matches!(&ticket.owner, DenoModuleUploadOwner::Workspace { .. }));
        errors
    }

    pub fn thread_status(&self, thread_id: &str) -> Option<ThreadStatus> {
        self.thread_manager
            .thread(thread_id)
            .ok()
            .map(|thread| thread.status.clone())
    }

    pub fn provider_state(&self, thread_id: &str) -> Option<&ProviderSessionState> {
        self.thread_manager.provider_state(thread_id)
    }

    pub fn provider_session_state(&self, thread_id: &str) -> Option<&ProviderSessionState> {
        self.thread_manager.provider_session_state(thread_id)
    }

    pub fn event_log_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.event_bus.event_log_path(thread_id)
    }

    /// Validates and admits a Deno execution without changing public thread
    /// lifecycle state.  The returned intent is safe to pass to a Desktop
    /// runtime after this Core mutex has been released.
    pub fn prepare_deno_run_intent(
        &self,
        input: DenoRunInput,
    ) -> Result<DenoExecutionIntent, PedelecError> {
        self.ensure_deno_module_setup_ready(&input.thread_id)?;
        let thread = self.thread_manager.thread(&input.thread_id)?;
        match thread.status {
            ThreadStatus::Running | ThreadStatus::WaitingToolResult => {}
            ThreadStatus::Ended => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_ENDED,
                    "thread has ended",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
            _ => {
                return Err(PedelecError::with_details(
                    error_codes::DENO_THREAD_NOT_ACTIVE,
                    "Deno execution requires an active provider turn",
                    serde_json::json!({
                        "threadId": input.thread_id,
                        "status": thread.status,
                    }),
                ));
            }
        }

        let active_provider_turn_id = self
            .thread_manager
            .provider_state(&input.thread_id)
            .and_then(|state| state.active_provider_turn_id.as_deref())
            .filter(|turn_id| !turn_id.trim().is_empty());
        if active_provider_turn_id.is_none() {
            return Err(PedelecError::with_details(
                error_codes::DENO_THREAD_NOT_ACTIVE,
                "Deno execution requires an active provider turn",
                serde_json::json!({
                    "threadId": input.thread_id,
                    "reason": "provider turn is not active",
                }),
            ));
        }

        let workspace_path = self
            .thread_workspace_path(&input.thread_id)
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::WORKSPACE_NOT_FOUND,
                    "thread workspace was not found",
                    serde_json::json!({ "threadId": input.thread_id }),
                )
            })?;
        let (workspace_path, target) = match input.target {
            DenoRunTarget::WorkspaceFile { entrypoint } => {
                let (workspace_path, entrypoint) =
                    resolve_deno_entrypoint(&input.thread_id, &workspace_path, &entrypoint)?;
                (
                    workspace_path,
                    DenoExecutionTarget::WorkspaceFile { entrypoint },
                )
            }
            DenoRunTarget::StdinSource { source } => (
                resolve_deno_workspace(&input.thread_id, &workspace_path)?,
                DenoExecutionTarget::StdinSource { source },
            ),
        };
        let import_map_path = self.validate_deno_module_runtime_snapshot(&input.thread_id)?;

        Ok(DenoExecutionIntent {
            thread_id: input.thread_id.clone(),
            owner: DenoExecutionOwner::Thread {
                thread_id: input.thread_id,
            },
            workspace_path,
            target,
            args: input.args,
            timeout_ms: DEFAULT_WORKSPACE_RUN_TIMEOUT_MS,
            import_map_path,
        })
    }

    /// Short alias for callers that describe this boundary as admission
    /// rather than preparation.
    pub fn prepare_deno_run(
        &self,
        input: DenoRunInput,
    ) -> Result<DenoExecutionIntent, PedelecError> {
        self.prepare_deno_run_intent(input)
    }

    pub fn begin_tool_call(
        &mut self,
        input: ToolCallInput,
    ) -> Result<ToolInvocationRegistration, PedelecError> {
        let thread_status = self.thread_manager.thread(&input.thread_id)?.status.clone();
        match &thread_status {
            ThreadStatus::Running | ThreadStatus::WaitingToolResult => {}
            ThreadStatus::Ended => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_ENDED,
                    "thread has ended",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
            _ => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is not running",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
        }

        let registry = self.tool_registry.get(&input.thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOLS_MANIFEST_INVALID,
                "tool registry was not found for thread",
                serde_json::json!({ "threadId": input.thread_id }),
            )
        })?;
        let operation_id = match self.pending_provider_operations.get(&input.thread_id) {
            Some(operation) if operation.kind() == PendingProviderOperationKind::UserTurn => {
                operation.operation_id.clone()
            }
            Some(_) => {
                return Err(runtime_protocol_error(
                    &input.thread_id,
                    "tool call does not belong to an active user operation",
                ));
            }
            None => {
                return Err(runtime_protocol_error(
                    &input.thread_id,
                    "tool call has no active user operation",
                ));
            }
        };
        let normalized = registry.normalize_tool_call(&input.tool_name, &input.args)?;
        let has_pending = self
            .tool_request_broker
            .has_pending_for_thread(&input.thread_id);
        if (thread_status == ThreadStatus::WaitingToolResult && !has_pending)
            || (thread_status == ThreadStatus::Running && has_pending)
        {
            return Err(PedelecError::with_details(
                error_codes::PENDING_TOOL_REQUEST_EXISTS,
                "thread already has a pending tool request",
                serde_json::json!({ "threadId": input.thread_id }),
            ));
        }
        let registration = self.tool_request_broker.begin_or_join_for_operation(
            input.thread_id.clone(),
            input.tool_name.clone(),
            normalized.args.clone(),
            normalized.timeout_ms,
            operation_id.clone(),
        )?;

        if let ToolInvocationRegistration::Created(wait) = &registration {
            let thread = self.thread_manager.thread_mut(&input.thread_id)?;
            thread.status = ThreadStatus::WaitingToolResult;
            thread.updated_at = Utc::now();
            self.event_bus.emit_status_changed_for_operation(
                &input.thread_id,
                ThreadStatus::WaitingToolResult,
                Some(&operation_id),
            );
            self.event_bus.emit_tool_call(
                &input.thread_id,
                &operation_id,
                &wait.request_id,
                &input.tool_name,
                normalized.args,
            );
        }

        Ok(registration)
    }

    pub fn tool_spec(&self, input: ToolSpecInput) -> Result<ToolDefinition, PedelecError> {
        let registry = self.tool_registry.get(&input.thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOLS_MANIFEST_INVALID,
                "tool registry was not found for thread",
                serde_json::json!({ "threadId": input.thread_id }),
            )
        })?;
        registry.get(&input.tool_name).cloned().ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOL_NOT_FOUND,
                "tool was not found in registry",
                serde_json::json!({ "toolName": input.tool_name }),
            )
        })
    }

    pub fn timeout_tool_call(&mut self, request_id: &str) {
        let timeout = ToolInvocationOutcome::CoreError(PedelecError::new(
            error_codes::TOOL_TIMEOUT,
            "tool timeout",
        ));
        let Some(mut pending) = self
            .tool_request_broker
            .terminalize(request_id, timeout.clone())
        else {
            return;
        };
        pending.broadcast(timeout);
        if let Ok(thread) = self.thread_manager.thread_mut(&pending.request.thread_id) {
            if thread.status == ThreadStatus::WaitingToolResult {
                thread.status = ThreadStatus::Running;
                thread.updated_at = Utc::now();
                self.event_bus.emit_status_changed_for_operation(
                    &pending.request.thread_id,
                    ThreadStatus::Running,
                    Some(&pending.request.operation_id),
                );
            }
        }
    }

    pub fn submit_tool_result(&mut self, input: SubmitToolResultInput) -> Result<(), PedelecError> {
        let pending = self
            .tool_request_broker
            .get(&input.request_id)
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PENDING_TOOL_REQUEST_NOT_FOUND,
                    "pending tool request was not found",
                    serde_json::json!({
                        "threadId": input.thread_id,
                        "requestId": input.request_id
                    }),
                )
            })?;

        if pending.request.thread_id != input.thread_id {
            return Err(PedelecError::with_details(
                error_codes::PENDING_TOOL_REQUEST_NOT_FOUND,
                "pending tool request does not belong to thread",
                serde_json::json!({
                    "threadId": input.thread_id,
                    "requestId": input.request_id
                }),
            ));
        }

        let outcome = ToolInvocationOutcome::Result(input.result.clone());
        let mut pending = self
            .tool_request_broker
            .terminalize(&input.request_id, outcome.clone())
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PENDING_TOOL_REQUEST_NOT_FOUND,
                    "pending tool request was not found",
                    serde_json::json!({
                        "threadId": input.thread_id,
                        "requestId": input.request_id
                    }),
                )
            })?;

        pending.broadcast(outcome);
        if let Ok(thread) = self.thread_manager.thread_mut(&input.thread_id) {
            if thread.status == ThreadStatus::WaitingToolResult {
                thread.status = ThreadStatus::Running;
                thread.updated_at = Utc::now();
                self.event_bus.emit_status_changed_for_operation(
                    &input.thread_id,
                    ThreadStatus::Running,
                    Some(&pending.request.operation_id),
                );
            }
        }
        self.event_bus.emit_tool_result(
            &input.thread_id,
            &pending.request.operation_id,
            &input.request_id,
            &pending.request.tool_name,
            input.result,
        );

        Ok(())
    }

    pub fn subscribe_thread(
        &mut self,
        input: SubscribeThreadInput,
    ) -> Result<mpsc::Receiver<ThreadEvent>, PedelecError> {
        Ok(self.subscribe_thread_with_snapshot(input)?.events)
    }

    pub fn subscribe_thread_with_snapshot(
        &mut self,
        input: SubscribeThreadInput,
    ) -> Result<ThreadSubscription, PedelecError> {
        self.thread_manager.thread(&input.thread_id)?;
        let events = self.event_bus.subscribe(&input.thread_id);
        let snapshot = self.build_thread_snapshot(&input.thread_id)?;
        Ok(ThreadSubscription { events, snapshot })
    }

    /// Returns the current authoritative lifecycle snapshot without creating
    /// an event subscriber or contacting a provider runtime.
    pub fn thread_snapshot(
        &self,
        input: SubscribeThreadInput,
    ) -> Result<ThreadSnapshot, PedelecError> {
        self.build_thread_snapshot(&input.thread_id)
    }

    fn build_thread_snapshot(&self, thread_id: &str) -> Result<ThreadSnapshot, PedelecError> {
        let active_operation = self
            .pending_provider_operations
            .get(thread_id)
            .map(|operation| ActiveOperationSnapshot {
                operation_id: operation.operation_id.clone(),
                operation_kind: match operation.kind() {
                    PendingProviderOperationKind::UserTurn => ThreadOperationKind::User,
                    PendingProviderOperationKind::Prepare => ThreadOperationKind::Prepare,
                },
                started_at: operation.started_at,
            });
        Ok(ThreadSnapshot {
            thread_id: thread_id.to_string(),
            workspace_id: self.thread_manager.thread(thread_id)?.workspace_id.clone(),
            status: self.thread_manager.thread(thread_id)?.status.clone(),
            latest_seq: self.event_bus.latest_seq(thread_id),
            usage: self.session_usage.get(thread_id).cloned(),
            active_operation,
            last_completed_operation: self.last_completed_operations.get(thread_id).cloned(),
            pending_tool_request: self.tool_request_broker.pending_for_thread(thread_id),
        })
    }

    pub fn subscribe_all_threads(&mut self) -> mpsc::Receiver<ThreadEvent> {
        self.event_bus.subscribe_all()
    }

    /// Subscribes to desktop-only persistent-provider diagnostics. This is
    /// intentionally separate from the SDK ThreadEvent stream.
    pub fn subscribe_provider_runtime_diagnostics(
        &mut self,
    ) -> mpsc::Receiver<ProviderRuntimeDiagnostic> {
        self.provider_runtime_diagnostics.subscribe()
    }

    pub fn provider_runtime_diagnostic_history(&self) -> Vec<ProviderRuntimeDiagnostic> {
        self.provider_runtime_diagnostics.history()
    }

    pub fn record_provider_runtime_diagnostic(&mut self, diagnostic: ProviderRuntimeDiagnostic) {
        self.provider_runtime_diagnostics.emit(diagnostic);
    }

    /// Subscribes to desktop-only live raw provider protocol traffic. Unlike
    /// runtime diagnostics this bus intentionally keeps no in-memory history
    /// because durable protocol history already lives in per-session JSONL logs.
    pub fn subscribe_provider_protocol_traffic(
        &mut self,
    ) -> mpsc::Receiver<ProviderProtocolTraffic> {
        self.provider_protocol_traffic.subscribe()
    }

    pub fn record_provider_protocol_traffic(&mut self, traffic: ProviderProtocolTraffic) {
        self.provider_protocol_traffic.emit(traffic);
    }
}

/// Waits until the initial provider snapshot has been installed.
///
/// The readiness handle is copied while briefly holding the runtime mutex and
/// the actual wait happens afterwards. This is important because the scan must
/// reacquire the runtime mutex to atomically install its completed snapshot.
pub fn wait_for_provider_readiness(runtime: &SharedCoreRuntime) -> Result<(), PedelecError> {
    let readiness = runtime.lock().unwrap().provider_readiness.clone();
    readiness.wait()
}

/// Starts the initial provider scan without blocking desktop startup.
pub fn start_initial_provider_scan(runtime: SharedCoreRuntime) {
    let worker_runtime = Arc::clone(&runtime);
    if let Err(error) = std::thread::Builder::new()
        .name("pedelec-provider-initial-scan".to_string())
        .spawn(move || {
            let _ = refresh_shared_providers(&worker_runtime);
        })
    {
        let failure = PedelecError::with_details(
            error_codes::PROVIDER_SCAN_FAILED,
            "initial provider scan could not be started",
            serde_json::json!({ "error": error.to_string() }),
        );
        let mut runtime = runtime.lock().unwrap();
        runtime.provider_refresh_in_progress = false;
        runtime.provider_readiness.mark_failed(failure);
    }
}

/// Scans provider CLIs without holding the shared runtime lock. The completed
/// scan is installed atomically, so readers see either the prior complete scan
/// or the new complete scan, never partial results.
pub fn refresh_shared_providers(runtime: &SharedCoreRuntime) -> Vec<ProviderInfo> {
    let (wait_for_initial, path_override) = {
        let mut runtime_guard = runtime.lock().unwrap();
        if runtime_guard.provider_refresh_in_progress {
            if runtime_guard.provider_readiness.is_initial_scanning() {
                (true, None)
            } else {
                return runtime_guard.list_providers();
            }
        } else {
            runtime_guard.provider_refresh_in_progress = true;
            if runtime_guard.provider_readiness.is_uninitialized() {
                runtime_guard.provider_readiness.mark_initial_scanning();
            }
            (false, runtime_guard.provider_path_value_override.clone())
        }
    };

    if wait_for_initial {
        let _ = wait_for_provider_readiness(runtime);
        return runtime.lock().unwrap().list_providers();
    }

    // Resolve the login shell only after releasing the runtime mutex. A shell
    // profile is user-controlled and may take several seconds to finish.
    let scan_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path_value = path_override.unwrap_or_else(resolve_provider_path_value);
        let provider_scan = scan_external_providers(Some(path_value.clone()));
        (path_value, provider_scan)
    }));

    match scan_result {
        Ok((path_value, provider_scan)) => {
            let mut runtime_guard = runtime.lock().unwrap();
            runtime_guard.provider_resolved_path = Some(path_value);
            runtime_guard.provider_scan = provider_scan;
            runtime_guard.provider_refresh_in_progress = false;
            let providers = runtime_guard.list_providers();
            let readiness = runtime_guard.provider_readiness.clone();
            drop(runtime_guard);
            readiness.mark_ready();
            providers
        }
        Err(payload) => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_SCAN_FAILED,
                "provider scan failed",
                serde_json::json!({
                    "error": panic_payload_to_string(payload),
                }),
            );
            let mut runtime_guard = runtime.lock().unwrap();
            let is_initial_scan = runtime_guard.provider_readiness.is_initial_scanning();
            runtime_guard.provider_refresh_in_progress = false;
            if is_initial_scan {
                runtime_guard.provider_readiness.mark_failed(error);
            }
            runtime_guard.list_providers()
        }
    }
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "provider scan panicked with a non-string payload".to_string()
}

#[derive(Debug)]
pub struct CoreRuntimeOwner {
    runtime: SharedCoreRuntime,
}

impl CoreRuntimeOwner {
    pub fn new() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(CoreRuntime::new_for_application())),
        }
    }

    pub fn runtime(&self) -> SharedCoreRuntime {
        Arc::clone(&self.runtime)
    }
}

pub type SharedCoreRuntime = Arc<Mutex<CoreRuntime>>;

/// Bounded in-memory fan-out for diagnostics that belong to a persistent
/// provider runtime rather than to the semantic Pedelec ThreadEvent stream.
#[derive(Debug)]
pub struct ProviderRuntimeDiagnosticBus {
    history: VecDeque<ProviderRuntimeDiagnostic>,
    subscribers: Vec<mpsc::Sender<ProviderRuntimeDiagnostic>>,
    max_entries: usize,
}

impl Default for ProviderRuntimeDiagnosticBus {
    fn default() -> Self {
        Self {
            history: VecDeque::new(),
            subscribers: Vec::new(),
            max_entries: 512,
        }
    }
}

impl ProviderRuntimeDiagnosticBus {
    fn subscribe(&mut self) -> mpsc::Receiver<ProviderRuntimeDiagnostic> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.push(tx);
        rx
    }

    fn emit(&mut self, diagnostic: ProviderRuntimeDiagnostic) {
        self.history.push_back(diagnostic.clone());
        while self.history.len() > self.max_entries {
            self.history.pop_front();
        }
        self.subscribers
            .retain(|subscriber| subscriber.send(diagnostic.clone()).is_ok());
    }

    fn history(&self) -> Vec<ProviderRuntimeDiagnostic> {
        self.history.iter().cloned().collect()
    }
}

/// Live-only fan-out for complete raw provider protocol frames used by the
/// desktop Event Monitor. No history is retained here; session protocol JSONL
/// is the durable source of truth.
#[derive(Debug, Default)]
pub struct ProviderProtocolTrafficBus {
    subscribers: Vec<mpsc::Sender<ProviderProtocolTraffic>>,
}

impl ProviderProtocolTrafficBus {
    fn subscribe(&mut self) -> mpsc::Receiver<ProviderProtocolTraffic> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.push(tx);
        rx
    }

    fn emit(&mut self, traffic: ProviderProtocolTraffic) {
        self.subscribers
            .retain(|subscriber| subscriber.send(traffic.clone()).is_ok());
    }
}

#[derive(Debug, Default)]
pub struct ThreadManager {
    threads: HashMap<String, ThreadState>,
    provider_sessions: HashMap<String, ProviderSessionState>,
    workspace_threads: HashMap<String, HashSet<String>>,
    next_thread_number: u64,
}

impl ThreadManager {
    fn next_thread_id(&mut self) -> Result<String, PedelecError> {
        if self.next_thread_number >= THREAD_ID_MAX_COUNTER {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_CREATE_FAILED,
                "thread id counter was exhausted",
            ));
        }

        self.next_thread_number += 1;
        let encoded = to_base36(self.next_thread_number);
        if encoded.len() > THREAD_ID_BASE36_MAX_WIDTH {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_CREATE_FAILED,
                "thread id counter was exhausted",
            ));
        }

        Ok(format!(
            "t{:0>width$}",
            encoded,
            width = THREAD_ID_BASE36_MIN_WIDTH
        ))
    }

    fn contains_thread(&self, thread_id: &str) -> bool {
        self.threads.contains_key(thread_id)
    }

    fn thread_ids(&self) -> Vec<String> {
        self.threads.keys().cloned().collect()
    }

    pub fn insert_thread(&mut self, state: ThreadState, provider_session: ProviderSessionState) {
        let thread_id = state.thread_id.clone();
        let workspace_id = state.workspace_id.clone();
        if let Some(previous) = self.threads.insert(thread_id.clone(), state) {
            if previous.workspace_id != workspace_id {
                let remove_previous_index =
                    if let Some(threads) = self.workspace_threads.get_mut(&previous.workspace_id) {
                        threads.remove(&thread_id);
                        threads.is_empty()
                    } else {
                        false
                    };
                if remove_previous_index {
                    self.workspace_threads.remove(&previous.workspace_id);
                }
            }
        }
        self.workspace_threads
            .entry(workspace_id)
            .or_default()
            .insert(thread_id.clone());
        self.provider_sessions.insert(thread_id, provider_session);
    }

    pub fn remove_thread(
        &mut self,
        thread_id: &str,
    ) -> Option<(ThreadState, ProviderSessionState)> {
        let state = self.threads.remove(thread_id)?;
        let remove_workspace_index =
            if let Some(threads) = self.workspace_threads.get_mut(&state.workspace_id) {
                threads.remove(thread_id);
                threads.is_empty()
            } else {
                false
            };
        if remove_workspace_index {
            self.workspace_threads.remove(&state.workspace_id);
        }
        let provider_session =
            self.provider_sessions
                .remove(thread_id)
                .unwrap_or(ProviderSessionState {
                    provider_session_id: None,
                    active_provider_turn_id: None,
                });
        Some((state, provider_session))
    }

    pub fn thread(&self, thread_id: &str) -> Result<&ThreadState, PedelecError> {
        self.threads.get(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::THREAD_NOT_FOUND,
                "thread was not found",
                serde_json::json!({ "threadId": thread_id }),
            )
        })
    }

    pub fn thread_mut(&mut self, thread_id: &str) -> Result<&mut ThreadState, PedelecError> {
        self.threads.get_mut(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::THREAD_NOT_FOUND,
                "thread was not found",
                serde_json::json!({ "threadId": thread_id }),
            )
        })
    }

    pub fn provider_session_state(&self, thread_id: &str) -> Option<&ProviderSessionState> {
        self.provider_sessions.get(thread_id)
    }

    pub fn provider_session_state_mut(
        &mut self,
        thread_id: &str,
    ) -> Option<&mut ProviderSessionState> {
        self.provider_sessions.get_mut(thread_id)
    }

    /// Shorthand accessor for provider session state.
    pub fn provider_state(&self, thread_id: &str) -> Option<&ProviderSessionState> {
        self.provider_session_state(thread_id)
    }

    /// Shorthand accessor for provider session state.
    pub fn provider_state_mut(&mut self, thread_id: &str) -> Option<&mut ProviderSessionState> {
        self.provider_session_state_mut(thread_id)
    }

    pub fn threads_in_workspace(&self, workspace_id: &str) -> Vec<String> {
        let mut thread_ids = self
            .workspace_threads
            .get(workspace_id)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        thread_ids.sort();
        thread_ids
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceManager {
    workspace_root: Option<PathBuf>,
}

impl WorkspaceManager {
    pub fn with_workspace_root(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: Some(workspace_root.into()),
        }
    }

    pub fn managed_workspace_exists(&self, workspace_id: &str) -> Result<bool, PedelecError> {
        let safe_workspace_id = sanitize_workspace_resource_id(workspace_id)?;
        Ok(self.workspace_root()?.join(safe_workspace_id).exists())
    }

    pub fn managed_workspace_root_for_internal_use(&self) -> Result<PathBuf, PedelecError> {
        self.workspace_root()
    }

    pub fn is_managed_workspace(&self, workspace_path: &Path) -> bool {
        let Ok(root) = self.workspace_root() else {
            return false;
        };
        let root = resolve_path_for_overlap(&root).ok();
        let workspace = resolve_path_for_overlap(workspace_path).ok();
        match (root, workspace) {
            (Some(root), Some(workspace)) => workspace.starts_with(root),
            _ => false,
        }
    }

    pub fn create_managed_workspace(&self, workspace_id: &str) -> Result<PathBuf, PedelecError> {
        let safe_workspace_id = sanitize_workspace_resource_id(workspace_id)?;
        let workspace_root = self.workspace_root()?;
        let workspace_path = workspace_root.join(safe_workspace_id);

        if workspace_path.exists() {
            return Err(PedelecError::with_details(
                error_codes::WORKSPACE_CREATE_FAILED,
                "managed workspace already exists",
                serde_json::json!({ "workspacePath": path_for_external_use(&workspace_path) }),
            ));
        }

        let create_result = (|| {
            fs::create_dir_all(&workspace_path).map_err(|err| {
                workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "cannot create managed workspace",
                    &workspace_path,
                    err,
                )
            })?;

            self.create_runtime_subdirectories(&workspace_path)?;

            Ok(workspace_path.clone())
        })();

        if create_result.is_err() {
            let _ = fs::remove_dir_all(&workspace_path);
        }

        create_result
    }

    pub fn prepare_custom_workspace(
        &self,
        custom_path: impl AsRef<Path>,
    ) -> Result<PathBuf, PedelecError> {
        let custom_path = custom_path.as_ref();
        let managed_root = self.workspace_root()?;
        if !custom_path.is_absolute() {
            return Err(workspace_path_invalid_error(
                "custom workspace path must be absolute",
                custom_path,
                &managed_root,
            ));
        }

        let custom_comparison_path = resolve_path_for_overlap(custom_path)?;
        let managed_comparison_path = resolve_path_for_overlap(&managed_root)?;
        ensure_paths_do_not_overlap(
            custom_path,
            &custom_comparison_path,
            &managed_root,
            &managed_comparison_path,
        )?;

        if custom_path.exists() {
            let metadata = fs::metadata(custom_path).map_err(|err| {
                workspace_path_invalid_io_error(
                    "cannot inspect custom workspace path",
                    custom_path,
                    err,
                )
            })?;
            if !metadata.is_dir() {
                return Err(workspace_path_invalid_error(
                    "custom workspace path is not a directory",
                    custom_path,
                    &managed_root,
                ));
            }
        } else {
            fs::create_dir_all(custom_path).map_err(|err| {
                workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "cannot create custom workspace directory",
                    custom_path,
                    err,
                )
            })?;
        }

        let resolved_custom_path = custom_path.canonicalize().map_err(|err| {
            workspace_path_invalid_io_error(
                "cannot canonicalize custom workspace path",
                custom_path,
                err,
            )
        })?;
        let resolved_managed_root = resolve_path_for_overlap(&managed_root)?;
        ensure_paths_do_not_overlap(
            custom_path,
            &resolved_custom_path,
            &managed_root,
            &resolved_managed_root,
        )?;

        self.create_runtime_subdirectories(&resolved_custom_path)?;
        Ok(resolved_custom_path)
    }

    fn ensure_custom_workspace_config(
        &self,
        workspace_path: &Path,
        sdk_version: &str,
        origin: &str,
    ) -> Result<(), PedelecError> {
        #[derive(Serialize)]
        struct WorkspaceConfig<'a> {
            #[serde(rename = "sdk-version")]
            sdk_version: &'a str,
            origin: &'a str,
        }

        let config_path = workspace_metadata_path(workspace_path);
        let contents = serde_json::to_vec_pretty(&WorkspaceConfig {
            sdk_version,
            origin,
        })
        .expect("workspace config serialization should not fail");

        match fs::symlink_metadata(&config_path) {
            Ok(metadata) if metadata.is_file() => return Ok(()),
            Ok(_) => {
                return Err(workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "workspace config path is not a regular file",
                    &config_path,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "workspace config path is occupied",
                    ),
                ));
            }
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "cannot inspect workspace config path",
                    &config_path,
                    err,
                ));
            }
            Err(_) => {}
        }

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return match fs::symlink_metadata(&config_path) {
                    Ok(metadata) if metadata.is_file() => Ok(()),
                    Ok(_) => Err(workspace_io_error(
                        error_codes::WORKSPACE_CREATE_FAILED,
                        "workspace config path is not a regular file",
                        &config_path,
                        io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "workspace config path is occupied",
                        ),
                    )),
                    Err(metadata_err) => Err(workspace_io_error(
                        error_codes::WORKSPACE_CREATE_FAILED,
                        "cannot inspect workspace config path",
                        &config_path,
                        metadata_err,
                    )),
                };
            }
            Err(err) => {
                return Err(workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "cannot create workspace config",
                    &config_path,
                    err,
                ));
            }
        };

        if let Err(err) = file.write_all(&contents).and_then(|_| file.flush()) {
            drop(file);
            let _ = fs::remove_file(&config_path);
            return Err(workspace_io_error(
                error_codes::WORKSPACE_CREATE_FAILED,
                "cannot write workspace config",
                &config_path,
                err,
            ));
        }

        Ok(())
    }

    fn create_runtime_subdirectories(&self, workspace_path: &Path) -> Result<(), PedelecError> {
        let private_data_root = workspace_runtime_data_root(workspace_path);
        ensure_runtime_directory(
            &private_data_root,
            "cannot create Pedelec runtime data directory",
        )?;
        for path in [
            workspace_threads_root(workspace_path),
            workspace_assets_root(workspace_path),
            workspace_logs_root(workspace_path),
            workspace_tmp_root(workspace_path),
        ] {
            ensure_runtime_directory(&path, "cannot create workspace runtime subdirectory")?;
        }
        Ok(())
    }

    pub fn create_managed_workspace_with<T>(
        &self,
        workspace_id: &str,
        initialize: impl FnOnce(&Path) -> Result<T, PedelecError>,
    ) -> Result<(PathBuf, T), PedelecError> {
        let workspace_path = self.create_managed_workspace(workspace_id)?;

        match initialize(&workspace_path) {
            Ok(value) => Ok((workspace_path, value)),
            Err(err) => {
                let _ = self.remove_managed_workspace(&workspace_path);
                Err(err)
            }
        }
    }

    pub fn remove_managed_workspace(
        &self,
        workspace_path: impl AsRef<Path>,
    ) -> Result<(), PedelecError> {
        let workspace_path = workspace_path.as_ref();
        if !workspace_path.exists() {
            return Ok(());
        }

        self.ensure_path_inside_workspace_root(workspace_path)?;
        fs::remove_dir_all(workspace_path).map_err(|err| {
            workspace_io_error(
                error_codes::WORKSPACE_REMOVE_FAILED,
                "cannot remove managed workspace",
                workspace_path,
                err,
            )
        })
    }

    pub fn remove_managed_workspace_with_retry(
        &self,
        workspace_path: impl AsRef<Path>,
    ) -> Result<(), PedelecError> {
        let workspace_path = workspace_path.as_ref();
        let mut last_error = None;
        for attempt in 0..WORKSPACE_REMOVE_MAX_ATTEMPTS {
            match self.remove_managed_workspace(workspace_path) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_error = Some(err);
                    if attempt + 1 < WORKSPACE_REMOVE_MAX_ATTEMPTS {
                        std::thread::sleep(WORKSPACE_REMOVE_RETRY_DELAY);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_REMOVE_FAILED,
                "cannot remove managed workspace",
                serde_json::json!({ "path": path_for_external_use(workspace_path) }),
            )
        }))
    }

    pub fn remove_all_managed_workspaces(&self) -> Vec<PedelecError> {
        let workspace_root = match self.workspace_root() {
            Ok(root) => root,
            Err(err) => return vec![err],
        };
        if !workspace_root.exists() {
            return vec![];
        }

        let entries = match fs::read_dir(&workspace_root) {
            Ok(entries) => entries,
            Err(err) => {
                return vec![workspace_io_error(
                    error_codes::WORKSPACE_REMOVE_FAILED,
                    "cannot read workspace root",
                    &workspace_root,
                    err,
                )];
            }
        };

        let mut errors = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    errors.push(workspace_io_error(
                        error_codes::WORKSPACE_REMOVE_FAILED,
                        "cannot read workspace root entry",
                        &workspace_root,
                        err,
                    ));
                    continue;
                }
            };

            let path = entry.path();
            let is_dir = match entry.file_type() {
                Ok(file_type) => file_type.is_dir(),
                Err(err) => {
                    errors.push(workspace_io_error(
                        error_codes::WORKSPACE_REMOVE_FAILED,
                        "cannot inspect workspace root entry",
                        &path,
                        err,
                    ));
                    continue;
                }
            };
            if !is_dir {
                continue;
            }

            if let Err(err) = self.remove_managed_workspace_with_retry(&path) {
                errors.push(err);
            }
        }

        errors
    }

    fn workspace_root(&self) -> Result<PathBuf, PedelecError> {
        match &self.workspace_root {
            Some(root) => Ok(root.clone()),
            None => dirs::home_dir()
                .map(|home| home.join(".pedelec").join("workspaces"))
                .ok_or_else(|| {
                    PedelecError::new(
                        error_codes::WORKSPACE_PATH_INVALID,
                        "cannot resolve user home directory for workspace root",
                    )
                }),
        }
    }

    fn ensure_path_inside_workspace_root(&self, path: &Path) -> Result<(), PedelecError> {
        let workspace_root = self.workspace_root()?;
        let root = workspace_root.canonicalize().map_err(|err| {
            workspace_io_error(
                error_codes::WORKSPACE_PATH_INVALID,
                "cannot canonicalize workspace root",
                &workspace_root,
                err,
            )
        })?;
        let target = path.canonicalize().map_err(|err| {
            workspace_io_error(
                error_codes::WORKSPACE_PATH_INVALID,
                "cannot canonicalize thread workspace",
                path,
                err,
            )
        })?;

        if !target.starts_with(root) {
            return Err(PedelecError::with_details(
                error_codes::WORKSPACE_PATH_INVALID,
                "thread workspace is outside workspace root",
                serde_json::json!({ "workspacePath": path_for_external_use(path) }),
            ));
        }

        Ok(())
    }
}

fn validate_workspace_deno_module_names(names: &[String]) -> Result<(), PedelecError> {
    let mut seen = HashSet::new();
    for name in names {
        validate_deno_module_name(name)?;
        if !seen.insert(name) {
            return Err(PedelecError::with_details(
                error_codes::INVALID_INPUT,
                "duplicate Deno Module name",
                serde_json::json!({ "moduleName": name }),
            ));
        }
    }
    Ok(())
}

fn validate_deno_module_upload_size(size: u64, module_name: &str) -> Result<(), PedelecError> {
    if size == 0 {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
            "Deno Module artifact size must be positive",
        ));
    }
    if size > MAX_ASSET_UPLOAD_BYTES {
        return Err(PedelecError::with_details(
            error_codes::DENO_MODULE_ARTIFACT_TOO_LARGE,
            "Deno Module artifact exceeds the 100 MiB limit",
            serde_json::json!({
                "moduleName": module_name,
                "expectedSizeBytes": size,
                "maxSizeBytes": MAX_ASSET_UPLOAD_BYTES,
            }),
        ));
    }
    Ok(())
}

fn reset_workspace_deno_workspace_root(workspace_path: &Path) -> Result<(), PedelecError> {
    let (canonical_workspace_root, canonical_root) =
        ensure_workspace_deno_workspace_root(workspace_path).map_err(|err| {
            deno_module_materialization_error(
                "cannot establish Workspace Deno Module root",
                &workspace_deno_workspace_root(workspace_path),
                err,
            )
        })?;
    let metadata = fs::symlink_metadata(&canonical_root).map_err(|err| {
        deno_module_materialization_error(
            "cannot inspect Workspace Deno Module root",
            &canonical_root,
            err,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Workspace Deno Module root is not a regular directory",
        ));
    }
    let canonical_existing = canonical_root.canonicalize().map_err(|err| {
        deno_module_materialization_error(
            "cannot inspect Workspace Deno Module root",
            &canonical_root,
            err,
        )
    })?;
    if canonical_existing != canonical_root
        || !canonical_existing.starts_with(&canonical_workspace_root)
    {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Workspace Deno Module root escapes the Workspace",
        ));
    }
    // Only reset the Workspace-run tree. The sibling deno/threads tree is
    // deliberately never traversed or removed here.
    fs::remove_dir_all(&canonical_root).map_err(|err| {
        deno_module_materialization_error(
            "cannot reset Workspace Deno Module root",
            &canonical_root,
            err,
        )
    })?;
    fs::create_dir(&canonical_root).map_err(|err| {
        deno_module_materialization_error(
            "cannot recreate Workspace Deno Module root",
            &canonical_root,
            err,
        )
    })?;
    let recreated = canonical_root.canonicalize().map_err(|err| {
        deno_module_materialization_error(
            "cannot inspect recreated Workspace Deno Module root",
            &canonical_root,
            err,
        )
    })?;
    if recreated != canonical_root || !recreated.starts_with(&canonical_workspace_root) {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "recreated Workspace Deno Module root is unsafe",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct WorkspaceDenoScopeRoots {
    canonical_workspace_root: PathBuf,
    canonical_scope_root: PathBuf,
    canonical_modules_root: PathBuf,
    canonical_runs_root: PathBuf,
}

fn ensure_workspace_deno_workspace_root(workspace_path: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let metadata = fs::symlink_metadata(workspace_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module workspace is not a regular directory",
        ));
    }
    let canonical_workspace_root = workspace_path.canonicalize()?;
    let mut current = canonical_workspace_root.clone();
    for component in [PEDELEC_RUNTIME_DATA_DIR, "deno", "workspace"] {
        current = ensure_workspace_deno_child_directory(
            &current,
            component,
            &[&canonical_workspace_root],
        )?;
    }
    Ok((canonical_workspace_root, current))
}

fn ensure_workspace_deno_scope_roots(
    workspace_path: &Path,
    scope_id: &str,
) -> Result<WorkspaceDenoScopeRoots, PedelecError> {
    validate_deno_module_scope_id(scope_id).map_err(|err| {
        deno_module_materialization_error(
            "Workspace Deno Module scope is invalid",
            &workspace_deno_workspace_root(workspace_path),
            err,
        )
    })?;
    let (canonical_workspace_root, canonical_workspace_run_root) =
        ensure_workspace_deno_workspace_root(workspace_path).map_err(|err| {
            deno_module_materialization_error(
                "cannot establish Workspace Deno Module root",
                &workspace_deno_workspace_root(workspace_path),
                err,
            )
        })?;
    let canonical_scope_root = ensure_workspace_deno_child_directory(
        &canonical_workspace_run_root,
        scope_id,
        &[&canonical_workspace_root, &canonical_workspace_run_root],
    )
    .map_err(|err| {
        deno_module_materialization_error(
            "cannot establish Workspace Deno Module scope",
            &workspace_deno_workspace_scope_root(workspace_path, scope_id),
            err,
        )
    })?;
    let canonical_modules_root = ensure_workspace_deno_child_directory(
        &canonical_scope_root,
        "modules",
        &[&canonical_workspace_root, &canonical_scope_root],
    )
    .map_err(|err| {
        deno_module_materialization_error(
            "cannot establish Workspace Deno Module package root",
            &workspace_deno_workspace_modules_root(workspace_path, scope_id),
            err,
        )
    })?;
    let canonical_runs_root = ensure_workspace_deno_child_directory(
        &canonical_scope_root,
        "runs",
        &[&canonical_workspace_root, &canonical_scope_root],
    )
    .map_err(|err| {
        deno_module_materialization_error(
            "cannot establish Workspace Deno Module run root",
            &workspace_deno_workspace_scope_root(workspace_path, scope_id).join("runs"),
            err,
        )
    })?;
    Ok(WorkspaceDenoScopeRoots {
        canonical_workspace_root,
        canonical_scope_root,
        canonical_modules_root,
        canonical_runs_root,
    })
}

fn ensure_workspace_deno_child_directory(
    canonical_parent: &Path,
    component: &str,
    containment_roots: &[&Path],
) -> io::Result<PathBuf> {
    let parent_metadata = fs::symlink_metadata(canonical_parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module parent is not a regular directory",
        ));
    }
    if canonical_parent.canonicalize()? != canonical_parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module parent is not canonical",
        ));
    }
    let mut components = Path::new(component).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module path component is invalid",
        ));
    }
    let child = canonical_parent.join(component);
    match fs::symlink_metadata(&child) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Workspace Deno Module path is not a regular directory",
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir(&child)?,
        Err(err) => return Err(err),
    }
    let canonical_child = child.canonicalize()?;
    if containment_roots
        .iter()
        .any(|root| !canonical_child.starts_with(root))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module path escapes its private root",
        ));
    }
    Ok(canonical_child)
}

fn validate_deno_module_scope_id(scope_id: &str) -> io::Result<()> {
    if scope_id.is_empty()
        || scope_id == "."
        || scope_id == ".."
        || scope_id.contains('/')
        || scope_id.contains('\\')
        || scope_id.contains('\0')
        || scope_id.contains(':')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Workspace Deno Module scope path is invalid",
        ));
    }
    Ok(())
}

fn remove_workspace_deno_module_package(
    roots: &WorkspaceDenoScopeRoots,
    module_name: &str,
) -> Result<(), PedelecError> {
    validate_deno_module_name(module_name)?;
    validate_canonical_workspace_deno_scope_roots(roots).map_err(|err| {
        deno_module_materialization_error(
            "Workspace Deno Module package root is unsafe",
            &roots.canonical_modules_root,
            err,
        )
    })?;

    let mut package_path = roots.canonical_modules_root.clone();
    for part in module_name.split('/') {
        package_path.push(part);
        let metadata = match fs::symlink_metadata(&package_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(deno_module_materialization_error(
                    "Workspace Deno Module package cleanup failed",
                    &package_path,
                    err,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Workspace Deno Module package path is unsafe",
            ));
        }
        let canonical_component = package_path.canonicalize().map_err(|err| {
            deno_module_materialization_error(
                "Workspace Deno Module package cleanup failed",
                &package_path,
                err,
            )
        })?;
        if !canonical_component.starts_with(&roots.canonical_modules_root)
            || !canonical_component.starts_with(&roots.canonical_scope_root)
            || !canonical_component.starts_with(&roots.canonical_workspace_root)
        {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Workspace Deno Module package cleanup path is unsafe",
            ));
        }
    }
    let canonical_package = package_path.canonicalize().map_err(|err| {
        deno_module_materialization_error(
            "Workspace Deno Module package cleanup failed",
            &package_path,
            err,
        )
    })?;
    fs::remove_dir_all(&canonical_package).map_err(|err| {
        deno_module_materialization_error(
            "Workspace Deno Module package cleanup failed",
            &canonical_package,
            err,
        )
    })
}

fn validate_canonical_workspace_deno_scope_roots(
    roots: &WorkspaceDenoScopeRoots,
) -> io::Result<()> {
    for (path, parent) in [
        (
            roots.canonical_scope_root.as_path(),
            roots.canonical_workspace_root.as_path(),
        ),
        (
            roots.canonical_modules_root.as_path(),
            roots.canonical_scope_root.as_path(),
        ),
        (
            roots.canonical_runs_root.as_path(),
            roots.canonical_scope_root.as_path(),
        ),
    ] {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Workspace Deno Module root is not a regular directory",
            ));
        }
        if path.canonicalize()? != path
            || !path.starts_with(parent)
            || !path.starts_with(&roots.canonical_workspace_root)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Workspace Deno Module root escapes its private root",
            ));
        }
    }
    Ok(())
}

fn materialize_workspace_deno_run_import_map(
    workspace_path: &Path,
    scope_id: &str,
    run_id: &str,
    module_names: &[String],
) -> Result<PathBuf, PedelecError> {
    validate_deno_module_scope_id(run_id).map_err(|err| {
        deno_module_materialization_error(
            "Workspace Deno Module run is invalid",
            &workspace_deno_workspace_root(workspace_path),
            err,
        )
    })?;
    let roots = ensure_workspace_deno_scope_roots(workspace_path, scope_id)?;
    let modules_root = roots.canonical_modules_root.clone();
    let mut imports = BTreeMap::new();
    for name in module_names {
        validate_deno_module_name(name)?;
        validate_deno_module_package(&modules_root, scope_id, name)?;
        imports.insert(name.clone(), format!("../../modules/{name}/index.mjs"));
    }
    let canonical_run_root = ensure_workspace_deno_child_directory(
        &roots.canonical_runs_root,
        run_id,
        &[
            &roots.canonical_workspace_root,
            &roots.canonical_scope_root,
            &roots.canonical_runs_root,
        ],
    )
    .map_err(|err| {
        deno_module_materialization_error(
            "cannot create Workspace Deno Module run state",
            &roots.canonical_runs_root,
            err,
        )
    })?;
    let import_map_path = canonical_run_root.join("import-map.json");
    let run_root = canonical_run_root.as_path();
    let bytes = serde_json::to_vec_pretty(&DenoModuleImportMap { imports }).map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "cannot serialize Workspace Deno Module import map",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let temporary_path = run_root.join(".pedelec-import-map.tmp");
    if fs::symlink_metadata(&temporary_path).is_ok()
        || fs::symlink_metadata(&import_map_path).is_ok()
    {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Workspace Deno Module run state is already occupied",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|err| {
            deno_module_materialization_error(
                "cannot write Workspace Deno Module import map",
                &temporary_path,
                err,
            )
        })?;
    let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    write_result.map_err(|err| {
        deno_module_materialization_error(
            "cannot write Workspace Deno Module import map",
            &temporary_path,
            err,
        )
    })?;
    fs::rename(&temporary_path, &import_map_path).map_err(|err| {
        deno_module_materialization_error(
            "cannot commit Workspace Deno Module import map",
            &import_map_path,
            err,
        )
    })?;
    Ok(import_map_path)
}

pub fn is_valid_deno_module_name(name: &str) -> bool {
    if name.is_empty()
        || name.trim() != name
        || name == "."
        || name == ".."
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':')
        || name.starts_with('/')
        || name.ends_with('/')
    {
        return false;
    }

    let parts = name.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [segment] => is_valid_deno_module_segment(segment),
        [scope, package] if scope.starts_with('@') && scope.len() > 1 => {
            is_valid_deno_module_segment(&scope[1..]) && is_valid_deno_module_segment(package)
        }
        _ => false,
    }
}

pub fn validate_deno_module_name(name: &str) -> Result<(), PedelecError> {
    if is_valid_deno_module_name(name) {
        Ok(())
    } else {
        Err(PedelecError::with_details(
            error_codes::DENO_MODULE_NAME_INVALID,
            "Deno Module name is invalid",
            serde_json::json!({ "moduleName": name }),
        ))
    }
}

fn is_valid_deno_module_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn normalize_deno_module_inputs(
    skills: Option<&CreateThreadSkillsInput>,
) -> Result<Vec<DenoModuleState>, PedelecError> {
    let Some(skills) = skills else {
        return Ok(Vec::new());
    };
    let mut seen = HashSet::new();
    let mut modules = Vec::with_capacity(skills.deno_modules.len());
    for module in &skills.deno_modules {
        validate_deno_module_name(&module.name)?;
        if !seen.insert(module.name.clone()) {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_NAME_INVALID,
                "duplicate Deno Module name",
                serde_json::json!({ "moduleName": module.name }),
            ));
        }
        if module.description.trim().is_empty() {
            return Err(PedelecError::with_details(
                error_codes::INVALID_INPUT,
                "Deno Module description must be a non-empty string",
                serde_json::json!({ "moduleName": module.name }),
            ));
        }
        if module.usage.trim().is_empty() {
            return Err(PedelecError::with_details(
                error_codes::INVALID_INPUT,
                "Deno Module usage must be a non-empty string",
                serde_json::json!({ "moduleName": module.name }),
            ));
        }
        modules.push(DenoModuleState {
            name: module.name.clone(),
            description: module.description.clone(),
            usage: module.usage.clone(),
            prefer_stdin_execution: module.prefer_stdin_execution,
            state: DenoModuleSetupState::Pending,
        });
    }
    Ok(modules)
}

fn validate_deno_module_artifact_envelope(
    envelope: &DenoModuleArtifactEnvelope,
) -> Result<(), PedelecError> {
    if envelope.version != 1 || envelope.format != "esm" {
        return Err(PedelecError::with_details(
            error_codes::DENO_MODULE_ARTIFACT_INVALID,
            "Deno Module artifact envelope has an unsupported version or format",
            serde_json::json!({ "version": envelope.version, "format": envelope.format }),
        ));
    }
    if envelope.runtime_source.trim().is_empty() || envelope.types_source.trim().is_empty() {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_ARTIFACT_INVALID,
            "Deno Module artifact must contain runtime and declaration sources",
        ));
    }
    Ok(())
}

fn materialize_deno_module_package(
    workspace_path: &Path,
    thread_id: &str,
    module_name: &str,
    envelope: &DenoModuleArtifactEnvelope,
    upload_id: &str,
) -> Result<(), PedelecError> {
    validate_deno_module_name(module_name)?;
    let (_modules_root, canonical_root) =
        ensure_deno_module_storage_root(workspace_path, thread_id).map_err(|err| {
            deno_module_materialization_error(
                "cannot create Deno Module package root",
                &workspace_deno_modules_root(workspace_path, thread_id),
                err,
            )
        })?;

    materialize_deno_module_package_at(&canonical_root, module_name, envelope, upload_id)
}

fn materialize_deno_module_package_at(
    canonical_root: &Path,
    module_name: &str,
    envelope: &DenoModuleArtifactEnvelope,
    upload_id: &str,
) -> Result<(), PedelecError> {
    validate_deno_module_name(module_name)?;
    let package_path = module_name
        .split('/')
        .fold(canonical_root.to_path_buf(), |path, part| path.join(part));
    let package_parent = package_path.parent().ok_or_else(|| {
        PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module package path is invalid",
        )
    })?;
    ensure_deno_module_package_parent(package_parent, &canonical_root).map_err(|err| {
        deno_module_materialization_error("Deno Module package path is unsafe", package_parent, err)
    })?;
    if fs::symlink_metadata(&package_path).is_ok() {
        return Err(PedelecError::with_details(
            error_codes::DENO_MODULE_ALREADY_READY,
            "Deno Module package already exists",
            serde_json::json!({ "moduleName": module_name }),
        ));
    }

    let temporary_package = package_parent.join(format!(".pedelec-{upload_id}-module"));
    if let Ok(metadata) = fs::symlink_metadata(&temporary_package) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            let _ = fs::remove_file(&temporary_package);
        } else {
            let _ = fs::remove_dir_all(&temporary_package);
        }
    }
    fs::create_dir(&temporary_package).map_err(|err| {
        deno_module_materialization_error(
            "cannot create temporary Deno Module package",
            &temporary_package,
            err,
        )
    })?;

    let result = (|| -> Result<(), PedelecError> {
        let package_json = serde_json::json!({
            "name": module_name,
            "type": "module",
            "types": "./index.d.ts",
            "exports": {
                ".": {
                    "types": "./index.d.ts",
                    "import": "./index.mjs"
                }
            }
        });
        write_deno_module_file(
            &temporary_package.join("package.json"),
            serde_json::to_vec_pretty(&package_json)
                .expect("Deno Module package metadata serialization should not fail"),
        )?;
        write_deno_module_file(
            &temporary_package.join("index.mjs"),
            envelope.runtime_source.as_bytes().to_vec(),
        )?;
        write_deno_module_file(
            &temporary_package.join("index.d.ts"),
            envelope.types_source.as_bytes().to_vec(),
        )?;
        fs::rename(&temporary_package, &package_path).map_err(|err| {
            deno_module_materialization_error(
                "cannot commit Deno Module package atomically",
                &package_path,
                err,
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary_package);
    }
    result
}

/// Builds and atomically commits the one import map for a complete thread
/// snapshot.  The package files are checked before the map becomes visible so
/// Deno can never observe an import target that is only partially installed.
fn materialize_deno_module_import_map(
    workspace_path: &Path,
    thread_id: &str,
    modules: &[DenoModuleState],
    upload_id: &str,
) -> Result<(), PedelecError> {
    let (canonical_thread_root, canonical_modules_root) =
        canonical_existing_deno_module_storage_root(workspace_path, thread_id).map_err(|err| {
            deno_module_materialization_error(
                "cannot inspect Deno Module storage before import-map commit",
                &workspace_deno_thread_root(workspace_path, thread_id),
                err,
            )
        })?;
    let imports = deno_module_import_map_entries(modules)?;
    for module in modules {
        validate_deno_module_package(&canonical_modules_root, thread_id, &module.name)?;
    }

    let import_map = DenoModuleImportMap { imports };
    let bytes = serde_json::to_vec_pretty(&import_map)
        .expect("Deno Module import map serialization should not fail");
    let import_map_path = canonical_thread_root.join("import-map.json");

    // A repeated completion attempt must not replace an immutable snapshot.
    // Accept the exact same committed map, but reject an occupied path with a
    // different value or an unsafe filesystem entry.
    match fs::symlink_metadata(&import_map_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Deno Module import map is not a regular file",
            ));
        }
        Ok(_) => {
            let existing = fs::read(&import_map_path).map_err(|err| {
                deno_module_materialization_error(
                    "cannot read the existing Deno Module import map",
                    &import_map_path,
                    err,
                )
            })?;
            if existing == bytes {
                return Ok(());
            }
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Deno Module import map is already committed with different contents",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(deno_module_materialization_error(
                "cannot inspect Deno Module import map",
                &import_map_path,
                err,
            ));
        }
    }

    let temporary_path = canonical_thread_root.join(format!(".pedelec-{upload_id}-import-map"));
    if fs::symlink_metadata(&temporary_path).is_ok() {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module import-map staging path is already occupied",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|err| {
            deno_module_materialization_error(
                "cannot create Deno Module import-map staging file",
                &temporary_path,
                err,
            )
        })?;
    let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    let write_result = write_result.and_then(|_| fs::rename(&temporary_path, &import_map_path));
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(deno_module_materialization_error(
            "cannot commit Deno Module import map atomically",
            &import_map_path,
            err,
        ));
    }
    Ok(())
}

fn deno_module_import_map_entries(
    modules: &[DenoModuleState],
) -> Result<BTreeMap<String, String>, PedelecError> {
    let mut imports = BTreeMap::new();
    for module in modules {
        validate_deno_module_name(&module.name)?;
        if module.state != DenoModuleSetupState::Ready {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_SETUP_INCOMPLETE,
                "Deno Module import map requires every module to be ready",
                serde_json::json!({ "moduleName": module.name }),
            ));
        }
        let target = format!("./modules/{}/index.mjs", module.name);
        if imports.insert(module.name.clone(), target).is_some() {
            return Err(PedelecError::with_details(
                error_codes::DENO_MODULE_NAME_INVALID,
                "duplicate Deno Module name",
                serde_json::json!({ "moduleName": module.name }),
            ));
        }
    }
    Ok(imports)
}

/// Validates the exact package and import-map snapshot expected by Core.  No
/// directory or file is created here; this is used by execution and resume
/// admission to detect private-runtime loss or tampering.
fn validate_deno_module_runtime_snapshot_files(
    workspace_path: &Path,
    thread_id: &str,
    modules: &[DenoModuleState],
) -> Result<Option<PathBuf>, PedelecError> {
    if modules.is_empty() {
        return Ok(None);
    }
    if modules
        .iter()
        .any(|module| module.state != DenoModuleSetupState::Ready)
    {
        let pending = modules
            .iter()
            .filter(|module| module.state != DenoModuleSetupState::Ready)
            .map(|module| module.name.clone())
            .collect::<Vec<_>>();
        return Err(PedelecError::with_details(
            error_codes::DENO_MODULE_SETUP_INCOMPLETE,
            "Deno Module setup is incomplete",
            serde_json::json!({ "threadId": thread_id, "modules": pending }),
        ));
    }

    let (canonical_thread_root, canonical_modules_root) =
        canonical_existing_deno_module_storage_root(workspace_path, thread_id).map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                None,
                "Deno Module private runtime storage is unavailable",
                err,
            )
        })?;
    let expected_imports = deno_module_import_map_entries(modules)?;
    for module in modules {
        validate_deno_module_package(&canonical_modules_root, thread_id, &module.name)?;
    }

    let import_map_path = canonical_thread_root.join("import-map.json");
    let metadata = fs::symlink_metadata(&import_map_path).map_err(|err| {
        deno_module_runtime_error(thread_id, None, "Deno Module import map is missing", err)
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(deno_module_runtime_error(
            thread_id,
            None,
            "Deno Module import map is not a regular file",
            io::Error::new(io::ErrorKind::InvalidInput, "unsafe import map entry"),
        ));
    }
    let canonical_import_map = import_map_path.canonicalize().map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            None,
            "Deno Module import map could not be canonicalized",
            err,
        )
    })?;
    let canonical_workspace = workspace_path.canonicalize().map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            None,
            "Deno Module workspace could not be canonicalized",
            err,
        )
    })?;
    if !canonical_import_map.starts_with(&canonical_workspace)
        || !canonical_import_map.starts_with(&canonical_thread_root)
    {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module import map resolves outside its private runtime root",
        ));
    }

    let map_bytes = fs::read(&import_map_path).map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            None,
            "Deno Module import map could not be read",
            err,
        )
    })?;
    let actual: DenoModuleImportMap = serde_json::from_slice(&map_bytes).map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            None,
            "Deno Module import map is invalid JSON",
            io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
        )
    })?;
    let expected_bytes = serde_json::to_vec_pretty(&DenoModuleImportMap {
        imports: expected_imports.clone(),
    })
    .expect("Deno Module import map serialization should not fail");
    if map_bytes != expected_bytes || actual.imports != expected_imports {
        return Err(PedelecError::with_details(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module import map does not match the declared module snapshot",
            serde_json::json!({ "threadId": thread_id }),
        ));
    }

    Ok(Some(PathBuf::from(path_for_external_use(
        &canonical_import_map,
    ))))
}

fn validate_deno_module_package(
    canonical_modules_root: &Path,
    thread_id: &str,
    module_name: &str,
) -> Result<(), PedelecError> {
    validate_deno_module_name(module_name)?;
    let mut package_path = canonical_modules_root.to_path_buf();
    for part in module_name.split('/') {
        package_path.push(part);
        let package_component = fs::symlink_metadata(&package_path).map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package directory is missing",
                err,
            )
        })?;
        if package_component.file_type().is_symlink() || !package_component.is_dir() {
            return Err(deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package directory is unsafe",
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "package path contains a non-directory component",
                ),
            ));
        }
        let canonical_component = package_path.canonicalize().map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package directory could not be canonicalized",
                err,
            )
        })?;
        if !canonical_component.starts_with(canonical_modules_root) {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Deno Module package path resolves outside its private module root",
            ));
        }
    }
    let canonical_package = package_path.canonicalize().map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            Some(module_name),
            "Deno Module package directory could not be canonicalized",
            err,
        )
    })?;
    if !canonical_package.starts_with(canonical_modules_root) {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module package resolves outside its private module root",
        ));
    }

    for filename in ["package.json", "index.mjs", "index.d.ts"] {
        let path = package_path.join(filename);
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package file is missing",
                err,
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package file is not a regular file",
                io::Error::new(io::ErrorKind::InvalidInput, "unsafe package file"),
            ));
        }
        let canonical_file = path.canonicalize().map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package file could not be canonicalized",
                err,
            )
        })?;
        if !canonical_file.starts_with(&canonical_package)
            || !canonical_file.starts_with(canonical_modules_root)
        {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Deno Module package file resolves outside its private module root",
            ));
        }
    }

    let package_json = serde_json::from_slice::<Value>(
        &fs::read(package_path.join("package.json")).map_err(|err| {
            deno_module_runtime_error(
                thread_id,
                Some(module_name),
                "Deno Module package manifest could not be read",
                err,
            )
        })?,
    )
    .map_err(|err| {
        deno_module_runtime_error(
            thread_id,
            Some(module_name),
            "Deno Module package manifest is invalid JSON",
            io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
        )
    })?;
    let manifest_is_expected = package_json.get("name") == Some(&Value::String(module_name.into()))
        && package_json.get("type") == Some(&Value::String("module".into()))
        && package_json.get("types") == Some(&Value::String("./index.d.ts".into()))
        && package_json
            .get("exports")
            .and_then(Value::as_object)
            .and_then(|exports| exports.get("."))
            .and_then(Value::as_object)
            .and_then(|root| root.get("import"))
            == Some(&Value::String("./index.mjs".into()))
        && package_json
            .get("exports")
            .and_then(Value::as_object)
            .and_then(|exports| exports.get("."))
            .and_then(Value::as_object)
            .and_then(|root| root.get("types"))
            == Some(&Value::String("./index.d.ts".into()));
    if !manifest_is_expected {
        return Err(PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module package manifest does not expose the expected runtime and types files",
        ));
    }
    Ok(())
}

fn deno_module_runtime_error(
    thread_id: &str,
    module_name: Option<&str>,
    message: &'static str,
    error: io::Error,
) -> PedelecError {
    let mut details = serde_json::Map::new();
    details.insert("threadId".into(), serde_json::json!(thread_id));
    if let Some(module_name) = module_name {
        details.insert("moduleName".into(), serde_json::json!(module_name));
    }
    details.insert("error".into(), serde_json::json!(error.to_string()));
    PedelecError::with_details(
        error_codes::DENO_MODULE_SETUP_FAILED,
        message,
        Value::Object(details),
    )
}

fn remove_materialized_deno_module_package(
    workspace_path: &Path,
    thread_id: &str,
    module_name: &str,
) -> Result<(), PedelecError> {
    validate_deno_module_name(module_name)?;
    let Ok((_thread_root, canonical_modules_root)) =
        canonical_existing_deno_module_storage_root(workspace_path, thread_id)
    else {
        return Ok(());
    };
    let mut package_path = canonical_modules_root.clone();
    for part in module_name.split('/') {
        package_path.push(part);
        let metadata = match fs::symlink_metadata(&package_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                return Err(PedelecError::new(
                    error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                    "Deno Module package cleanup failed",
                ));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || !package_path
                .canonicalize()
                .map(|path| path.starts_with(&canonical_modules_root))
                .unwrap_or(false)
        {
            return Err(PedelecError::new(
                error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
                "Deno Module package cleanup path is unsafe",
            ));
        }
    }
    fs::remove_dir_all(package_path).map_err(|_| {
        PedelecError::new(
            error_codes::DENO_MODULE_MATERIALIZATION_FAILED,
            "Deno Module package cleanup failed",
        )
    })
}

/// Resolves the already-materialized Deno storage without creating any
/// missing component. Every private directory is checked with
/// `symlink_metadata` before canonicalization so a resume/execution check
/// cannot accidentally follow an attacker-controlled replacement.
fn canonical_existing_deno_module_storage_root(
    workspace_path: &Path,
    thread_id: &str,
) -> io::Result<(PathBuf, PathBuf)> {
    validate_deno_module_thread_id(thread_id)?;
    let workspace_metadata = fs::symlink_metadata(workspace_path)?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module workspace is not a regular directory",
        ));
    }
    let canonical_workspace = workspace_path.canonicalize()?;
    let mut current = workspace_path.to_path_buf();
    for component in [
        PEDELEC_RUNTIME_DATA_DIR,
        "deno",
        "threads",
        thread_id,
        "modules",
    ] {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module storage path is not a regular directory",
            ));
        }
        if !current.canonicalize()?.starts_with(&canonical_workspace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module storage path escapes the workspace",
            ));
        }
    }
    let canonical_modules_root = current.canonicalize()?;
    let canonical_thread_root = canonical_modules_root
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing Deno thread root"))?
        .to_path_buf();
    Ok((canonical_thread_root, canonical_modules_root))
}

fn validate_deno_module_thread_id(thread_id: &str) -> io::Result<()> {
    if thread_id.is_empty()
        || thread_id == "."
        || thread_id == ".."
        || thread_id.contains('/')
        || thread_id.contains('\\')
        || thread_id.contains('\0')
        || thread_id.contains(':')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module thread path is invalid",
        ));
    }
    Ok(())
}

fn ensure_deno_module_storage_root(
    workspace_path: &Path,
    thread_id: &str,
) -> io::Result<(PathBuf, PathBuf)> {
    if thread_id.is_empty()
        || thread_id == "."
        || thread_id == ".."
        || thread_id.contains('/')
        || thread_id.contains('\\')
        || thread_id.contains('\0')
        || thread_id.contains(':')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module thread path is invalid",
        ));
    }

    let workspace_metadata = fs::symlink_metadata(workspace_path)?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module workspace is not a regular directory",
        ));
    }
    let canonical_workspace = workspace_path.canonicalize()?;
    let mut current = workspace_path.to_path_buf();
    for component in [
        PEDELEC_RUNTIME_DATA_DIR,
        "deno",
        "threads",
        thread_id,
        "modules",
    ] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Deno Module storage path is not a regular directory",
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(err) => return Err(err),
        }
        if !current.canonicalize()?.starts_with(&canonical_workspace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module storage path escapes the workspace",
            ));
        }
    }

    let canonical_root = current.canonicalize()?;
    Ok((current, canonical_root))
}

fn ensure_deno_module_package_parent(
    package_parent: &Path,
    canonical_root: &Path,
) -> io::Result<()> {
    let mut current = canonical_root.to_path_buf();
    let relative = package_parent.strip_prefix(canonical_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module package parent is outside its root",
        )
    })?;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module package path contains an invalid component",
            ));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Deno Module package parent is not a regular directory",
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(err) => return Err(err),
        }
        if !current.canonicalize()?.starts_with(canonical_root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module package path escapes its root",
            ));
        }
    }
    Ok(())
}

fn write_deno_module_file(path: &Path, bytes: Vec<u8>) -> Result<(), PedelecError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            deno_module_materialization_error("cannot create Deno Module file", path, err)
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|err| {
            deno_module_materialization_error("cannot write Deno Module file", path, err)
        })
}

fn deno_module_materialization_error(
    message: &'static str,
    _path: &Path,
    _err: io::Error,
) -> PedelecError {
    // Module materialization errors can cross the browser-facing upload
    // response.  Keep Core-owned workspace paths out of that response.
    PedelecError::new(error_codes::DENO_MODULE_MATERIALIZATION_FAILED, message)
}

fn cleanup_thread_private_runtime_artifacts(
    workspace_path: &Path,
    thread_id: &str,
) -> Result<(), PedelecError> {
    let skills_root = thread_skills_root(workspace_path, thread_id);
    ensure_thread_skills_root_is_safe(workspace_path, thread_id).map_err(|err| {
        PedelecError::with_details(
            error_codes::WORKSPACE_REMOVE_FAILED,
            "cannot inspect aborted thread skills state",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    match fs::symlink_metadata(&skills_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&skills_root).map_err(|err| {
                PedelecError::with_details(
                    error_codes::WORKSPACE_REMOVE_FAILED,
                    "cannot remove aborted thread skills state",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
        }
        Ok(_) => {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_REMOVE_FAILED,
                "aborted thread skills path is not a regular directory",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(PedelecError::with_details(
                error_codes::WORKSPACE_REMOVE_FAILED,
                "cannot inspect aborted thread skills state",
                serde_json::json!({ "error": err.to_string() }),
            ));
        }
    }

    let deno_thread_root = workspace_deno_thread_root(workspace_path, thread_id);
    ensure_deno_module_thread_root_is_safe(workspace_path, thread_id).map_err(|err| {
        deno_module_materialization_error(
            "cannot inspect aborted Deno Module state",
            &deno_thread_root,
            err,
        )
    })?;
    match fs::symlink_metadata(&deno_thread_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&deno_thread_root).map_err(|err| {
                deno_module_materialization_error(
                    "cannot remove aborted Deno Module state",
                    &deno_thread_root,
                    err,
                )
            })?;
        }
        Ok(_) => {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_REMOVE_FAILED,
                "aborted Deno Module state path is not a regular directory",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(deno_module_materialization_error(
                "cannot inspect aborted Deno Module state",
                &deno_thread_root,
                err,
            ));
        }
    }
    Ok(())
}

fn ensure_thread_skills_root_is_safe(workspace_path: &Path, thread_id: &str) -> io::Result<()> {
    validate_deno_module_thread_id(thread_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid thread id"))?;
    let workspace_metadata = fs::symlink_metadata(workspace_path)?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "thread skills workspace is not a regular directory",
        ));
    }
    let canonical_workspace = workspace_path.canonicalize()?;
    let mut current = workspace_path.to_path_buf();
    for component in [PEDELEC_RUNTIME_DATA_DIR, "threads", thread_id, "skills"] {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread skills path is not a regular directory",
            ));
        }
        if !current.canonicalize()?.starts_with(&canonical_workspace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread skills path escapes the workspace",
            ));
        }
    }
    Ok(())
}

fn ensure_deno_module_thread_root_is_safe(
    workspace_path: &Path,
    thread_id: &str,
) -> io::Result<()> {
    if thread_id.is_empty()
        || thread_id == "."
        || thread_id == ".."
        || thread_id.contains('/')
        || thread_id.contains('\\')
        || thread_id.contains('\0')
        || thread_id.contains(':')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module thread path is invalid",
        ));
    }
    let workspace_metadata = fs::symlink_metadata(workspace_path)?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deno Module workspace is not a regular directory",
        ));
    }
    let canonical_workspace = workspace_path.canonicalize()?;
    let mut current = workspace_path.to_path_buf();
    for component in [PEDELEC_RUNTIME_DATA_DIR, "deno", "threads", thread_id] {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module thread path is not a regular directory",
            ));
        }
        if !current.canonicalize()?.starts_with(&canonical_workspace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Deno Module thread path escapes the workspace",
            ));
        }
    }
    Ok(())
}

fn initialize_generated_skills(
    workspace: &Path,
    thread_id: &str,
    skills_input: Option<&CreateThreadSkillsInput>,
) -> Result<(Vec<SkillFile>, ToolRegistry), PedelecError> {
    let skills_dir = thread_skills_root(workspace, thread_id);
    fs::create_dir_all(&skills_dir).map_err(|err| {
        skill_download_error(
            "cannot create skills directory",
            None,
            Some(&skills_dir),
            err,
        )
    })?;
    let registry = ToolRegistry::from_skills_input(skills_input)?;
    let skills = write_generated_tool_specs(&skills_dir, &registry)?;
    Ok((skills, registry))
}

fn thread_event_log_path(workspace_path: &Path, thread_id: &str) -> PathBuf {
    workspace_logs_root(workspace_path).join(format!("events-{thread_id}-{}.jsonl", Uuid::new_v4()))
}

fn ensure_runtime_directory(path: &Path, message: &'static str) -> Result<(), PedelecError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(workspace_io_error(
            error_codes::WORKSPACE_CREATE_FAILED,
            message,
            path,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace runtime directory path is occupied by a non-directory entry",
            ),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|create_err| {
                workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    message,
                    path,
                    create_err,
                )
            })
        }
        Err(err) => Err(workspace_io_error(
            error_codes::WORKSPACE_CREATE_FAILED,
            message,
            path,
            err,
        )),
    }
}

fn workspace_path_invalid_error(
    message: &'static str,
    custom_path: &Path,
    managed_root: &Path,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_PATH_INVALID,
        message,
        serde_json::json!({
            "workspacePath": path_for_external_use(custom_path),
            "managedWorkspaceRoot": path_for_external_use(managed_root),
        }),
    )
}

fn workspace_path_invalid_io_error(
    message: &'static str,
    path: &Path,
    err: std::io::Error,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_PATH_INVALID,
        message,
        serde_json::json!({
            "workspacePath": path_for_external_use(path),
            "error": err.to_string(),
        }),
    )
}

fn resolve_path_for_overlap(path: &Path) -> Result<PathBuf, PedelecError> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| {
                workspace_path_invalid_io_error("cannot resolve current directory", path, err)
            })?
            .join(path)
    };
    let normalized_path = normalize_absolute_path(&absolute_path)?;
    let mut existing_ancestor = normalized_path.clone();
    let mut missing_components = Vec::new();

    while !existing_ancestor.exists() {
        let component = existing_ancestor.file_name().ok_or_else(|| {
            PedelecError::with_details(
                error_codes::WORKSPACE_PATH_INVALID,
                "cannot resolve workspace path ancestor",
                serde_json::json!({ "path": path_for_external_use(path) }),
            )
        })?;
        missing_components.push(component.to_os_string());
        if !existing_ancestor.pop() {
            return Err(PedelecError::with_details(
                error_codes::WORKSPACE_PATH_INVALID,
                "cannot resolve workspace path ancestor",
                serde_json::json!({ "path": path_for_external_use(path) }),
            ));
        }
    }

    let mut resolved = existing_ancestor.canonicalize().map_err(|err| {
        workspace_path_invalid_io_error(
            "cannot canonicalize workspace path ancestor",
            &existing_ancestor,
            err,
        )
    })?;
    for component in missing_components.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, PedelecError> {
    if !path.is_absolute() {
        return Err(PedelecError::with_details(
            error_codes::WORKSPACE_PATH_INVALID,
            "workspace path must be absolute",
            serde_json::json!({ "path": path_for_external_use(path) }),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn resolve_deno_workspace(thread_id: &str, workspace_path: &Path) -> Result<PathBuf, PedelecError> {
    let canonical_workspace = workspace_path.canonicalize().map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            "authoritative thread workspace could not be opened",
            serde_json::json!({
                "threadId": thread_id,
                "workspacePath": path_for_external_use(workspace_path),
                "error": err.to_string(),
            }),
        )
    })?;
    let workspace_metadata = fs::metadata(&canonical_workspace).map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            "authoritative thread workspace could not be inspected",
            serde_json::json!({
                "threadId": thread_id,
                "workspacePath": path_for_external_use(&canonical_workspace),
                "error": err.to_string(),
            }),
        )
    })?;
    if !workspace_metadata.is_dir() {
        return Err(PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            "authoritative thread workspace is not a directory",
            serde_json::json!({
                "threadId": thread_id,
                "workspacePath": path_for_external_use(&canonical_workspace),
            }),
        ));
    }
    Ok(canonical_workspace)
}

fn resolve_deno_entrypoint(
    thread_id: &str,
    workspace_path: &Path,
    entrypoint: &str,
) -> Result<(PathBuf, PathBuf), PedelecError> {
    let invalid = |message: &'static str, path: Option<&Path>| {
        let mut details = serde_json::Map::new();
        details.insert("threadId".to_string(), serde_json::json!(thread_id));
        details.insert("entrypoint".to_string(), serde_json::json!(entrypoint));
        if let Some(path) = path {
            details.insert(
                "path".to_string(),
                serde_json::json!(path_for_external_use(path)),
            );
        }
        PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            message,
            Value::Object(details),
        )
    };

    if entrypoint.trim().is_empty()
        || entrypoint.starts_with('-')
        || entrypoint.chars().any(char::is_control)
    {
        return Err(invalid("Deno entrypoint must be a non-empty path", None));
    }

    let relative = Path::new(entrypoint);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(invalid(
            "Deno entrypoint must be a workspace-relative path without traversal",
            Some(relative),
        ));
    }

    let canonical_workspace = resolve_deno_workspace(thread_id, workspace_path)?;

    let candidate = canonical_workspace.join(relative);
    let canonical_entrypoint = candidate.canonicalize().map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            "Deno entrypoint does not exist or could not be resolved",
            serde_json::json!({
                "threadId": thread_id,
                "entrypoint": entrypoint,
                "workspacePath": path_for_external_use(&canonical_workspace),
                "error": err.to_string(),
            }),
        )
    })?;

    if !path_is_prefix(&canonical_workspace, &canonical_entrypoint) {
        return Err(invalid(
            "Deno entrypoint resolves outside the authoritative workspace",
            Some(&canonical_entrypoint),
        ));
    }

    let metadata = fs::metadata(&canonical_entrypoint).map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_ENTRYPOINT_INVALID,
            "Deno entrypoint could not be inspected",
            serde_json::json!({
                "threadId": thread_id,
                "entrypoint": entrypoint,
                "error": err.to_string(),
            }),
        )
    })?;
    if !metadata.is_file() {
        return Err(invalid(
            "Deno entrypoint must resolve to a regular file",
            Some(&canonical_entrypoint),
        ));
    }

    Ok((canonical_workspace, canonical_entrypoint))
}

fn ensure_paths_do_not_overlap(
    custom_path: &Path,
    custom_comparison_path: &Path,
    managed_root: &Path,
    managed_comparison_path: &Path,
) -> Result<(), PedelecError> {
    if path_is_prefix(custom_comparison_path, managed_comparison_path)
        || path_is_prefix(managed_comparison_path, custom_comparison_path)
    {
        return Err(workspace_path_invalid_error(
            "custom workspace path overlaps the managed workspace root",
            custom_path,
            managed_root,
        ));
    }
    Ok(())
}

fn path_is_prefix(parent: &Path, target: &Path) -> bool {
    let mut parent_components = parent.components();
    let mut target_components = target.components();
    loop {
        match (parent_components.next(), target_components.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(parent), Some(target)) if path_components_equal(parent, target) => {}
            (Some(_), Some(_)) => return false,
        }
    }
}

#[cfg(windows)]
fn path_components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(not(windows))]
fn path_components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left == right
}

#[derive(Debug, Clone)]
pub struct SkillManager {
    max_file_size_bytes: u64,
}

impl Default for SkillManager {
    fn default() -> Self {
        Self {
            max_file_size_bytes: DEFAULT_MAX_SKILL_SIZE_BYTES,
        }
    }
}

impl SkillManager {
    pub fn with_max_file_size_bytes(max_file_size_bytes: u64) -> Self {
        Self {
            max_file_size_bytes,
        }
    }

    pub fn download_skills(
        &self,
        skills_dir: impl AsRef<Path>,
        skills_urls: &[String],
    ) -> Result<Vec<SkillFile>, PedelecError> {
        let skills_dir = skills_dir.as_ref();
        fs::create_dir_all(skills_dir).map_err(|err| {
            skill_download_error(
                "cannot create skills directory",
                None,
                Some(skills_dir),
                err,
            )
        })?;

        let canonical_skills_dir = skills_dir.canonicalize().map_err(|err| {
            skill_download_error(
                "cannot canonicalize skills directory",
                None,
                Some(skills_dir),
                err,
            )
        })?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| {
                PedelecError::with_details(
                    error_codes::SKILL_DOWNLOAD_FAILED,
                    "cannot create skill download client",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;

        let mut used_filenames = HashMap::<String, usize>::new();
        let mut downloaded = Vec::with_capacity(skills_urls.len());

        for skill_url in skills_urls {
            let (url, original_filename, safe_filename) =
                validate_skill_url_and_filename(skill_url)?;
            let target_filename = unique_available_filename(
                &safe_filename,
                &canonical_skills_dir,
                &mut used_filenames,
            );
            let target_path = canonical_skills_dir.join(&target_filename);
            ensure_child_path(&canonical_skills_dir, &target_path)?;

            let bytes = self.download_skill_bytes(&client, &url)?;
            fs::write(&target_path, &bytes).map_err(|err| {
                skill_download_error(
                    "cannot write downloaded skill",
                    Some(url.as_str()),
                    Some(&target_path),
                    err,
                )
            })?;

            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let sha256 = format!("{:x}", hasher.finalize());

            downloaded.push(SkillFile {
                original_url: skill_url.clone(),
                original_filename,
                local_path: target_path,
                sha256,
                size_bytes: bytes.len() as u64,
            });
        }

        Ok(downloaded)
    }

    fn download_skill_bytes(
        &self,
        client: &reqwest::blocking::Client,
        url: &Url,
    ) -> Result<Vec<u8>, PedelecError> {
        let mut response = client.get(url.clone()).send().map_err(|err| {
            PedelecError::with_details(
                error_codes::SKILL_DOWNLOAD_FAILED,
                "cannot download skill",
                serde_json::json!({ "url": url.as_str(), "error": err.to_string() }),
            )
        })?;

        if !response.status().is_success() {
            return Err(PedelecError::with_details(
                error_codes::SKILL_DOWNLOAD_FAILED,
                "skill download returned non-success status",
                serde_json::json!({ "url": url.as_str(), "status": response.status().as_u16() }),
            ));
        }

        if response
            .content_length()
            .is_some_and(|len| len > self.max_file_size_bytes)
        {
            return Err(PedelecError::with_details(
                error_codes::SKILL_DOWNLOAD_FAILED,
                "downloaded skill exceeds size limit",
                serde_json::json!({
                    "url": url.as_str(),
                    "maxSizeBytes": self.max_file_size_bytes
                }),
            ));
        }

        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(self.max_file_size_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| {
                PedelecError::with_details(
                    error_codes::SKILL_DOWNLOAD_FAILED,
                    "cannot read downloaded skill",
                    serde_json::json!({ "url": url.as_str(), "error": err.to_string() }),
                )
            })?;

        if bytes.len() as u64 > self.max_file_size_bytes {
            return Err(PedelecError::with_details(
                error_codes::SKILL_DOWNLOAD_FAILED,
                "downloaded skill exceeds size limit",
                serde_json::json!({
                    "url": url.as_str(),
                    "maxSizeBytes": self.max_file_size_bytes
                }),
            ));
        }

        Ok(bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub args_schema: Value,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedToolCall {
    pub args: Value,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolRegistry {
    guidance: Option<String>,
    tools: HashMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn from_skills_input(
        skills: Option<&CreateThreadSkillsInput>,
    ) -> Result<Self, PedelecError> {
        let Some(skills) = skills else {
            return Ok(Self::default());
        };

        let mut tools = HashMap::with_capacity(skills.tools.len());
        for raw_tool in &skills.tools {
            validate_tool_name(&raw_tool.name)?;
            if raw_tool.description.trim().is_empty() {
                return Err(PedelecError::with_details(
                    error_codes::TOOLS_MANIFEST_INVALID,
                    "tool description must be a non-empty string",
                    serde_json::json!({ "toolName": raw_tool.name }),
                ));
            }
            if tools.contains_key(&raw_tool.name) {
                return Err(PedelecError::with_details(
                    error_codes::TOOLS_MANIFEST_INVALID,
                    "duplicate tool name in tools manifest",
                    serde_json::json!({ "toolName": raw_tool.name }),
                ));
            }
            validate_tool_args_schema(&raw_tool.name, &raw_tool.args_schema)?;
            let timeout_ms = raw_tool.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
            if timeout_ms == 0 {
                return Err(PedelecError::with_details(
                    error_codes::TOOLS_MANIFEST_INVALID,
                    "tool timeoutMs must be a positive integer",
                    serde_json::json!({ "toolName": raw_tool.name }),
                ));
            }

            tools.insert(
                raw_tool.name.clone(),
                ToolDefinition {
                    name: raw_tool.name.clone(),
                    description: raw_tool.description.clone(),
                    args_schema: raw_tool.args_schema.clone(),
                    timeout_ms,
                },
            );
        }

        Ok(Self {
            guidance: Some(skills.guidance.clone()),
            tools,
        })
    }

    pub fn load_from_skills_dir(skills_dir: impl AsRef<Path>) -> Result<Self, PedelecError> {
        let skills_dir = skills_dir.as_ref();
        let tools_json_path = skills_dir.join("tools.json");
        if !tools_json_path.exists() {
            let mut generated_paths = Vec::new();
            let entries = match fs::read_dir(skills_dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self::default());
                }
                Err(err) => {
                    return Err(PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "cannot read generated tool specs directory",
                        serde_json::json!({
                            "path": skills_dir.to_string_lossy(),
                            "error": err.to_string()
                        }),
                    ));
                }
            };
            for entry in entries {
                let entry = entry.map_err(|err| {
                    PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "cannot read generated tool spec entry",
                        serde_json::json!({
                            "path": skills_dir.to_string_lossy(),
                            "error": err.to_string()
                        }),
                    )
                })?;
                let path = entry.path();
                if entry
                    .file_type()
                    .map_err(|err| {
                        PedelecError::with_details(
                            error_codes::TOOLS_JSON_INVALID,
                            "cannot inspect generated tool spec entry",
                            serde_json::json!({
                                "path": path.to_string_lossy(),
                                "error": err.to_string()
                            }),
                        )
                    })?
                    .is_file()
                    && path.extension().and_then(OsStr::to_str) == Some("json")
                    && path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| name.starts_with("tools-"))
                {
                    generated_paths.push(path);
                }
            }
            generated_paths.sort();

            let mut tools = HashMap::with_capacity(generated_paths.len());
            for path in generated_paths {
                let contents = fs::read_to_string(&path).map_err(|err| {
                    PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "cannot read generated tool spec",
                        serde_json::json!({
                            "path": path.to_string_lossy(),
                            "error": err.to_string()
                        }),
                    )
                })?;
                let tool: ToolDefinition = serde_json::from_str(&contents).map_err(|err| {
                    PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "generated tool spec is not valid JSON",
                        serde_json::json!({
                            "path": path.to_string_lossy(),
                            "error": err.to_string()
                        }),
                    )
                })?;
                validate_tool_name_legacy(&tool.name)?;
                validate_tool_args_schema_legacy(&tool.name, &tool.args_schema)?;
                if tool.timeout_ms == 0 {
                    return Err(PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "generated tool timeoutMs must be a positive integer",
                        serde_json::json!({ "toolName": tool.name }),
                    ));
                }
                if tools.insert(tool.name.clone(), tool).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::TOOLS_JSON_INVALID,
                        "duplicate generated tool name",
                        serde_json::json!({ "path": path.to_string_lossy() }),
                    ));
                }
            }

            return Ok(Self {
                // Generated specs do not persist the original guidance text,
                // but a non-None marker keeps the provider tool configuration
                // visible when this registry is used for a revived turn.
                guidance: (!tools.is_empty()).then_some(String::new()),
                tools,
            });
        }

        let tools_json = fs::read_to_string(&tools_json_path).map_err(|err| {
            PedelecError::with_details(
                error_codes::TOOLS_JSON_INVALID,
                "cannot read tools.json",
                serde_json::json!({
                    "path": tools_json_path.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })?;
        Self::from_tools_json_str(&tools_json)
    }

    pub fn from_tools_json_str(tools_json: &str) -> Result<Self, PedelecError> {
        let raw: RawToolRegistry = serde_json::from_str(tools_json).map_err(|err| {
            PedelecError::with_details(
                error_codes::TOOLS_JSON_INVALID,
                "tools.json is not valid JSON",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;

        let mut tools = HashMap::with_capacity(raw.tools.len());
        for raw_tool in raw.tools {
            validate_tool_name_legacy(&raw_tool.name)?;
            if tools.contains_key(&raw_tool.name) {
                return Err(PedelecError::with_details(
                    error_codes::TOOLS_JSON_INVALID,
                    "duplicate tool name in tools.json",
                    serde_json::json!({ "toolName": raw_tool.name }),
                ));
            }
            validate_tool_args_schema_legacy(&raw_tool.name, &raw_tool.args_schema)?;

            let timeout_ms = raw_tool.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
            tools.insert(
                raw_tool.name.clone(),
                ToolDefinition {
                    name: raw_tool.name,
                    description: raw_tool.description,
                    args_schema: raw_tool.args_schema,
                    timeout_ms,
                },
            );
        }

        Ok(Self {
            guidance: None,
            tools,
        })
    }

    pub fn validate_tool_call(&self, tool_name: &str, args: &Value) -> Result<u64, PedelecError> {
        Ok(self.normalize_tool_call(tool_name, args)?.timeout_ms)
    }

    pub fn normalize_tool_call(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Result<NormalizedToolCall, PedelecError> {
        let tool = self.tools.get(tool_name).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOL_NOT_FOUND,
                "tool was not found in registry",
                serde_json::json!({ "toolName": tool_name }),
            )
        })?;

        if !args.is_object() {
            return Err(PedelecError::with_details(
                error_codes::TOOL_ARGS_INVALID,
                "tool args must be a JSON object",
                serde_json::json!({ "toolName": tool_name }),
            ));
        }

        let schema_defines_timeout_ms =
            tool_schema_defines_top_level_property(&tool.args_schema, TOOL_TIMEOUT_OVERRIDE_FIELD);
        let mut normalized_args = args.clone();
        let mut timeout_override_ms = None;
        if let Value::Object(args_object) = &mut normalized_args {
            if let Some(timeout_value) = args_object.get(TOOL_TIMEOUT_OVERRIDE_FIELD).cloned() {
                timeout_override_ms = Some(parse_tool_timeout_override(tool_name, &timeout_value)?);
                if !schema_defines_timeout_ms {
                    args_object.remove(TOOL_TIMEOUT_OVERRIDE_FIELD);
                }
            }
        }

        let validator = jsonschema::validator_for(&tool.args_schema).map_err(|err| {
            PedelecError::with_details(
                error_codes::TOOLS_JSON_INVALID,
                "tool argsSchema cannot be compiled",
                serde_json::json!({ "toolName": tool_name, "error": err.to_string() }),
            )
        })?;

        validator.validate(&normalized_args).map_err(|err| {
            PedelecError::with_details(
                error_codes::TOOL_ARGS_INVALID,
                "tool args do not match schema",
                serde_json::json!({ "toolName": tool_name, "error": err.to_string() }),
            )
        })?;

        Ok(NormalizedToolCall {
            args: normalized_args,
            timeout_ms: timeout_override_ms.unwrap_or(tool.timeout_ms),
        })
    }

    pub fn get(&self, tool_name: &str) -> Option<&ToolDefinition> {
        self.tools.get(tool_name)
    }

    pub fn tools(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values()
    }

    pub fn guidance(&self) -> Option<&str> {
        self.guidance.as_deref()
    }

    pub fn has_skills_configuration(&self) -> bool {
        self.guidance.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolRegistryStore {
    registries: HashMap<String, ToolRegistry>,
}

impl ToolRegistryStore {
    pub fn insert(&mut self, thread_id: impl Into<String>, registry: ToolRegistry) {
        self.registries.insert(thread_id.into(), registry);
    }

    pub fn get(&self, thread_id: &str) -> Option<&ToolRegistry> {
        self.registries.get(thread_id)
    }

    pub fn remove(&mut self, thread_id: &str) -> Option<ToolRegistry> {
        self.registries.remove(thread_id)
    }
}

#[derive(Debug, Deserialize)]
struct RawToolRegistry {
    tools: Vec<RawToolDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawToolDefinition {
    name: String,
    description: String,
    args_schema: Value,
    timeout_ms: Option<u64>,
}

fn validate_tool_name(tool_name: &str) -> Result<(), PedelecError> {
    if !is_valid_tool_name(tool_name) {
        return Err(PedelecError::with_details(
            error_codes::TOOLS_MANIFEST_INVALID,
            "tool name is invalid",
            serde_json::json!({ "toolName": tool_name }),
        ));
    }
    Ok(())
}

fn validate_tool_name_legacy(tool_name: &str) -> Result<(), PedelecError> {
    if !is_valid_tool_name(tool_name) {
        return Err(PedelecError::with_details(
            error_codes::TOOLS_JSON_INVALID,
            "tool name is invalid",
            serde_json::json!({ "toolName": tool_name }),
        ));
    }
    Ok(())
}

fn is_valid_tool_name(tool_name: &str) -> bool {
    let mut chars = tool_name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-')
}

fn validate_tool_args_schema(tool_name: &str, args_schema: &Value) -> Result<(), PedelecError> {
    validate_tool_args_schema_with_code(tool_name, args_schema, error_codes::TOOLS_MANIFEST_INVALID)
}

fn validate_tool_args_schema_legacy(
    tool_name: &str,
    args_schema: &Value,
) -> Result<(), PedelecError> {
    validate_tool_args_schema_with_code(tool_name, args_schema, error_codes::TOOLS_JSON_INVALID)
}

fn validate_tool_args_schema_with_code(
    tool_name: &str,
    args_schema: &Value,
    error_code: &str,
) -> Result<(), PedelecError> {
    jsonschema::meta::validate(args_schema).map_err(|err| {
        PedelecError::with_details(
            error_code,
            "tool argsSchema is not a valid JSON Schema",
            serde_json::json!({ "toolName": tool_name, "error": err.to_string() }),
        )
    })?;
    jsonschema::validator_for(args_schema).map_err(|err| {
        PedelecError::with_details(
            error_code,
            "tool argsSchema cannot be compiled",
            serde_json::json!({ "toolName": tool_name, "error": err.to_string() }),
        )
    })?;
    Ok(())
}

fn parse_tool_timeout_override(
    tool_name: &str,
    timeout_value: &Value,
) -> Result<u64, PedelecError> {
    timeout_value
        .as_u64()
        .filter(|timeout_ms| *timeout_ms > 0)
        .ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOL_ARGS_INVALID,
                "tool timeoutMs must be a positive integer",
                serde_json::json!({
                    "toolName": tool_name,
                    "field": TOOL_TIMEOUT_OVERRIDE_FIELD
                }),
            )
        })
}

fn tool_schema_defines_top_level_property(args_schema: &Value, property_name: &str) -> bool {
    args_schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(property_name))
}

#[derive(Debug)]
pub struct PendingToolInvocation {
    pub request: PendingToolRequest,
    pub deadline: Instant,
    waiters: Vec<mpsc::Sender<ToolInvocationOutcome>>,
}

impl PendingToolInvocation {
    fn broadcast(&mut self, outcome: ToolInvocationOutcome) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(outcome.clone());
        }
    }
}

#[derive(Debug)]
struct ReplayableToolInvocation {
    request: PendingToolRequest,
    outcome: ToolInvocationOutcome,
    completed_at: Instant,
    expires_at: Instant,
}

#[derive(Debug, Default)]
pub struct ToolRequestBroker {
    pending: HashMap<String, PendingToolInvocation>,
    replayable: HashMap<String, ReplayableToolInvocation>,
    next_request_number: u64,
}

impl ToolRequestBroker {
    pub fn pending_for_thread(&self, thread_id: &str) -> Option<PendingToolRequest> {
        self.pending
            .values()
            .find(|pending| pending.request.thread_id == thread_id)
            .map(|pending| pending.request.clone())
    }

    pub fn begin_or_join(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
    ) -> Result<ToolInvocationRegistration, PedelecError> {
        let operation_id = resolve_operation_id(None).expect("operation ID generation cannot fail");
        self.begin_or_join_for_operation(thread_id, tool_name, args, timeout_ms, operation_id)
    }

    pub fn begin_or_join_for_operation(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
        operation_id: String,
    ) -> Result<ToolInvocationRegistration, PedelecError> {
        self.purge_expired_replay_candidates(Instant::now());
        let pending_id = self
            .pending
            .iter()
            .find(|(_, pending)| pending.request.thread_id == thread_id)
            .map(|(request_id, _)| request_id.clone());

        if let Some(pending_id) = pending_id {
            let pending = self
                .pending
                .get_mut(&pending_id)
                .expect("pending request exists");
            if pending.request.tool_name != tool_name || pending.request.args != args {
                return Err(PedelecError::with_details(
                    error_codes::PENDING_TOOL_REQUEST_EXISTS,
                    "thread already has a pending tool request",
                    serde_json::json!({ "threadId": thread_id }),
                ));
            }

            let (result_tx, result_rx) = mpsc::channel();
            pending.waiters.push(result_tx);
            return Ok(ToolInvocationRegistration::Joined(ToolInvocationWait {
                request_id: pending.request.request_id.clone(),
                timeout_ms: pending.request.timeout_ms,
                remaining_timeout: remaining_timeout(pending.deadline),
                result_rx,
            }));
        }

        let replay_id = self
            .replayable
            .values()
            .find(|candidate| {
                candidate.request.thread_id == thread_id
                    && candidate.request.tool_name == tool_name
                    && candidate.request.args == args
            })
            .map(|candidate| candidate.request.request_id.clone());
        if let Some(replay_id) = replay_id {
            let candidate = self
                .replayable
                .get(&replay_id)
                .expect("replay candidate exists");
            let (result_tx, result_rx) = mpsc::channel();
            let outcome = candidate.outcome.clone();
            let _ = result_tx.send(outcome);
            return Ok(ToolInvocationRegistration::Replayed(ToolInvocationWait {
                request_id: candidate.request.request_id.clone(),
                timeout_ms: candidate.request.timeout_ms,
                remaining_timeout: Duration::ZERO,
                result_rx,
            }));
        }

        Ok(ToolInvocationRegistration::Created(self.create_new(
            thread_id,
            tool_name,
            args,
            timeout_ms,
            operation_id,
        )))
    }

    pub fn create_pending(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
    ) -> Result<(String, mpsc::Receiver<ToolInvocationOutcome>), PedelecError> {
        if self.has_pending_for_thread(&thread_id) {
            return Err(PedelecError::with_details(
                error_codes::PENDING_TOOL_REQUEST_EXISTS,
                "thread already has a pending tool request",
                serde_json::json!({ "threadId": thread_id }),
            ));
        }

        let operation_id = resolve_operation_id(None).expect("operation ID generation cannot fail");
        let wait = self.create_new(thread_id, tool_name, args, timeout_ms, operation_id);
        Ok((wait.request_id, wait.result_rx))
    }

    fn create_new(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
        operation_id: String,
    ) -> ToolInvocationWait {
        self.next_request_number += 1;
        let request_id = format!(
            "toolreq_{}_{}",
            Utc::now().timestamp_millis(),
            self.next_request_number
        );
        let (result_tx, result_rx) = mpsc::channel();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let request = PendingToolRequest {
            request_id: request_id.clone(),
            thread_id,
            operation_id,
            tool_name,
            args,
            created_at: Utc::now(),
            timeout_ms,
        };

        self.pending.insert(
            request_id.clone(),
            PendingToolInvocation {
                request,
                deadline,
                waiters: vec![result_tx],
            },
        );

        ToolInvocationWait {
            request_id,
            timeout_ms,
            remaining_timeout: remaining_timeout(deadline),
            result_rx,
        }
    }

    pub fn has_pending_for_thread(&self, thread_id: &str) -> bool {
        self.pending
            .values()
            .any(|pending| pending.request.thread_id == thread_id)
    }

    pub fn get(&self, request_id: &str) -> Option<&PendingToolInvocation> {
        self.pending.get(request_id)
    }

    pub fn remove(&mut self, request_id: &str) -> Option<PendingToolInvocation> {
        self.pending.remove(request_id)
    }

    fn terminalize(
        &mut self,
        request_id: &str,
        outcome: ToolInvocationOutcome,
    ) -> Option<PendingToolInvocation> {
        let pending = self.pending.remove(request_id)?;
        self.remember_replay_candidate(pending.request.clone(), outcome);
        Some(pending)
    }

    fn remember_replay_candidate(
        &mut self,
        request: PendingToolRequest,
        outcome: ToolInvocationOutcome,
    ) {
        let now = Instant::now();
        self.purge_expired_replay_candidates(now);
        self.replayable.insert(
            request.request_id.clone(),
            ReplayableToolInvocation {
                request,
                outcome,
                completed_at: now,
                expires_at: now + TOOL_RESULT_REPLAY_WINDOW,
            },
        );
        while self.replayable.len() > TOOL_RESULT_REPLAY_MAX_ENTRIES {
            let oldest_id = self
                .replayable
                .iter()
                .min_by_key(|(_, candidate)| {
                    (candidate.completed_at, &candidate.request.request_id)
                })
                .map(|(request_id, _)| request_id.clone());
            if let Some(oldest_id) = oldest_id {
                self.replayable.remove(&oldest_id);
            } else {
                break;
            }
        }
    }

    fn purge_expired_replay_candidates(&mut self, now: Instant) {
        self.replayable
            .retain(|_, candidate| candidate.expires_at > now);
    }

    pub fn acknowledge_tool_delivery(&mut self, request_id: &str) {
        self.replayable.remove(request_id);
    }

    pub fn replay_candidate_count(&mut self) -> usize {
        self.purge_expired_replay_candidates(Instant::now());
        self.replayable.len()
    }

    pub fn purge_expired_replay_candidates_at(&mut self, now: Instant) {
        self.purge_expired_replay_candidates(now);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn waiter_count(&self, request_id: &str) -> Option<usize> {
        self.pending
            .get(request_id)
            .map(|pending| pending.waiters.len())
    }

    pub fn remaining_timeout(&self, request_id: &str) -> Option<Duration> {
        self.pending
            .get(request_id)
            .map(|pending| remaining_timeout(pending.deadline))
    }

    pub fn clear_thread(&mut self, thread_id: &str) {
        self.pending
            .retain(|_, pending| pending.request.thread_id != thread_id);
        self.replayable
            .retain(|_, candidate| candidate.request.thread_id != thread_id);
    }

    /// Remove pending invocations and wake every waiter with a terminal Core
    /// error. Runtime failures use this instead of silently dropping the
    /// sender, so a tool-call IPC request observes the actual provider failure
    /// rather than being misreported as a local timeout.
    pub fn clear_thread_with_error(&mut self, thread_id: &str, error: PedelecError) {
        let request_ids = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.request.thread_id == thread_id)
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        let outcome = ToolInvocationOutcome::CoreError(error);
        for request_id in request_ids {
            if let Some(mut pending) = self.pending.remove(&request_id) {
                pending.broadcast(outcome.clone());
            }
        }
        self.replayable
            .retain(|_, candidate| candidate.request.thread_id != thread_id);
    }
}

fn remaining_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[derive(Debug, Default)]
pub struct EventBus {
    next_seq_by_thread: HashMap<String, u64>,
    subscribers_by_thread: HashMap<String, Vec<mpsc::Sender<ThreadEvent>>>,
    all_thread_subscribers: Vec<mpsc::Sender<ThreadEvent>>,
    event_log_paths_by_thread: HashMap<String, PathBuf>,
}

impl EventBus {
    pub fn register_thread_log(&mut self, thread_id: &str, path: PathBuf) {
        self.event_log_paths_by_thread
            .insert(thread_id.to_string(), path);
    }

    pub fn unregister_thread_log(&mut self, thread_id: &str) {
        self.event_log_paths_by_thread.remove(thread_id);
    }

    pub fn remove_thread(&mut self, thread_id: &str) {
        self.next_seq_by_thread.remove(thread_id);
        self.subscribers_by_thread.remove(thread_id);
        self.event_log_paths_by_thread.remove(thread_id);
    }

    pub fn event_log_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.event_log_paths_by_thread.get(thread_id).cloned()
    }

    pub fn subscribe(&mut self, thread_id: &str) -> mpsc::Receiver<ThreadEvent> {
        let (tx, rx) = mpsc::channel();
        self.subscribers_by_thread
            .entry(thread_id.to_string())
            .or_default()
            .push(tx);
        rx
    }

    pub fn subscribe_all(&mut self) -> mpsc::Receiver<ThreadEvent> {
        let (tx, rx) = mpsc::channel();
        self.all_thread_subscribers.push(tx);
        rx
    }

    pub fn latest_seq(&self, thread_id: &str) -> u64 {
        self.next_seq_by_thread.get(thread_id).copied().unwrap_or(0)
    }

    pub fn emit_created(&mut self, thread_id: &str) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::Created {
                seq,
                thread_id: thread_id.to_string(),
            },
        );
    }

    pub fn emit_status_changed(&mut self, thread_id: &str, status: ThreadStatus) {
        self.emit_status_changed_for_operation(thread_id, status, None);
    }

    pub fn emit_status_changed_for_operation(
        &mut self,
        thread_id: &str,
        status: ThreadStatus,
        operation_id: Option<&str>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::StatusChanged {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.map(ToOwned::to_owned),
                status,
            },
        );
    }

    pub fn emit_assistant_delta(&mut self, thread_id: &str, text: String) {
        self.emit_assistant_delta_for_operation(thread_id, text, None);
    }

    pub fn emit_assistant_delta_for_operation(
        &mut self,
        thread_id: &str,
        text: String,
        operation_id: Option<&str>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::AssistantDelta {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.map(ToOwned::to_owned),
                text,
            },
        );
    }

    pub fn emit_assistant_message(&mut self, thread_id: &str, text: String) {
        self.emit_assistant_message_for_operation(thread_id, text, None);
    }

    pub fn emit_assistant_message_for_operation(
        &mut self,
        thread_id: &str,
        text: String,
        operation_id: Option<&str>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::AssistantMessage {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.map(ToOwned::to_owned),
                text,
            },
        );
    }

    pub fn emit_tool_call(
        &mut self,
        thread_id: &str,
        operation_id: &str,
        request_id: &str,
        tool_name: &str,
        args: Value,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::ToolCall {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.to_string(),
                request_id: request_id.to_string(),
                tool_name: tool_name.to_string(),
                args,
            },
        );
    }

    pub fn emit_tool_result(
        &mut self,
        thread_id: &str,
        operation_id: &str,
        request_id: &str,
        tool_name: &str,
        result: Value,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::ToolResult {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.to_string(),
                request_id: request_id.to_string(),
                tool_name: tool_name.to_string(),
                result,
            },
        );
    }

    pub fn emit_provider_session_id_updated(
        &mut self,
        thread_id: &str,
        provider_session_id: String,
    ) {
        self.emit_provider_session_id_updated_for_operation(thread_id, provider_session_id, None);
    }

    pub fn emit_provider_session_id_updated_for_operation(
        &mut self,
        thread_id: &str,
        provider_session_id: String,
        operation_id: Option<&str>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::ProviderSessionIdUpdated {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.map(ToOwned::to_owned),
                provider_session_id,
            },
        );
    }

    pub fn emit_usage_updated(&mut self, thread_id: &str, total_tokens: u64) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::UsageUpdated {
                seq,
                thread_id: thread_id.to_string(),
                total_tokens,
            },
        );
    }

    pub fn emit_operation_completed(
        &mut self,
        thread_id: &str,
        operation_id: &str,
        operation_kind: ThreadOperationKind,
        success: bool,
        error: Option<PedelecError>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::OperationCompleted {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.to_string(),
                operation_kind,
                success,
                error,
            },
        );
    }

    pub fn emit_provider_error(
        &mut self,
        thread_id: &str,
        provider: ProviderCode,
        error: PedelecError,
    ) {
        self.emit_provider_error_for_operation(thread_id, provider, error, None);
    }

    pub fn emit_provider_error_for_operation(
        &mut self,
        thread_id: &str,
        provider: ProviderCode,
        error: PedelecError,
        operation_id: Option<&str>,
    ) {
        self.emit_error_for_operation(
            thread_id,
            ThreadErrorSource::Provider { provider },
            error,
            operation_id,
        );
    }

    pub fn emit_core_error(&mut self, thread_id: &str, error: PedelecError) {
        self.emit_error_for_operation(thread_id, ThreadErrorSource::Core, error, None);
    }

    pub fn emit_error_for_operation(
        &mut self,
        thread_id: &str,
        source: ThreadErrorSource,
        error: PedelecError,
        operation_id: Option<&str>,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::Error {
                seq,
                thread_id: thread_id.to_string(),
                operation_id: operation_id.map(ToOwned::to_owned),
                source,
                error,
            },
        );
    }

    pub fn emit_ended(&mut self, thread_id: &str) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::Ended {
                seq,
                thread_id: thread_id.to_string(),
            },
        );
    }

    fn next_seq(&mut self, thread_id: &str) -> u64 {
        let seq = self
            .next_seq_by_thread
            .entry(thread_id.to_string())
            .or_insert(0);
        *seq += 1;
        *seq
    }

    fn emit(&mut self, thread_id: &str, event: ThreadEvent) {
        self.write_event_log(thread_id, &event);
        self.all_thread_subscribers
            .retain(|tx| tx.send(event.clone()).is_ok());
        if let Some(subscribers) = self.subscribers_by_thread.get_mut(thread_id) {
            subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }

    fn write_event_log(&self, thread_id: &str, event: &ThreadEvent) {
        let Some(path) = self.event_log_paths_by_thread.get(thread_id) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }

        let record = serde_json::json!({
            "ts": Utc::now(),
            "seq": event.seq(),
            "event": event,
        });
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }
}

fn safe_asset_filename(filename: &str) -> String {
    let mut value: String = filename
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    value = value
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();
    if value.is_empty()
        || matches!(
            value.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "LPT1"
        )
    {
        value = "upload".to_string();
    }
    value.chars().take(180).collect()
}

pub fn normalize_sdk_origin(value: &str) -> Result<String, PedelecError> {
    let url = Url::parse(value).map_err(|_| invalid_sdk_origin_error())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_sdk_origin_error());
    }
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        return Err(invalid_sdk_origin_error());
    }
    Ok(origin)
}

fn normalize_workspace_origin(value: &str) -> Result<String, PedelecError> {
    normalize_sdk_origin(value).map_err(|_| {
        PedelecError::new(
            error_codes::WORKSPACE_ACCESS_DENIED,
            "invalid caller origin for workspace access",
        )
    })
}

#[derive(Debug, Clone, Copy)]
enum WorkspaceListKind {
    Files,
    Folders,
}

fn list_workspace_descendants(
    workspace: &WorkspaceState,
    requested_path: Option<&str>,
    kind: WorkspaceListKind,
) -> Result<WorkspaceListOutput, PedelecError> {
    let start = resolve_workspace_directory(workspace, requested_path)?;
    let mut pending = vec![start];
    let mut paths = Vec::new();
    let mut estimated_json_bytes = br#"{"paths":[ ]}"#.len();

    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| {
            workspace_list_io_error(
                &workspace.workspace_id,
                requested_path,
                "cannot read workspace directory",
                error,
            )
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| {
                workspace_list_io_error(
                    &workspace.workspace_id,
                    requested_path,
                    "cannot read workspace directory entry",
                    error,
                )
            })?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path).map_err(|error| {
                workspace_list_io_error(
                    &workspace.workspace_id,
                    requested_path,
                    "cannot inspect workspace directory entry",
                    error,
                )
            })?;
            if is_link_or_junction(&metadata) {
                continue;
            }

            let relative_path = workspace_relative_path(&workspace.canonical_path, &entry_path)?;
            if metadata.is_dir() {
                if matches!(kind, WorkspaceListKind::Folders) {
                    push_workspace_list_path(
                        &workspace.workspace_id,
                        requested_path,
                        relative_path,
                        &mut paths,
                        &mut estimated_json_bytes,
                    )?;
                }
                pending.push(entry_path);
            } else if metadata.is_file() && matches!(kind, WorkspaceListKind::Files) {
                push_workspace_list_path(
                    &workspace.workspace_id,
                    requested_path,
                    relative_path,
                    &mut paths,
                    &mut estimated_json_bytes,
                )?;
            }
        }
    }

    paths.sort();
    Ok(WorkspaceListOutput { paths })
}

fn push_workspace_list_path(
    workspace_id: &str,
    requested_path: Option<&str>,
    path: String,
    paths: &mut Vec<String>,
    estimated_json_bytes: &mut usize,
) -> Result<(), PedelecError> {
    let encoded_len = serde_json::to_vec(&path)
        .map(|value| value.len())
        .unwrap_or_else(|_| path.len().saturating_add(2));
    *estimated_json_bytes = estimated_json_bytes
        .saturating_add(encoded_len)
        .saturating_add(usize::from(!paths.is_empty()));
    if *estimated_json_bytes > WORKSPACE_LIST_EARLY_LIMIT_BYTES {
        return Err(workspace_list_too_large_error(workspace_id, requested_path));
    }
    paths.push(path);
    Ok(())
}

fn resolve_workspace_root(workspace: &WorkspaceState) -> Result<PathBuf, PedelecError> {
    let metadata = fs::symlink_metadata(&workspace.canonical_path).map_err(|error| {
        workspace_list_io_error(
            &workspace.workspace_id,
            None,
            "workspace is no longer available",
            error,
        )
    })?;
    if is_link_or_junction(&metadata) || !metadata.is_dir() {
        return Err(workspace_path_error(
            &workspace.workspace_id,
            None,
            "workspace root is not a directory",
        ));
    }
    Ok(workspace.canonical_path.clone())
}

fn resolve_workspace_directory(
    workspace: &WorkspaceState,
    requested_path: Option<&str>,
) -> Result<PathBuf, PedelecError> {
    let root = resolve_workspace_root(workspace)?;
    let relative = match requested_path {
        None => PathBuf::new(),
        Some(value) => workspace_relative_input(value, &workspace.workspace_id)?,
    };
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(workspace_path_error(
                &workspace.workspace_id,
                requested_path,
                "workspace path contains a malformed component",
            ));
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            workspace_path_io_error(
                &workspace.workspace_id,
                requested_path,
                "workspace path could not be opened",
                error,
            )
        })?;
        if is_link_or_junction(&metadata) {
            return Err(workspace_path_error(
                &workspace.workspace_id,
                requested_path,
                "workspace path contains a symbolic link or junction",
            ));
        }
    }

    let metadata = fs::symlink_metadata(&current).map_err(|error| {
        workspace_path_io_error(
            &workspace.workspace_id,
            requested_path,
            "workspace path could not be opened",
            error,
        )
    })?;
    if !metadata.is_dir() {
        return Err(workspace_path_error(
            &workspace.workspace_id,
            requested_path,
            "workspace path is not a directory",
        ));
    }
    Ok(current)
}

fn workspace_relative_input(value: &str, workspace_id: &str) -> Result<PathBuf, PedelecError> {
    if value.is_empty() || value.contains('\0') {
        return Err(workspace_path_error(
            workspace_id,
            Some(value),
            "workspace path is empty or contains NUL",
        ));
    }
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        return Err(workspace_path_error(
            workspace_id,
            Some(value),
            "workspace path must be relative",
        ));
    }

    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(workspace_path_error(
                    workspace_id,
                    Some(value),
                    "workspace path contains traversal or an absolute component",
                ));
            }
        }
    }
    Ok(normalized)
}

fn workspace_relative_path(root: &Path, path: &Path) -> Result<String, PedelecError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        PedelecError::new(
            error_codes::WORKSPACE_PATH_INVALID,
            "workspace entry escaped the authoritative workspace root",
        )
    })?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(PedelecError::new(
                error_codes::WORKSPACE_PATH_INVALID,
                "workspace entry contained a malformed relative path",
            ));
        };
        components.push(part.to_string_lossy().into_owned());
    }
    Ok(components.join("/"))
}

fn is_link_or_junction(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn workspace_path_error(
    workspace_id: &str,
    requested_path: Option<&str>,
    message: &'static str,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_PATH_INVALID,
        message,
        serde_json::json!({
            "workspaceId": workspace_id,
            "path": requested_path,
        }),
    )
}

fn workspace_path_io_error(
    workspace_id: &str,
    requested_path: Option<&str>,
    message: &'static str,
    error: io::Error,
) -> PedelecError {
    let code = if error.kind() == io::ErrorKind::PermissionDenied {
        error_codes::WORKSPACE_ACCESS_DENIED
    } else {
        error_codes::WORKSPACE_PATH_INVALID
    };
    PedelecError::with_details(
        code,
        message,
        serde_json::json!({
            "workspaceId": workspace_id,
            "path": requested_path,
            "error": error.to_string(),
        }),
    )
}

fn workspace_list_io_error(
    workspace_id: &str,
    requested_path: Option<&str>,
    message: &'static str,
    error: io::Error,
) -> PedelecError {
    let code = if error.kind() == io::ErrorKind::PermissionDenied {
        error_codes::WORKSPACE_ACCESS_DENIED
    } else {
        error_codes::WORKSPACE_OPEN_FAILED
    };
    PedelecError::with_details(
        code,
        message,
        serde_json::json!({
            "workspaceId": workspace_id,
            "path": requested_path,
            "error": error.to_string(),
        }),
    )
}

fn workspace_list_too_large_error(
    workspace_id: &str,
    requested_path: Option<&str>,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_LIST_TOO_LARGE,
        "workspace listing is too large; list a narrower path",
        serde_json::json!({
            "workspaceId": workspace_id,
            "path": requested_path,
        }),
    )
}

fn workspace_busy_error(workspace_id: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_BUSY,
        "workspace has an active provider or Workspace run",
        serde_json::json!({ "workspaceId": workspace_id }),
    )
}

fn invalid_sdk_origin_error() -> PedelecError {
    PedelecError::new(error_codes::THREAD_ACCESS_DENIED, "invalid caller origin")
}

fn collect_assets(
    root: &Path,
    directory: &Path,
    assets: &mut Vec<Asset>,
) -> Result<(), PedelecError> {
    let relative_directory = asset_relative_path(root, directory)?;
    let entries = fs::read_dir(directory).map_err(|err| {
        PedelecError::with_details(
            error_codes::ASSET_LIST_FAILED,
            "failed to read workspace assets",
            serde_json::json!({ "path": relative_directory, "error": err.to_string() }),
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to read asset",
                serde_json::json!({ "path": relative_directory, "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        let public_path = asset_relative_path(root, &path)?;
        let file_type = entry.file_type().map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to inspect asset",
                serde_json::json!({ "path": public_path, "error": err.to_string() }),
            )
        })?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().into_string().map_err(|_| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "asset filename cannot be encoded",
                serde_json::json!({ "path": public_path }),
            )
        })?;
        if name.starts_with(".pedelec-") {
            continue;
        }
        if file_type.is_dir() {
            collect_assets(root, &path, assets)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to read asset metadata",
                serde_json::json!({ "path": public_path, "error": err.to_string() }),
            )
        })?;
        let modified_at = metadata
            .modified()
            .map_err(|err| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "failed to read asset modified time",
                    serde_json::json!({ "path": public_path, "error": err.to_string() }),
                )
            })?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "asset modified time predates Unix epoch",
                    serde_json::json!({ "path": public_path }),
                )
            })?
            .as_millis();
        let modified_at = i64::try_from(modified_at).map_err(|_| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "asset modified time is out of range",
                serde_json::json!({ "path": public_path }),
            )
        })?;
        assets.push(Asset {
            name,
            path: public_path,
            size_bytes: metadata.len(),
            modified_at,
        });
    }
    Ok(())
}

fn asset_relative_path(root: &Path, path: &Path) -> Result<String, PedelecError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        PedelecError::new(
            error_codes::ASSET_LIST_FAILED,
            "asset path is outside the asset root",
        )
    })?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => components.push(part.to_str().ok_or_else(|| {
                PedelecError::new(
                    error_codes::ASSET_LIST_FAILED,
                    "asset path cannot be encoded",
                )
            })?),
            _ => {
                return Err(PedelecError::new(
                    error_codes::ASSET_LIST_FAILED,
                    "asset path is invalid",
                ))
            }
        }
    }
    Ok(if components.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", components.join("/"))
    })
}

fn parse_public_asset_path(public_path: &str) -> Result<(String, PathBuf), ()> {
    if !public_path.starts_with('/')
        || public_path.len() == 1
        || public_path.starts_with("//")
        || public_path.contains('\\')
        || public_path.chars().any(char::is_control)
    {
        return Err(());
    }
    let relative = &public_path[1..];
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(());
    }
    Ok((public_path.to_string(), relative.split('/').collect()))
}

fn resolve_asset_file(
    thread: &ThreadState,
    workspace_path: &Path,
    public_path: &str,
) -> Result<(PathBuf, String, u64, i64), PedelecError> {
    let (_, relative_path) = parse_public_asset_path(public_path).map_err(|_| {
        PedelecError::with_details(
            error_codes::ASSET_PATH_INVALID,
            "asset path is invalid",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        )
    })?;
    let root = workspace_assets_root(workspace_path);
    let target = root.join(relative_path);
    let metadata = fs::symlink_metadata(&target).map_err(|_| {
        PedelecError::with_details(
            error_codes::ASSET_NOT_FOUND,
            "asset was not found",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PedelecError::with_details(
            error_codes::ASSET_NOT_FILE,
            "asset is not a regular file",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        ));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| PedelecError::new(error_codes::ASSET_NOT_FOUND, "asset root was not found"))?;
    let canonical_target = target.canonicalize().map_err(|_| {
        PedelecError::with_details(
            error_codes::ASSET_NOT_FOUND,
            "asset was not found",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        )
    })?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(PedelecError::with_details(
            error_codes::ASSET_PATH_INVALID,
            "asset path escapes the asset root",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        ));
    }
    if metadata.len() > MAX_ASSET_UPLOAD_BYTES {
        return Err(PedelecError::with_details(
            error_codes::ASSET_READ_TOO_LARGE,
            "asset exceeds the 100 MiB limit",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        ));
    }
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0);
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            PedelecError::new(error_codes::ASSET_PATH_INVALID, "asset filename is invalid")
        })?
        .to_string();
    Ok((canonical_target, name, metadata.len(), modified_at))
}

fn asset_mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|part| part.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" | "md" | "csv" => "text/plain",
        "json" => "application/json",
        "html" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "glb" => "model/gltf-binary",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub mod error_codes {
    pub const CORE_RUNTIME_UNAVAILABLE: &str = "CORE_RUNTIME_UNAVAILABLE";
    pub const THREAD_NOT_FOUND: &str = "THREAD_NOT_FOUND";
    pub const THREAD_ACCESS_DENIED: &str = "THREAD_ACCESS_DENIED";
    pub const THREAD_BUSY: &str = "THREAD_BUSY";
    pub const THREAD_ENDED: &str = "THREAD_ENDED";
    pub const DENO_ARGS_INVALID: &str = "DENO_ARGS_INVALID";
    pub const DENO_ENTRYPOINT_INVALID: &str = "DENO_ENTRYPOINT_INVALID";
    pub const DENO_IMPORT_MAP_INVALID: &str = "DENO_IMPORT_MAP_INVALID";
    pub const DENO_THREAD_NOT_ACTIVE: &str = "DENO_THREAD_NOT_ACTIVE";
    pub const DENO_RUNTIME_UNAVAILABLE: &str = "DENO_RUNTIME_UNAVAILABLE";
    pub const DENO_EXECUTION_BUSY: &str = "DENO_EXECUTION_BUSY";
    pub const DENO_EXECUTION_TIMEOUT: &str = "DENO_EXECUTION_TIMEOUT";
    pub const DENO_EXECUTION_CANCELLED: &str = "DENO_EXECUTION_CANCELLED";
    pub const DENO_PROCESS_SPAWN_FAILED: &str = "DENO_PROCESS_SPAWN_FAILED";
    pub const DENO_PROCESS_IO_FAILED: &str = "DENO_PROCESS_IO_FAILED";
    pub const PROVIDER_NOT_FOUND: &str = "PROVIDER_NOT_FOUND";
    pub const PROVIDER_UNSUPPORTED: &str = "PROVIDER_UNSUPPORTED";
    pub const PROVIDER_PROMPT_TOO_LARGE: &str = "PROVIDER_PROMPT_TOO_LARGE";
    pub const PROVIDER_PROCESS_START_FAILED: &str = "PROVIDER_PROCESS_START_FAILED";
    pub const PROVIDER_PROCESS_STOP_FAILED: &str = "PROVIDER_PROCESS_STOP_FAILED";
    pub const PROVIDER_STDIN_CLOSED: &str = "PROVIDER_STDIN_CLOSED";
    pub const PROVIDER_COMMAND_FAILED: &str = "PROVIDER_COMMAND_FAILED";
    pub const PROVIDER_RUNTIME_START_FAILED: &str = "PROVIDER_RUNTIME_START_FAILED";
    pub const PROVIDER_RUNTIME_DISCONNECTED: &str = "PROVIDER_RUNTIME_DISCONNECTED";
    pub const PROVIDER_PROTOCOL_ERROR: &str = "PROVIDER_PROTOCOL_ERROR";
    pub const PROVIDER_REQUEST_FAILED: &str = "PROVIDER_REQUEST_FAILED";
    pub const PROVIDER_INSTALL_UNSUPPORTED: &str = "PROVIDER_INSTALL_UNSUPPORTED";
    pub const PROVIDER_INSTALLER_LAUNCH_FAILED: &str = "PROVIDER_INSTALLER_LAUNCH_FAILED";
    pub const PROVIDER_TERMINAL_UNSUPPORTED: &str = "PROVIDER_TERMINAL_UNSUPPORTED";
    pub const PROVIDER_TERMINAL_UNAVAILABLE: &str = "PROVIDER_TERMINAL_UNAVAILABLE";
    pub const PROVIDER_SCAN_FAILED: &str = "PROVIDER_SCAN_FAILED";
    pub const PROVIDER_TERMINAL_WORKDIR_FAILED: &str = "PROVIDER_TERMINAL_WORKDIR_FAILED";
    pub const PROVIDER_TERMINAL_LAUNCH_FAILED: &str = "PROVIDER_TERMINAL_LAUNCH_FAILED";
    pub const PROVIDER_PREPARE_UNSUPPORTED: &str = "PROVIDER_PREPARE_UNSUPPORTED";
    pub const PREPARE_SESSION_ID_MISSING: &str = "PREPARE_SESSION_ID_MISSING";
    pub const PREPARE_ACK_INVALID: &str = "PREPARE_ACK_INVALID";
    pub const PROVIDER_BOOTSTRAP_CONFIG_INVALID: &str = "PROVIDER_BOOTSTRAP_CONFIG_INVALID";
    pub const PROVIDER_BOOTSTRAP_ASSET_FAILED: &str = "PROVIDER_BOOTSTRAP_ASSET_FAILED";
    pub const SKILL_URL_INVALID: &str = "SKILL_URL_INVALID";
    pub const SKILL_DOWNLOAD_FAILED: &str = "SKILL_DOWNLOAD_FAILED";
    pub const WORKSPACE_CREATE_FAILED: &str = "WORKSPACE_CREATE_FAILED";
    pub const WORKSPACE_REMOVE_FAILED: &str = "WORKSPACE_REMOVE_FAILED";
    pub const WORKSPACE_PATH_INVALID: &str = "WORKSPACE_PATH_INVALID";
    pub const WORKSPACE_OPEN_FAILED: &str = "WORKSPACE_OPEN_FAILED";
    pub const WORKSPACE_NOT_FOUND: &str = "WORKSPACE_NOT_FOUND";
    pub const WORKSPACE_ACCESS_DENIED: &str = "WORKSPACE_ACCESS_DENIED";
    pub const WORKSPACE_BUSY: &str = "WORKSPACE_BUSY";
    pub const WORKSPACE_LIST_TOO_LARGE: &str = "WORKSPACE_LIST_TOO_LARGE";
    pub const WORKSPACE_RUN_OUTPUT_TOO_LARGE: &str = "WORKSPACE_RUN_OUTPUT_TOO_LARGE";
    pub const DIRECTORY_PICKER_FAILED: &str = "DIRECTORY_PICKER_FAILED";
    pub const TOOLS_JSON_NOT_FOUND: &str = "TOOLS_JSON_NOT_FOUND";
    pub const TOOLS_JSON_INVALID: &str = "TOOLS_JSON_INVALID";
    pub const TOOLS_MANIFEST_INVALID: &str = "TOOLS_MANIFEST_INVALID";
    pub const TOOLS_MD_NOT_FOUND: &str = "TOOLS_MD_NOT_FOUND";
    pub const TOOL_NOT_FOUND: &str = "TOOL_NOT_FOUND";
    pub const TOOL_ARGS_INVALID: &str = "TOOL_ARGS_INVALID";
    pub const TOOL_TIMEOUT: &str = "TOOL_TIMEOUT";
    pub const PENDING_TOOL_REQUEST_EXISTS: &str = "PENDING_TOOL_REQUEST_EXISTS";
    pub const PENDING_TOOL_REQUEST_NOT_FOUND: &str = "PENDING_TOOL_REQUEST_NOT_FOUND";
    pub const IPC_UNAVAILABLE: &str = "IPC_UNAVAILABLE";
    pub const IPC_UNAUTHORIZED: &str = "IPC_UNAUTHORIZED";
    pub const MESSAGE_TOO_LARGE: &str = "MESSAGE_TOO_LARGE";
    pub const NATIVE_CONNECTION_CLOSED: &str = "NATIVE_CONNECTION_CLOSED";
    pub const DEFAULT_PROVIDER_NOT_SET: &str = "DEFAULT_PROVIDER_NOT_SET";
    pub const DEFAULT_PROVIDER_UNAVAILABLE: &str = "DEFAULT_PROVIDER_UNAVAILABLE";
    pub const MODEL_REQUIRED: &str = "MODEL_REQUIRED";
    pub const SETTINGS_READ_FAILED: &str = "SETTINGS_READ_FAILED";
    pub const SETTINGS_WRITE_FAILED: &str = "SETTINGS_WRITE_FAILED";
    pub const EFFORT_WIZARD_PRESET_INVALID: &str = "EFFORT_WIZARD_PRESET_INVALID";
    pub const EFFORT_WIZARD_SETTINGS_CHANGED: &str = "EFFORT_WIZARD_SETTINGS_CHANGED";
    pub const EFFORT_WIZARD_APPLY_INVALID: &str = "EFFORT_WIZARD_APPLY_INVALID";
    pub const OLLAMA_API_KEY_REQUIRED: &str = "OLLAMA_API_KEY_REQUIRED";
    pub const OLLAMA_AUTH_FAILED: &str = "OLLAMA_AUTH_FAILED";
    pub const OLLAMA_MODEL_NOT_FOUND: &str = "OLLAMA_MODEL_NOT_FOUND";
    pub const OLLAMA_CLOUD_LIMIT_EXCEEDED: &str = "OLLAMA_CLOUD_LIMIT_EXCEEDED";
    pub const OLLAMA_BASE_URL_INVALID: &str = "OLLAMA_BASE_URL_INVALID";
    pub const OLLAMA_UNAVAILABLE: &str = "OLLAMA_UNAVAILABLE";
    pub const OLLAMA_REQUEST_FAILED: &str = "OLLAMA_REQUEST_FAILED";
    pub const OLLAMA_RESPONSE_INVALID: &str = "OLLAMA_RESPONSE_INVALID";
    pub const INVALID_INPUT: &str = "INVALID_INPUT";
    pub const ASSET_TOO_LARGE: &str = "ASSET_TOO_LARGE";
    pub const ASSET_UPLOAD_SERVER_UNAVAILABLE: &str = "ASSET_UPLOAD_SERVER_UNAVAILABLE";
    pub const ASSET_UPLOAD_TICKET_EXPIRED: &str = "ASSET_UPLOAD_TICKET_EXPIRED";
    pub const ASSET_UPLOAD_UNAUTHORIZED: &str = "ASSET_UPLOAD_UNAUTHORIZED";
    pub const ASSET_UPLOAD_SIZE_MISMATCH: &str = "ASSET_UPLOAD_SIZE_MISMATCH";
    pub const ASSET_UPLOAD_FAILED: &str = "ASSET_UPLOAD_FAILED";
    pub const ASSET_LIST_FAILED: &str = "ASSET_LIST_FAILED";
    pub const ASSET_PATH_INVALID: &str = "ASSET_PATH_INVALID";
    pub const ASSET_NOT_FOUND: &str = "ASSET_NOT_FOUND";
    pub const ASSET_NOT_FILE: &str = "ASSET_NOT_FILE";
    pub const ASSET_READ_TOO_LARGE: &str = "ASSET_READ_TOO_LARGE";
    pub const ASSET_READ_FAILED: &str = "ASSET_READ_FAILED";
    pub const ASSET_DOWNLOAD_TICKET_EXPIRED: &str = "ASSET_DOWNLOAD_TICKET_EXPIRED";
    pub const ASSET_DOWNLOAD_UNAUTHORIZED: &str = "ASSET_DOWNLOAD_UNAUTHORIZED";
    pub const DENO_MODULE_NAME_INVALID: &str = "DENO_MODULE_NAME_INVALID";
    pub const DENO_MODULE_SETUP_INCOMPLETE: &str = "DENO_MODULE_SETUP_INCOMPLETE";
    pub const DENO_MODULE_NOT_FOUND: &str = "DENO_MODULE_NOT_FOUND";
    pub const DENO_MODULE_ALREADY_READY: &str = "DENO_MODULE_ALREADY_READY";
    pub const DENO_MODULE_ARTIFACT_TOO_LARGE: &str = "DENO_MODULE_ARTIFACT_TOO_LARGE";
    pub const DENO_MODULE_ARTIFACT_INVALID: &str = "DENO_MODULE_ARTIFACT_INVALID";
    pub const DENO_MODULE_MATERIALIZATION_FAILED: &str = "DENO_MODULE_MATERIALIZATION_FAILED";
    pub const DENO_MODULE_UPLOAD_SERVER_UNAVAILABLE: &str = "DENO_MODULE_UPLOAD_SERVER_UNAVAILABLE";
    pub const DENO_MODULE_UPLOAD_TICKET_EXPIRED: &str = "DENO_MODULE_UPLOAD_TICKET_EXPIRED";
    pub const DENO_MODULE_UPLOAD_UNAUTHORIZED: &str = "DENO_MODULE_UPLOAD_UNAUTHORIZED";
    pub const DENO_MODULE_UPLOAD_SIZE_MISMATCH: &str = "DENO_MODULE_UPLOAD_SIZE_MISMATCH";
    pub const DENO_MODULE_UPLOAD_FAILED: &str = "DENO_MODULE_UPLOAD_FAILED";
    pub const DENO_MODULE_SETUP_FAILED: &str = "DENO_MODULE_SETUP_FAILED";
}

fn provider_code_as_str(provider: &ProviderCode) -> &'static str {
    match provider {
        ProviderCode::Codex => "codex",
        ProviderCode::Antigravity => "antigravity",
        ProviderCode::OpenCode => "opencode",
        ProviderCode::Cursor => "cursor",
        ProviderCode::Claude => "claude",
        ProviderCode::Ollama => "ollama",
    }
}

fn resolve_operation_id(operation_id: Option<String>) -> Result<String, PedelecError> {
    if let Some(operation_id) = operation_id {
        if operation_id.trim().is_empty() {
            return Err(PedelecError::new(
                error_codes::INVALID_INPUT,
                "operationId must not be empty",
            ));
        }
        return Ok(operation_id);
    }

    Ok(format!(
        "operation_{}_{}",
        Utc::now().timestamp_millis(),
        Uuid::new_v4().simple()
    ))
}

fn new_provider_turn_id() -> String {
    format!("local_{}", Uuid::new_v4().simple())
}

fn runtime_protocol_error(thread_id: &str, message: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_PROTOCOL_ERROR,
        message,
        serde_json::json!({ "threadId": thread_id }),
    )
}

fn provider_model_from_effort_args(provider: &ProviderCode, args: &[String]) -> Option<String> {
    let model_flag = provider_model_arg_key(provider);
    args.windows(2)
        .find(|pair| pair[0] == model_flag)
        .map(|pair| pair[1].clone())
        .filter(|model| !model.trim().is_empty())
}

fn provider_model_arg_key(provider: &ProviderCode) -> &'static str {
    if *provider == ProviderCode::Codex {
        "-m"
    } else {
        "--model"
    }
}

fn parse_codex_session_settings(
    args: &[String],
    thread_id: &str,
) -> Result<(Option<String>, Option<CodexReasoningEffort>), PedelecError> {
    validate_effort_tier(&ProviderCode::Codex, EffortLevel::Default, args).map_err(|error| {
        PedelecError::with_details(
            error_codes::INVALID_INPUT,
            "Codex settings could not be mapped to typed App Server fields",
            serde_json::json!({
                "threadId": thread_id,
                "provider": "codex",
                "error": error,
                "source": "effort_args",
            }),
        )
    })?;

    let mut model = None;
    let mut effort = None;
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "-m" => {
                if model.replace(pair[1].trim().to_string()).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Codex model setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "codex" }),
                    ));
                }
            }
            "-c" => {
                let raw = parse_codex_reasoning_effort(&pair[1]).ok_or_else(|| {
                    PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Codex reasoning effort setting is invalid",
                        serde_json::json!({
                            "threadId": thread_id,
                            "provider": "codex",
                            "setting": pair[1],
                        }),
                    )
                })?;
                let parsed = match raw {
                    "low" => CodexReasoningEffort::Low,
                    "medium" => CodexReasoningEffort::Medium,
                    "high" => CodexReasoningEffort::High,
                    "xhigh" => CodexReasoningEffort::XHigh,
                    "max" => CodexReasoningEffort::Max,
                    _ => unreachable!("validate_effort_tier accepted an unknown Codex effort"),
                };
                if effort.replace(parsed).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Codex reasoning effort setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "codex" }),
                    ));
                }
            }
            _ => unreachable!("validate_effort_tier accepted an unknown Codex setting"),
        }
    }
    Ok((model, effort))
}

fn parse_antigravity_session_settings(
    args: &[String],
    thread_id: &str,
) -> Result<(Option<String>, Option<AntigravityReasoningEffort>), PedelecError> {
    validate_effort_tier(&ProviderCode::Antigravity, EffortLevel::Default, args).map_err(
        |error| {
            PedelecError::with_details(
                error_codes::INVALID_INPUT,
                "Antigravity settings could not be mapped to typed stream runtime fields",
                serde_json::json!({
                    "threadId": thread_id,
                    "provider": "antigravity",
                    "error": error,
                    "source": "effort_args",
                }),
            )
        },
    )?;

    let mut model = None;
    let mut effort = None;
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "--model" => {
                if model.replace(pair[1].trim().to_string()).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Antigravity model setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "antigravity" }),
                    ));
                }
            }
            "--effort" => {
                let parsed = match pair[1].trim() {
                    "low" => AntigravityReasoningEffort::Low,
                    "medium" => AntigravityReasoningEffort::Medium,
                    "high" => AntigravityReasoningEffort::High,
                    _ => {
                        unreachable!("validate_effort_tier accepted an unknown Antigravity effort")
                    }
                };
                if effort.replace(parsed).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Antigravity reasoning effort setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "antigravity" }),
                    ));
                }
            }
            _ => unreachable!("validate_effort_tier accepted an unknown Antigravity setting"),
        }
    }
    Ok((model, effort))
}

fn parse_claude_session_settings(
    args: &[String],
    thread_id: &str,
) -> Result<(Option<String>, Option<ClaudeReasoningEffort>), PedelecError> {
    validate_effort_tier(&ProviderCode::Claude, EffortLevel::Default, args).map_err(|error| {
        PedelecError::with_details(
            error_codes::INVALID_INPUT,
            "Claude settings could not be mapped to typed stream runtime fields",
            serde_json::json!({
                "threadId": thread_id,
                "provider": "claude",
                "error": error,
                "source": "effort_args",
            }),
        )
    })?;

    let mut model = None;
    let mut effort = None;
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "--model" => {
                if model.replace(pair[1].trim().to_string()).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Claude model setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "claude" }),
                    ));
                }
            }
            "--effort" => {
                let parsed = match pair[1].trim() {
                    "low" => ClaudeReasoningEffort::Low,
                    "medium" => ClaudeReasoningEffort::Medium,
                    "high" => ClaudeReasoningEffort::High,
                    "xhigh" => ClaudeReasoningEffort::XHigh,
                    "max" => ClaudeReasoningEffort::Max,
                    _ => unreachable!("validate_effort_tier accepted an unknown Claude effort"),
                };
                if effort.replace(parsed).is_some() {
                    return Err(PedelecError::with_details(
                        error_codes::INVALID_INPUT,
                        "Claude reasoning effort setting is duplicated",
                        serde_json::json!({ "threadId": thread_id, "provider": "claude" }),
                    ));
                }
            }
            _ => unreachable!("validate_effort_tier accepted an unknown Claude setting"),
        }
    }
    Ok((model, effort))
}

fn provider_display_name(provider: &ProviderCode) -> &'static str {
    match provider {
        ProviderCode::Codex => "Codex",
        ProviderCode::Antigravity => "Antigravity",
        ProviderCode::OpenCode => "OpenCode",
        ProviderCode::Cursor => "Cursor",
        ProviderCode::Claude => "Claude Code",
        ProviderCode::Ollama => "Ollama",
    }
}

fn provider_program_name(provider: &ProviderCode) -> &'static str {
    match provider {
        ProviderCode::Codex => "codex",
        ProviderCode::Antigravity => "agy",
        ProviderCode::OpenCode => "opencode",
        ProviderCode::Cursor => "cursor-agent",
        ProviderCode::Claude => "claude",
        ProviderCode::Ollama => "pedelec-agent",
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderCli {
    path: Option<PathBuf>,
    version: Option<ProviderVersion>,
    error: Option<String>,
    /// `Some(false)` means the executable was found and versioned, but it
    /// cannot satisfy Pedelec's App Server-only Codex execution contract.
    /// `None` is retained for synthetic/test snapshots created before the
    /// capability probe existed.
    app_server_capability: Option<bool>,
    /// `Some(false)` means the provider lacks the required persistent ACP entrypoint.
    acp_capability: Option<bool>,
    /// `Some(false)` means a persistent stream-json provider was found and
    /// versioned but does not expose the bidirectional stream-json transport
    /// required by Pedelec. Used by Antigravity and Claude.
    stream_json_capability: Option<bool>,
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct ProviderVersion(Vec<u64>);

fn provider_version_display(version: &ProviderVersion) -> String {
    version
        .0
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn external_provider_codes() -> [ProviderCode; 5] {
    [
        ProviderCode::Codex,
        ProviderCode::Antigravity,
        ProviderCode::OpenCode,
        ProviderCode::Cursor,
        ProviderCode::Claude,
    ]
}

/// GUI applications do not reliably inherit shell profile PATH updates. The
/// process PATH and platform-specific installer locations are merged without
/// mutating the user's shell configuration or process-wide environment.
fn merged_provider_path(current: Option<OsString>) -> OsString {
    merge_provider_paths(current.as_ref(), None, provider_fallback_paths())
}

fn resolve_provider_path_value() -> OsString {
    let process_path = env::var_os("PATH");
    #[cfg(target_os = "macos")]
    let login_shell_path = resolve_macos_login_shell_path();
    #[cfg(not(target_os = "macos"))]
    let login_shell_path = None;

    merge_provider_paths(
        process_path.as_ref(),
        login_shell_path.as_ref(),
        provider_fallback_paths(),
    )
}

fn provider_fallback_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    if let Ok(hkcu) =
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).open_subkey("Environment")
    {
        if let Ok(value) = hkcu.get_value::<OsString, _>("Path") {
            paths.extend(env::split_paths(&value));
        }
    }

    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".pedelec"));
        #[cfg(windows)]
        paths.extend([
            home.join("AppData/Local/Programs/OpenAI/Codex/bin"),
            home.join("AppData/Local/agy/bin"),
            home.join(".opencode/bin"),
            home.join(".local/bin"),
            home.join("AppData/Roaming/npm"),
        ]);
        #[cfg(not(windows))]
        paths.extend([home.join(".local/bin"), home.join(".opencode/bin")]);
        #[cfg(target_os = "macos")]
        paths.extend([
            home.join(".volta/bin"),
            home.join(".asdf/shims"),
            home.join(".local/share/mise/shims"),
            home.join("Library/pnpm"),
            home.join(".bun/bin"),
        ]);
    }
    #[cfg(target_os = "macos")]
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
    ]);
    paths
}

fn merge_provider_paths(
    process_path: Option<&OsString>,
    login_shell_path: Option<&OsString>,
    fallback_paths: Vec<PathBuf>,
) -> OsString {
    let mut paths = process_path.map_or_else(Vec::new, |value| env::split_paths(value).collect());
    if let Some(login_shell_path) = login_shell_path {
        paths.extend(env::split_paths(login_shell_path));
    }
    paths.extend(fallback_paths);

    let mut normalized = Vec::new();
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        append_unique_provider_path(&mut normalized, path);
    }
    env::join_paths(normalized).unwrap_or_default()
}

fn append_unique_provider_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let duplicate = paths.iter().any(|existing| {
        #[cfg(windows)]
        {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        }
        #[cfg(not(windows))]
        {
            existing == &path
        }
    });
    if !duplicate {
        paths.push(path);
    }
}

#[cfg(any(target_os = "macos", test))]
const MACOS_PATH_MARKER_COMMAND: &str =
    "printf '__PEDELEC_PATH_START__%s__PEDELEC_PATH_END__\\n' \"$PATH\"";
#[cfg(any(target_os = "macos", test))]
const MACOS_PATH_MARKER_START: &str = "__PEDELEC_PATH_START__";
#[cfg(any(target_os = "macos", test))]
const MACOS_PATH_MARKER_END: &str = "__PEDELEC_PATH_END__";
#[cfg(target_os = "macos")]
const MACOS_LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(any(target_os = "macos", test))]
fn macos_shell_probe_arguments(shell_path: &Path) -> Vec<Vec<&'static str>> {
    match shell_path.file_name().and_then(OsStr::to_str) {
        Some("zsh") => vec![vec!["-l", "-i", "-c", MACOS_PATH_MARKER_COMMAND]],
        Some("bash") => vec![
            vec!["-l", "-c", MACOS_PATH_MARKER_COMMAND],
            vec!["-i", "-c", MACOS_PATH_MARKER_COMMAND],
        ],
        _ => vec![vec!["-l", "-c", MACOS_PATH_MARKER_COMMAND]],
    }
}

#[cfg(target_os = "macos")]
fn resolve_macos_login_shell_path() -> Option<OsString> {
    let shell_path = env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && is_provider_executable(path))
        .or_else(|| {
            [PathBuf::from("/bin/zsh"), PathBuf::from("/bin/bash")]
                .into_iter()
                .find(|path| is_provider_executable(path))
        })?;

    resolve_macos_shell_path_with_strategies(
        &shell_path,
        Instant::now() + MACOS_LOGIN_SHELL_TIMEOUT,
    )
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn resolve_macos_shell_path_with_strategies(
    shell_path: &Path,
    deadline: Instant,
) -> Option<OsString> {
    let mut paths = Vec::new();
    for args in macos_shell_probe_arguments(shell_path) {
        if Instant::now() >= deadline {
            break;
        }
        if let Some(path) = probe_shell_path(shell_path, &args, deadline) {
            paths.push(path);
        }
    }
    merge_shell_probe_paths(&paths)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn probe_shell_path(shell_path: &Path, args: &[&str], deadline: Instant) -> Option<OsString> {
    if Instant::now() >= deadline {
        log_macos_login_shell_failure(shell_path, "timeout");
        return None;
    }

    let mut command = Command::new(shell_path);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        log_macos_login_shell_failure(shell_path, "spawn");
        return None;
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        log_macos_login_shell_failure(shell_path, "stdout");
        return None;
    };
    let (output_sender, output_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take(u64::MAX)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = output_sender.send(result);
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    log_macos_login_shell_failure(shell_path, "timeout");
                    break None;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                log_macos_login_shell_failure(shell_path, "wait");
                break None;
            }
        }
    }?;

    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = match output_receiver.recv_timeout(remaining) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            log_macos_login_shell_failure(shell_path, "read");
            return None;
        }
        Err(_) => {
            log_macos_login_shell_failure(shell_path, "read-timeout");
            return None;
        }
    };
    if !status.success() {
        log_macos_login_shell_failure(shell_path, "exit");
        return None;
    }
    let path = parse_login_shell_path_output(&output);
    if path.is_none() {
        log_macos_login_shell_failure(shell_path, "parse");
    }
    path
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn merge_shell_probe_paths(paths: &[OsString]) -> Option<OsString> {
    let mut merged = Vec::new();
    for value in paths {
        for path in env::split_paths(value) {
            if !path.as_os_str().is_empty() {
                append_unique_provider_path(&mut merged, path);
            }
        }
    }
    if merged.is_empty() {
        None
    } else {
        env::join_paths(merged).ok()
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn log_macos_login_shell_failure(shell_path: &Path, category: &str) {
    #[cfg(debug_assertions)]
    eprintln!(
        "macOS login shell PATH lookup failed: shell={} failure={}",
        shell_path.display(),
        category
    );
}

#[cfg(any(target_os = "macos", test))]
fn parse_login_shell_path_output(output: &[u8]) -> Option<OsString> {
    let start = output
        .windows(MACOS_PATH_MARKER_START.len())
        .position(|window| window == MACOS_PATH_MARKER_START.as_bytes())?
        + MACOS_PATH_MARKER_START.len();
    let end = output[start..]
        .windows(MACOS_PATH_MARKER_END.len())
        .position(|window| window == MACOS_PATH_MARKER_END.as_bytes())?
        + start;
    let path = std::str::from_utf8(&output[start..end]).ok()?.trim();
    if path.is_empty()
        || !env::split_paths(OsString::from(path).as_os_str())
            .any(|entry| !entry.as_os_str().is_empty())
    {
        return None;
    }
    Some(OsString::from(path))
}

fn scan_external_providers(path_value: Option<OsString>) -> HashMap<ProviderCode, ProviderCli> {
    external_provider_codes()
        .into_iter()
        .map(|provider| {
            let program = provider_program_name(&provider);
            (provider, scan_provider_cli(program, path_value.as_ref()))
        })
        .collect()
}

fn scan_provider_cli(program: &str, path_value: Option<&OsString>) -> ProviderCli {
    let Some(path_value) = path_value else {
        return ProviderCli {
            error: Some("PATH was not available".to_string()),
            ..Default::default()
        };
    };
    let path_dirs = env::split_paths(path_value).collect::<Vec<_>>();
    let mut candidates = provider_binary_lookup_candidates(program, &path_dirs);
    candidates.sort();
    candidates.dedup();
    let executable_candidates = candidates
        .into_iter()
        .filter(|path| is_provider_executable(path))
        .collect::<Vec<_>>();
    let has_executable = !executable_candidates.is_empty();
    let recognized = executable_candidates
        .into_iter()
        .filter_map(|path| {
            provider_cli_version(&path, Some(path_value)).map(|version| (path, version))
        })
        .max_by(|(left_path, left), (right_path, right)| {
            left.cmp(right).then_with(|| left_path.cmp(right_path))
        });
    match recognized {
        Some((path, version)) => {
            let app_server_capability = if program == provider_program_name(&ProviderCode::Codex) {
                Some(probe_codex_app_server_capability(&path, Some(path_value)))
            } else {
                None
            };
            let acp_capability = if program == provider_program_name(&ProviderCode::OpenCode)
                || program == provider_program_name(&ProviderCode::Cursor)
            {
                Some(probe_acp_capability(&path, Some(path_value)))
            } else {
                None
            };
            let stream_json_capability =
                if program == provider_program_name(&ProviderCode::Antigravity) {
                    Some(probe_antigravity_stream_json_capability(
                        &path,
                        Some(path_value),
                    ))
                } else if program == provider_program_name(&ProviderCode::Claude) {
                    Some(probe_claude_persistent_stream_capability(
                        &path,
                        Some(path_value),
                    ))
                } else {
                    None
                };
            let mut error = match (
                app_server_capability,
                acp_capability,
                stream_json_capability,
            ) {
                (Some(false), _, _) => Some(
                    "Codex executable does not expose the required `app-server` capability"
                        .to_string(),
                ),
                (_, Some(false), _) => Some(format!(
                    "{program} executable does not expose the required `acp` capability"
                )),
                (_, _, Some(false)) if program == provider_program_name(&ProviderCode::Claude) => {
                    Some(
                        "claude executable does not expose the required persistent `stream-json` capability"
                            .to_string(),
                    )
                }
                (_, _, Some(false)) => Some(
                    "agy executable does not expose the required bidirectional `stream-json` capability"
                        .to_string(),
                ),
                _ => None,
            };
            if error.is_none()
                && program == provider_program_name(&ProviderCode::Antigravity)
                && !antigravity_custom_agent_version_supported(&version)
            {
                error = Some(
                    "agy executable version does not support the required workspace custom agent capability"
                        .to_string(),
                );
            }
            ProviderCli {
                path: Some(path),
                version: Some(version),
                error,
                app_server_capability,
                acp_capability,
                stream_json_capability,
            }
        }
        None => ProviderCli {
            path: None,
            version: None,
            error: Some(if has_executable {
                format!("{program} executable version was unrecognized")
            } else {
                format!("{program} executable was not found in PATH")
            }),
            app_server_capability: None,
            acp_capability: None,
            stream_json_capability: None,
        },
    }
}

fn probe_codex_app_server_capability(path: &Path, path_value: Option<&OsString>) -> bool {
    let output = run_bounded_provider_probe({
        let mut command = provider_version_command(path, path_value);
        command.arg("app-server").arg("--help");
        command
    });
    let Some(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    text.contains("app-server")
}

fn probe_acp_capability(path: &Path, path_value: Option<&OsString>) -> bool {
    let output = run_bounded_provider_probe({
        let mut command = provider_version_command(path, path_value);
        command.arg("acp").arg("--help");
        command
    });
    let Some(output) = output else {
        return false;
    };
    output.status.success()
        && format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase()
        .contains("acp")
}

fn probe_antigravity_stream_json_capability(path: &Path, path_value: Option<&OsString>) -> bool {
    let output = run_bounded_provider_probe({
        let mut command = provider_version_command(path, path_value);
        command.arg("--help");
        command
    });
    let Some(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    text.contains("--input-format")
        && text.contains("--output-format")
        && text.contains("stream-json")
}

fn probe_claude_persistent_stream_capability(path: &Path, path_value: Option<&OsString>) -> bool {
    let output = run_bounded_provider_probe({
        let mut command = provider_version_command(path, path_value);
        command.arg("--help");
        command
    });
    let Some(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    text.contains("--input-format")
        && text.contains("--output-format")
        && text.contains("stream-json")
        && text.contains("--include-partial-messages")
        && text.contains("--append-system-prompt")
}

fn is_provider_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn provider_cli_version(path: &Path, path_value: Option<&OsString>) -> Option<ProviderVersion> {
    let output = run_bounded_provider_probe({
        let mut command = provider_version_command(path, path_value);
        command.arg("--version");
        command
    });
    let output = output?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = parse_provider_version(&text);
    parsed
}

/// Run a provider discovery command with both a time and output bound. A
/// provider executable is user-controlled and may be a wrapper that starts a
/// long-lived process, so normal scans must never wait indefinitely or retain
/// an unbounded help/version response.
fn run_bounded_provider_probe(mut command: Command) -> Option<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout
            .take(PROVIDER_PROBE_MAX_OUTPUT_BYTES)
            .read_to_end(&mut output);
        let _ = stdout_sender.send(output);
    });
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stderr
            .take(PROVIDER_PROBE_MAX_OUTPUT_BYTES)
            .read_to_end(&mut output);
        let _ = stderr_sender.send(output);
    });

    let deadline = Instant::now() + PROVIDER_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    }?;

    let stdout = stdout_receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    let stderr = stderr_receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    Some(Output {
        status,
        stdout,
        stderr,
    })
}

fn provider_version_command(path: &Path, path_value: Option<&OsString>) -> Command {
    #[cfg(windows)]
    {
        let is_script_wrapper = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(extension.to_ascii_lowercase().as_str(), "cmd" | "bat")
            });
        let mut command = if is_script_wrapper {
            let mut command = Command::new("cmd.exe");
            command.arg("/d").arg("/c").arg("call").arg(path);
            command
        } else {
            Command::new(path)
        };
        if let Some(path_value) = path_value {
            command.env("PATH", path_value);
        }
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }

    #[cfg(not(windows))]
    {
        let mut command = Command::new(path);
        if let Some(path_value) = path_value {
            command.env("PATH", path_value);
        }
        command
    }
}

fn parse_provider_version(text: &str) -> Option<ProviderVersion> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() || (start > 0 && bytes[start - 1].is_ascii_digit()) {
            continue;
        }
        let mut end = start;
        let mut parts = Vec::new();
        loop {
            let segment_start = end;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if segment_start == end {
                break;
            }
            parts.push(
                std::str::from_utf8(&bytes[segment_start..end])
                    .ok()?
                    .parse()
                    .ok()?,
            );
            if end >= bytes.len() || bytes[end] != b'.' {
                break;
            }
            end += 1;
            if end >= bytes.len() || !bytes[end].is_ascii_digit() {
                break;
            }
        }
        if !parts.is_empty() {
            while parts.last() == Some(&0) && parts.len() > 1 {
                parts.pop();
            }
            return Some(ProviderVersion(parts));
        }
    }
    None
}

fn list_provider_infos_with_scan(
    provider_scan: &HashMap<ProviderCode, ProviderCli>,
    path_value: Option<OsString>,
) -> Vec<ProviderInfo> {
    [
        ProviderCode::Codex,
        ProviderCode::Antigravity,
        ProviderCode::OpenCode,
        ProviderCode::Cursor,
        ProviderCode::Claude,
        ProviderCode::Ollama,
    ]
    .into_iter()
    .map(|provider| provider_info_for(provider, provider_scan, path_value.as_ref()))
    .collect()
}

#[cfg(test)]
fn list_provider_infos(path_value: Option<OsString>) -> Vec<ProviderInfo> {
    let scan = scan_external_providers(path_value.clone());
    list_provider_infos_with_scan(&scan, path_value)
}

fn required_runtime_capability(provider: &ProviderCode) -> Option<&'static str> {
    match provider {
        ProviderCode::Codex => Some("app-server"),
        ProviderCode::OpenCode | ProviderCode::Cursor => Some("acp"),
        ProviderCode::Antigravity => Some("stream-json + workspace-custom-agent"),
        ProviderCode::Claude => Some("persistent stream-json"),
        ProviderCode::Ollama => None,
    }
}

fn antigravity_custom_agent_capability_available(scan: &ProviderCli) -> Option<bool> {
    scan.version
        .as_ref()
        .map(antigravity_custom_agent_version_supported)
}

fn runtime_capability_available(provider: &ProviderCode, scan: &ProviderCli) -> Option<bool> {
    match provider {
        ProviderCode::Codex => scan.app_server_capability,
        ProviderCode::OpenCode | ProviderCode::Cursor => scan.acp_capability,
        ProviderCode::Antigravity => {
            let stream_json = scan.stream_json_capability;
            let custom_agent = antigravity_custom_agent_capability_available(scan);
            match (stream_json, custom_agent) {
                (Some(stream_json), Some(custom_agent)) => Some(stream_json && custom_agent),
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), None) => Some(true),
                (None, Some(custom_agent)) => Some(custom_agent),
                (None, None) => None,
            }
        }
        ProviderCode::Claude => scan.stream_json_capability,
        ProviderCode::Ollama => None,
    }
}

fn required_runtime_capability_is_available(provider: &ProviderCode, scan: &ProviderCli) -> bool {
    // `None` is retained as a compatibility value for hand-built neutral
    // snapshots. Real scans always set the required capability to Some(true)
    // or Some(false), so production availability is capability-gated.
    runtime_capability_available(provider, scan) != Some(false)
}

fn provider_info_for(
    provider: ProviderCode,
    provider_scan: &HashMap<ProviderCode, ProviderCli>,
    path_value: Option<&OsString>,
) -> ProviderInfo {
    if provider != ProviderCode::Ollama {
        let scanned_complete = provider_scan.contains_key(&provider);
        let scanned = provider_scan
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| ProviderCli {
                error: Some("provider scan has not completed".to_string()),
                ..Default::default()
            });
        let available = scanned.version.is_some()
            && required_runtime_capability_is_available(&provider, &scanned);
        return ProviderInfo {
            name: provider_display_name(&provider).to_string(),
            code: provider,
            scanned: scanned_complete,
            version: scanned.version.as_ref().map(provider_version_display),
            path: scanned.path.map(|path| path.to_string_lossy().to_string()),
            available,
            error: scanned.error,
        };
    }
    let program = provider_program_name(&provider);
    match resolve_provider_binary_for_list(program, path_value) {
        Ok(path) => ProviderInfo {
            name: provider_display_name(&provider).to_string(),
            code: provider,
            scanned: true,
            version: None,
            path: Some(path.to_string_lossy().to_string()),
            available: true,
            error: None,
        },
        Err(error) => ProviderInfo {
            name: provider_display_name(&provider).to_string(),
            code: provider,
            scanned: true,
            version: None,
            path: None,
            available: false,
            error: Some(error),
        },
    }
}

fn normalize_update_settings(
    input: UpdateSettingsInput,
    provider_scan: &HashMap<ProviderCode, ProviderCli>,
) -> Result<PedelecSettings, PedelecError> {
    let provider_info = provider_info_for(input.default_provider.clone(), provider_scan, None);
    if input.default_provider != ProviderCode::Ollama && !provider_info.available {
        return Err(PedelecError::with_details(
            error_codes::DEFAULT_PROVIDER_UNAVAILABLE,
            "default provider is not currently available",
            serde_json::json!({
                "provider": provider_code_as_str(&input.default_provider),
                "error": provider_info.error
            }),
        ));
    }

    let provider_settings = normalize_provider_settings(
        input.provider_settings,
        if input.default_provider == ProviderCode::Ollama {
            OllamaValidationMode::Required
        } else {
            OllamaValidationMode::Optional
        },
    )?;

    if input.default_provider == ProviderCode::Ollama {
        required_ollama_model_from_args(&provider_settings.ollama.efforts_args.default)?;
    }

    Ok(PedelecSettings {
        default_provider: Some(input.default_provider),
        provider_settings,
        wizard_metadata: EffortWizardMetadata::default(),
    })
}

#[cfg(test)]
fn normalize_update_settings_for_test(
    input: UpdateSettingsInput,
    path_value: Option<&OsString>,
) -> Result<PedelecSettings, PedelecError> {
    let scan = scan_external_providers(path_value.cloned());
    normalize_update_settings(input, &scan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OllamaValidationMode {
    Required,
    Optional,
}

fn normalize_provider_settings(
    settings: ProviderSettingsInput,
    ollama_validation: OllamaValidationMode,
) -> Result<ProviderSettings, PedelecError> {
    Ok(ProviderSettings {
        codex: CommonProviderSettings {
            efforts_args: normalize_efforts_args(ProviderCode::Codex, settings.codex.efforts_args)?,
        },
        antigravity: CommonProviderSettings {
            efforts_args: normalize_efforts_args(
                ProviderCode::Antigravity,
                settings.antigravity.efforts_args,
            )?,
        },
        opencode: CommonProviderSettings {
            efforts_args: normalize_efforts_args(
                ProviderCode::OpenCode,
                settings.opencode.efforts_args,
            )?,
        },
        cursor: CommonProviderSettings {
            efforts_args: normalize_efforts_args(
                ProviderCode::Cursor,
                settings.cursor.efforts_args,
            )?,
        },
        claude: CommonProviderSettings {
            efforts_args: normalize_efforts_args(
                ProviderCode::Claude,
                settings.claude.efforts_args,
            )?,
        },
        ollama: normalize_ollama_provider_settings(settings.ollama, ollama_validation)?,
    })
}

fn normalize_ollama_provider_settings(
    settings: OllamaProviderSettingsInput,
    validation: OllamaValidationMode,
) -> Result<OllamaProviderSettings, PedelecError> {
    Ok(OllamaProviderSettings {
        base_url: normalize_ollama_base_url(settings.base_url)?,
        timeout_ms: validate_ollama_timeout(
            settings.timeout_ms.unwrap_or(DEFAULT_OLLAMA_TIMEOUT_MS),
        )?,
        api_key: match validation {
            OllamaValidationMode::Required => require_ollama_api_key(settings.api_key)?,
            OllamaValidationMode::Optional => normalize_optional_secret(settings.api_key),
        },
        tavily_api_key: normalize_optional_secret(settings.tavily_api_key),
        efforts_args: normalize_efforts_args(ProviderCode::Ollama, settings.efforts_args)?,
    })
}

fn normalize_optional_secret(value: Option<String>) -> String {
    value.unwrap_or_default().trim().to_string()
}

fn resolve_profile_session_args(
    settings: &PedelecSettings,
    provider: &ProviderCode,
    level: EffortLevel,
) -> Result<Vec<String>, PedelecError> {
    let efforts = provider_efforts_args(&settings.provider_settings, provider);
    validate_effort_tier(provider, level, efforts.get(level))?;
    let args = efforts.get(level).to_vec();
    if *provider == ProviderCode::Ollama {
        required_ollama_model_from_args(&args)?;
    }
    Ok(args)
}

fn resolve_explicit_session_args(
    provider: &ProviderCode,
    model: &str,
    effort: Option<&str>,
) -> Result<Vec<String>, PedelecError> {
    let mut args = vec![
        provider_model_arg_key(provider).to_string(),
        model.to_string(),
    ];
    if let Some(effort) = effort {
        let effort = normalize_explicit_effort(provider, effort)?;
        match provider {
            ProviderCode::Codex => args.extend([
                "-c".to_string(),
                format!("model_reasoning_effort=\"{effort}\""),
            ]),
            ProviderCode::Antigravity | ProviderCode::Claude => {
                args.extend(["--effort".to_string(), effort.to_string()])
            }
            ProviderCode::OpenCode | ProviderCode::Cursor | ProviderCode::Ollama => {
                unreachable!("unsupported explicit effort was rejected")
            }
        }
    }
    validate_effort_tier(provider, EffortLevel::Default, &args)?;
    Ok(args)
}

fn normalize_explicit_model(value: Option<String>) -> Result<Option<String>, PedelecError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let model = value.trim();
    if model.is_empty() {
        return Err(PedelecError::new(
            error_codes::INVALID_INPUT,
            "model must be a non-empty string when provided",
        ));
    }
    Ok(Some(model.to_string()))
}

fn normalize_explicit_effort<'a>(
    provider: &ProviderCode,
    value: &'a str,
) -> Result<&'a str, PedelecError> {
    let effort = value.trim();
    let supported = match provider {
        ProviderCode::Codex => is_supported_codex_effort(effort),
        ProviderCode::Antigravity => is_supported_antigravity_effort(effort),
        ProviderCode::Claude => is_supported_claude_effort(effort),
        ProviderCode::OpenCode | ProviderCode::Cursor | ProviderCode::Ollama => false,
    };
    if supported {
        return Ok(effort);
    }
    Err(PedelecError::with_details(
        error_codes::INVALID_INPUT,
        "explicit effort is not supported for this provider",
        serde_json::json!({
            "provider": provider_code_as_str(provider),
            "effort": effort,
        }),
    ))
}

fn provider_efforts_args<'a>(
    settings: &'a ProviderSettings,
    provider: &ProviderCode,
) -> &'a EffortsArgs {
    match provider {
        ProviderCode::Codex => &settings.codex.efforts_args,
        ProviderCode::Antigravity => &settings.antigravity.efforts_args,
        ProviderCode::OpenCode => &settings.opencode.efforts_args,
        ProviderCode::Cursor => &settings.cursor.efforts_args,
        ProviderCode::Claude => &settings.claude.efforts_args,
        ProviderCode::Ollama => &settings.ollama.efforts_args,
    }
}

fn normalize_efforts_args(
    provider: ProviderCode,
    efforts: EffortsArgs,
) -> Result<EffortsArgs, PedelecError> {
    validate_effort_tier(&provider, EffortLevel::Default, &efforts.default)?;
    validate_effort_tier(&provider, EffortLevel::Low, &efforts.low)?;
    validate_effort_tier(&provider, EffortLevel::High, &efforts.high)?;
    Ok(efforts)
}

fn validate_effort_tier(
    provider: &ProviderCode,
    level: EffortLevel,
    args: &[String],
) -> Result<(), PedelecError> {
    if args.len() % 2 != 0 {
        return Err(effort_args_error(
            provider,
            level,
            "effort args must contain key/value pairs",
        ));
    }

    let mut seen = Vec::new();
    for pair in args.chunks_exact(2) {
        let key = pair[0].as_str();
        let value = pair[1].trim();
        if value.is_empty() {
            return Err(effort_args_error(
                provider,
                level,
                "effort arg values must not be empty",
            ));
        }
        if seen.iter().any(|candidate| *candidate == key) {
            return Err(effort_args_error(
                provider,
                level,
                "duplicate effort arg keys are not allowed",
            ));
        }
        seen.push(key);

        let model_key = provider_model_arg_key(provider);
        let allowed = if key == model_key {
            true
        } else {
            match provider {
                ProviderCode::Codex => {
                    key == "-c"
                        && parse_codex_reasoning_effort(value)
                            .is_some_and(is_supported_codex_effort)
                }
                ProviderCode::Antigravity => {
                    key == "--effort" && is_supported_antigravity_effort(value)
                }
                ProviderCode::Claude => key == "--effort" && is_supported_claude_effort(value),
                ProviderCode::OpenCode | ProviderCode::Cursor | ProviderCode::Ollama => false,
            }
        };
        if !allowed {
            return Err(effort_args_error(
                provider,
                level,
                "effort arg key or native value is not allowed for this provider",
            ));
        }
    }
    Ok(())
}

fn parse_codex_reasoning_effort(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    let remainder = trimmed.strip_prefix("model_reasoning_effort")?.trim_start();
    let raw = remainder.strip_prefix('=')?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(quoted) = raw.strip_prefix('"') {
        let quoted = quoted.strip_suffix('"')?.trim();
        if quoted.is_empty() || quoted.contains('"') {
            return None;
        }
        return Some(quoted);
    }
    if raw.contains('"') {
        return None;
    }
    Some(raw)
}

fn is_supported_codex_effort(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high" | "xhigh" | "max")
}

fn is_supported_antigravity_effort(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high")
}

fn is_supported_claude_effort(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high" | "xhigh" | "max")
}

fn effort_args_error(provider: &ProviderCode, level: EffortLevel, message: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::INVALID_INPUT,
        message,
        serde_json::json!({
            "provider": provider_code_as_str(provider),
            "effortLevel": level,
        }),
    )
}

pub fn normalize_ollama_base_url(value: Option<String>) -> Result<String, PedelecError> {
    let trimmed = value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_OLLAMA_BASE_URL);
    validate_ollama_base_url(trimmed)
}

pub fn validate_ollama_base_url(value: &str) -> Result<String, PedelecError> {
    let trimmed = value.trim();
    let parsed = Url::parse(trimmed).map_err(|err| {
        PedelecError::with_details(
            error_codes::OLLAMA_BASE_URL_INVALID,
            "Ollama Base URL must be a valid http(s) URL and must not include /api.",
            serde_json::json!({ "field": "baseUrl", "value": value, "error": err.to_string() }),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || !parsed.has_host() {
        return Err(PedelecError::with_details(
            error_codes::OLLAMA_BASE_URL_INVALID,
            "Ollama Base URL must be a valid http(s) URL and must not include /api.",
            serde_json::json!({ "field": "baseUrl", "value": value }),
        ));
    }
    if parsed
        .path_segments()
        .is_some_and(|mut segments| segments.any(|segment| segment.eq_ignore_ascii_case("api")))
    {
        return Err(PedelecError::with_details(
            error_codes::OLLAMA_BASE_URL_INVALID,
            "Ollama Base URL must be a valid http(s) URL and must not include /api.",
            serde_json::json!({ "field": "baseUrl", "value": value }),
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

pub fn validate_ollama_timeout(value: u64) -> Result<u64, PedelecError> {
    if value == 0 {
        return Err(PedelecError::with_details(
            error_codes::OLLAMA_REQUEST_FAILED,
            "Ollama timeout must be greater than 0 milliseconds.",
            serde_json::json!({ "field": "timeoutMs", "value": value }),
        ));
    }
    Ok(value)
}

fn require_ollama_api_key(value: Option<String>) -> Result<String, PedelecError> {
    let trimmed = value.as_deref().map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return Err(PedelecError::new(
            error_codes::OLLAMA_API_KEY_REQUIRED,
            "Ollama API key is required. For local models, enter 'ollama'.",
        ));
    }
    Ok(trimmed.to_string())
}

fn list_ollama_models(
    input: ListOllamaModelsInput,
) -> Result<Vec<OllamaModelOption>, PedelecError> {
    let base_url = normalize_ollama_base_url(input.base_url)?;
    let timeout_ms =
        validate_ollama_timeout(input.timeout_ms.unwrap_or(DEFAULT_OLLAMA_TIMEOUT_MS))?;
    let api_key = require_ollama_api_key(input.api_key)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|err| {
            PedelecError::with_details(
                error_codes::OLLAMA_REQUEST_FAILED,
                "Ollama request failed.",
                serde_json::json!({ "error": err.to_string(), "timeoutMs": timeout_ms }),
            )
        })?;
    let url = format!("{base_url}/api/tags");
    let response = client.get(&url).bearer_auth(api_key).send().map_err(|err| {
        let code = if err.is_timeout() || err.is_connect() {
            error_codes::OLLAMA_UNAVAILABLE
        } else {
            error_codes::OLLAMA_REQUEST_FAILED
        };
        PedelecError::with_details(
            code,
            if code == error_codes::OLLAMA_UNAVAILABLE {
                "Ollama is unavailable. Check the Base URL, network connection, and timeout setting."
            } else {
                "Ollama request failed."
            },
            serde_json::json!({ "url": url, "timeoutMs": timeout_ms, "error": err.to_string() }),
        )
    })?;
    let status = response.status();
    let text = response.text().map_err(|err| {
        PedelecError::with_details(
            error_codes::OLLAMA_REQUEST_FAILED,
            "Ollama request failed.",
            serde_json::json!({ "url": url, "error": err.to_string() }),
        )
    })?;
    if !status.is_success() {
        return Err(ollama_http_status_error(status.as_u16(), &text, Some(url)));
    }
    parse_ollama_models_response(&text)
}

fn check_ollama_connection(input: CheckOllamaConnectionInput) -> CheckOllamaConnectionOutput {
    check_ollama_connection_with_timeout(input, OLLAMA_CONNECTION_CHECK_TIMEOUT_MS)
}

fn check_ollama_connection_with_timeout(
    input: CheckOllamaConnectionInput,
    timeout_ms: u64,
) -> CheckOllamaConnectionOutput {
    let Ok(base_url) = normalize_ollama_base_url(input.base_url) else {
        return CheckOllamaConnectionOutput { connected: false };
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    else {
        return CheckOllamaConnectionOutput { connected: false };
    };
    let Ok(response) = client.get(format!("{base_url}/api/tags")).send() else {
        return CheckOllamaConnectionOutput { connected: false };
    };
    if !response.status().is_success() {
        return CheckOllamaConnectionOutput { connected: false };
    }
    let Ok(text) = response.text() else {
        return CheckOllamaConnectionOutput { connected: false };
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return CheckOllamaConnectionOutput { connected: false };
    };
    CheckOllamaConnectionOutput {
        connected: is_valid_ollama_tags_response(&value),
    }
}

fn is_valid_ollama_tags_response(value: &Value) -> bool {
    value.get("models").and_then(Value::as_array).is_some()
}

fn ollama_http_status_error(status: u16, body: &str, url: Option<String>) -> PedelecError {
    let lower_body = body.to_ascii_lowercase();
    let (code, message) = match status {
        401 | 403 => (
            error_codes::OLLAMA_AUTH_FAILED,
            "Ollama authentication failed. Check your API key.",
        ),
        429 => (
            error_codes::OLLAMA_CLOUD_LIMIT_EXCEEDED,
            "Ollama Cloud limit was exceeded. Try again later or check your Ollama account usage.",
        ),
        404 if lower_body.contains("model") && lower_body.contains("not found") => (
            error_codes::OLLAMA_MODEL_NOT_FOUND,
            "Ollama model was not found. Refresh the model list and choose an available model.",
        ),
        _ => (error_codes::OLLAMA_REQUEST_FAILED, "Ollama request failed."),
    };
    let mut details = serde_json::json!({ "status": status, "body": body });
    if let Some(url) = url {
        details["url"] = Value::String(url);
    }
    PedelecError::with_details(code, message, details)
}

fn parse_ollama_models_response(text: &str) -> Result<Vec<OllamaModelOption>, PedelecError> {
    let value = serde_json::from_str::<Value>(text).map_err(|err| {
        PedelecError::with_details(
            error_codes::OLLAMA_RESPONSE_INVALID,
            "Ollama response was invalid.",
            serde_json::json!({ "error": err.to_string(), "body": text }),
        )
    })?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PedelecError::with_details(
                error_codes::OLLAMA_RESPONSE_INVALID,
                "Ollama response was invalid.",
                serde_json::json!({ "body": value }),
            )
        })?;
    Ok(models
        .iter()
        .filter_map(|item| {
            let model = item.get("model")?.as_str()?.trim();
            if model.is_empty() {
                return None;
            }
            let label = item
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(model);
            Some(OllamaModelOption {
                value: model.to_string(),
                label: label.to_string(),
            })
        })
        .collect())
}

fn default_settings_file_path() -> Result<PathBuf, PedelecError> {
    pedelec_shared::paths::pedelec_home_dir()
        .map(|home| home.join("settings.json"))
        .map_err(|err| PedelecError {
            code: err.code,
            message: err.message,
            details: err.details,
        })
}

fn read_settings_file(path: &Path) -> Result<PedelecSettings, PedelecError> {
    if !path.exists() {
        return Ok(PedelecSettings::default());
    }

    let content = fs::read_to_string(path).map_err(|err| {
        PedelecError::with_details(
            error_codes::SETTINGS_READ_FAILED,
            "cannot read Pedelec settings",
            serde_json::json!({
                "path": path.to_string_lossy(),
                "error": err.to_string()
            }),
        )
    })?;

    serde_json::from_str(&content).map_err(|err| {
        PedelecError::with_details(
            error_codes::SETTINGS_READ_FAILED,
            "Pedelec settings file was not valid JSON",
            serde_json::json!({
                "path": path.to_string_lossy(),
                "error": err.to_string()
            }),
        )
    })
}

fn write_settings_file(path: &Path, settings: &PedelecSettings) -> Result<(), PedelecError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            PedelecError::with_details(
                error_codes::SETTINGS_WRITE_FAILED,
                "cannot create Pedelec settings directory",
                serde_json::json!({
                    "path": parent.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })?;
    }

    let content = serde_json::to_string_pretty(settings).map_err(|err| {
        PedelecError::with_details(
            error_codes::SETTINGS_WRITE_FAILED,
            "cannot serialize Pedelec settings",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    fs::write(path, content).map_err(|err| {
        PedelecError::with_details(
            error_codes::SETTINGS_WRITE_FAILED,
            "cannot write Pedelec settings",
            serde_json::json!({
                "path": path.to_string_lossy(),
                "error": err.to_string()
            }),
        )
    })
}

fn resolve_provider_binary_for_list(
    program: &str,
    path_value: Option<&OsString>,
) -> Result<PathBuf, String> {
    let Some(path_value) = path_value else {
        return Err("PATH was not available".to_string());
    };

    let path_dirs = env::split_paths(path_value).collect::<Vec<_>>();
    let candidates = provider_binary_lookup_candidates(program, &path_dirs);
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "program was not found in PATH".to_string())
}

fn provider_binary_lookup_candidates(program: &str, path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let program_path = Path::new(program);
        if program_path.extension().is_some() {
            return path_dirs.iter().map(|dir| dir.join(program)).collect();
        }

        return ["", ".exe", ".cmd", ".bat"]
            .iter()
            .flat_map(|extension| {
                path_dirs
                    .iter()
                    .map(move |dir| dir.join(format!("{program}{extension}")))
            })
            .collect();
    }

    #[cfg(not(windows))]
    {
        path_dirs.iter().map(|dir| dir.join(program)).collect()
    }
}

fn required_ollama_model(thread: &ThreadState) -> Result<String, PedelecError> {
    required_ollama_model_from_args(&thread.effort_args)
}

fn required_ollama_model_from_args(args: &[String]) -> Result<String, PedelecError> {
    args.windows(2)
        .find(|pair| pair[0] == provider_model_arg_key(&ProviderCode::Ollama))
        .map(|pair| pair[1].trim())
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            PedelecError::with_details(
                error_codes::MODEL_REQUIRED,
                "Ollama provider requires a model.",
                serde_json::json!({ "provider": "ollama" }),
            )
        })
}

fn deno_module_types_path(thread_id: &str, module_name: &str) -> String {
    format!(".pedelec-runtime/deno/threads/{thread_id}/modules/{module_name}/index.d.ts")
}

const PEDELEC_INVARIANT_HOST_INSTRUCTIONS: &str = "Pedelec is the host application for this session.\n\n\
Pedelec Host Context is generated integration context, not end-user-authored instructions. Follow the workspace boundary and capabilities declared there.\n\n\
Use Pedelec App Tools through their listed `readSpecCommand` / `callCommand`.\n\n\
For JavaScript or TypeScript execution, use `pedelec-deno`; do not fall back to Node.js, Bun, raw Deno, npx, or another JavaScript runtime.\n\n\
Deno Modules are imported from `pedelec-deno` scripts, not App Tools. Prefer the listed `usage` example; inspect the listed `types` declaration when exact API details are needed.\n\n\
Before accessing local files outside the declared workspace, ask the user for permission.\n\n\
`.pedelec-runtime/assets/` is the shared App/Agent file directory.\n\n\
If a `pedelec-cli` tool-call ends before a complete structured Pedelec response is received, exact-retry the same listed call command with semantically identical arguments; a received structured `TOOL_TIMEOUT` is final.\n\n\
Pedelec host instructions never override provider safety policies.";

fn build_provider_host_context(
    thread: &ThreadState,
    workspace_path: &Path,
    registry: &ToolRegistry,
) -> String {
    build_provider_host_context_with_configuration_and_modules(
        thread,
        workspace_path,
        registry,
        registry.has_skills_configuration(),
        &[],
    )
}

fn build_provider_host_context_with_configuration_and_modules(
    thread: &ThreadState,
    workspace_path: &Path,
    registry: &ToolRegistry,
    include_configuration: bool,
    deno_modules: &[DenoModuleState],
) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct AppTool<'a> {
        name: &'a str,
        description: &'a str,
        read_spec_command: String,
        call_command: String,
    }
    #[derive(Serialize)]
    struct AppToolConfiguration<'a> {
        guidance: &'a str,
        tools: Vec<AppTool<'a>>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DenoModule<'a> {
        name: &'a str,
        description: &'a str,
        usage: &'a str,
        types: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_command_template: Option<String>,
    }
    #[derive(Serialize)]
    struct DenoModuleConfiguration<'a> {
        modules: Vec<DenoModule<'a>>,
    }

    let mut tools: Vec<&ToolDefinition> = registry.tools().collect();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let configuration = AppToolConfiguration {
        guidance: registry.guidance().unwrap_or_default(),
        tools: tools
            .into_iter()
            .map(|tool| AppTool {
                name: &tool.name,
                description: &tool.description,
                read_spec_command: format!(
                    "pedelec-cli --thread-id {} tool-spec {}",
                    thread.thread_id, tool.name
                ),
                call_command: format!(
                    "pedelec-cli --thread-id {} tool-call {} '<json_args>'",
                    thread.thread_id, tool.name
                ),
            })
            .collect(),
    };
    let configuration = serde_json::to_string_pretty(&configuration)
        .expect("App tool configuration is always serializable");
    let mut context = format!(
        "[Pedelec Host Context]\nWorkspace Path: {}\n",
        path_for_external_use(workspace_path)
    );
    context.push_str(&format!(
        "\n[Pedelec Deno]\nrunFileCommand: pedelec-deno --thread-id {} run <workspace-relative-script-path>\nrunStdinCommand: pedelec-deno --thread-id {} run -\n[/Pedelec Deno]\n",
        thread.thread_id, thread.thread_id
    ));
    if !deno_modules.is_empty() {
        let mut modules = deno_modules.iter().collect::<Vec<_>>();
        modules.sort_by(|left, right| left.name.cmp(&right.name));
        let configuration = serde_json::to_string_pretty(&DenoModuleConfiguration {
            modules: modules
                .into_iter()
                .map(|module| DenoModule {
                    name: &module.name,
                    description: &module.description,
                    usage: &module.usage,
                    types: deno_module_types_path(&thread.thread_id, &module.name),
                    run_command_template: module.prefer_stdin_execution.then(|| {
                        format!(
                            "@'\n<typescript-source>\n'@ | pedelec-deno --thread-id {} run -",
                            thread.thread_id
                        )
                    }),
                })
                .collect(),
        })
        .expect("Deno Module configuration is always serializable");
        context.push_str(&format!(
            "\n[Pedelec Deno Modules]\n{configuration}\n[/Pedelec Deno Modules]\n"
        ));
    }
    if include_configuration {
        context.push_str(&format!(
            "\n[Pedelec App Tool Configuration]\n{configuration}\n[/Pedelec App Tool Configuration]\n"
        ));
    }
    context.push_str("[/Pedelec Host Context]\n\n------\n\n");
    context
}

#[allow(dead_code)]
fn build_provider_instruction(
    thread: &ThreadState,
    workspace_path: &Path,
    registry: &ToolRegistry,
) -> String {
    build_provider_host_context(thread, workspace_path, registry)
}

fn insert_antigravity_custom_agent_body(bootstrap: &str) -> String {
    format!(
        "---\nname: pedelec-runtime\ndescription: Pedelec host integration bootstrap for Pedelec-managed agent sessions.\nmainAgent: true\nsubagent: false\n---\n\n# System Prompt\n\n{bootstrap}\n"
    )
}

/// Materializes the static Antigravity workspace agent required by the
/// persistent `--agent pedelec-runtime` launch contract.
fn ensure_antigravity_custom_agent(workspace_path: &Path) -> Result<(), PedelecError> {
    let agent_dir = workspace_path.join(PEDELEC_ANTIGRAVITY_AGENT_DIR);
    let agent_path = agent_dir.join(PEDELEC_ANTIGRAVITY_AGENT_FILE);
    let expected = insert_antigravity_custom_agent_body(&build_pedelec_bootstrap_instruction());
    if fs::read_to_string(&agent_path).ok().as_deref() == Some(expected.as_str()) {
        return Ok(());
    }
    fs::create_dir_all(&agent_dir).map_err(|error| {
        PedelecError::with_details(
            error_codes::PROVIDER_BOOTSTRAP_ASSET_FAILED,
            "failed to create Antigravity custom agent directory",
            serde_json::json!({
                "provider": "antigravity",
                "path": path_for_external_use(&agent_dir),
                "error": error.to_string()
            }),
        )
    })?;
    fs::write(&agent_path, expected).map_err(|error| {
        PedelecError::with_details(
            error_codes::PROVIDER_BOOTSTRAP_ASSET_FAILED,
            "failed to write Antigravity custom agent",
            serde_json::json!({
                "provider": "antigravity",
                "path": path_for_external_use(&agent_path),
                "error": error.to_string()
            }),
        )
    })
}

fn antigravity_custom_agent_version_supported(version: &ProviderVersion) -> bool {
    version.0.as_slice() >= &[1, 1, 6]
}

fn build_pedelec_bootstrap_instruction() -> String {
    PEDELEC_INVARIANT_HOST_INSTRUCTIONS.to_owned()
}

/// Persistent providers receive host integration context without the legacy
/// synthetic prepare turn or its `PEDELEC_PREPARED` acknowledgement. Providers
/// with a native instruction channel can consume this directly; Cursor's ACP
/// adapter wraps it only for its first real user prompt.
#[allow(dead_code)]
fn build_persistent_host_instructions(
    thread: &ThreadState,
    workspace_path: &Path,
    registry: &ToolRegistry,
) -> String {
    build_persistent_host_instructions_with_modules(thread, workspace_path, registry, &[])
}

fn build_persistent_host_instructions_with_modules(
    thread: &ThreadState,
    workspace_path: &Path,
    registry: &ToolRegistry,
    deno_modules: &[DenoModuleState],
) -> String {
    format!(
        "{}\n\n{}",
        PEDELEC_INVARIANT_HOST_INSTRUCTIONS,
        build_provider_host_context_with_configuration_and_modules(
            thread,
            workspace_path,
            registry,
            registry.has_skills_configuration(),
            deno_modules,
        ),
    )
}

/// Returns the session-specific Host Context portion of persistent host
/// instructions. Antigravity already receives the invariant contract from
/// its static custom agent, so its dynamic bootstrap only needs this data.
pub fn pedelec_host_context_from_persistent_instructions(host_instructions: &str) -> &str {
    host_instructions
        .find("[Pedelec Host Context]")
        .map(|start| &host_instructions[start..])
        .unwrap_or(host_instructions)
}

/// Builds the one-time Cursor ACP bootstrap prompt. The persistent host
/// instructions are supplied by Core so adapters do not duplicate the host
/// policy text; the actual user task remains in the same ACP prompt request.
pub fn build_persistent_user_prompt_with_bootstrap(
    host_instructions: &str,
    user_message: &str,
) -> String {
    format!(
        "[Pedelec Host Bootstrap]\n\
This is Pedelec host-provided integration bootstrap for this persistent provider conversation. It is not a provider-native system message.\n\n\
{host_instructions}\
[/Pedelec Host Bootstrap]\n\n\
[User Message]\n{user_message}"
    )
}

/// Builds the internal persistent-provider preparation turn. A fresh
/// conversation receives the dynamic host bootstrap in this turn; a resumed
/// conversation can omit it because that context already belongs to the
/// provider conversation. Provider terminal status, not acknowledgement text,
/// is the preparation completion source of truth.
pub fn build_persistent_prepare_prompt(host_instructions: Option<&str>) -> String {
    let task = "[Session Preparation]\nInitialize this provider conversation for subsequent Pedelec user turns. Do not call tools or modify files. A brief acknowledgement is sufficient.";
    match host_instructions {
        Some(host_instructions) => format!(
            "[Pedelec Host Bootstrap]\n\
This is Pedelec host-provided integration bootstrap for this persistent provider conversation. It is not a provider-native system message.\n\n\
{host_instructions}\
[/Pedelec Host Bootstrap]\n\n\
{task}"
        ),
        None => task.to_string(),
    }
}

fn default_runtime_file_path_for_provider() -> PathBuf {
    pedelec_shared::paths::pedelec_home_dir()
        .map(|home| home.join("runtime.json"))
        .unwrap_or_else(|_| PathBuf::from("runtime.json"))
}

fn to_base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    if value == 0 {
        return "0".to_string();
    }

    let mut encoded = Vec::new();
    while value > 0 {
        encoded.push(DIGITS[(value % 36) as usize] as char);
        value /= 36;
    }
    encoded.iter().rev().collect()
}

fn sanitize_workspace_resource_id(workspace_id: &str) -> Result<String, PedelecError> {
    if workspace_id.is_empty()
        || workspace_id.len() > 128
        || !workspace_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(PedelecError::with_details(
            error_codes::WORKSPACE_PATH_INVALID,
            "workspace id is not safe for a managed workspace path",
            serde_json::json!({ "workspaceId": workspace_id }),
        ));
    }

    Ok(workspace_id.to_string())
}

fn validate_skill_url_and_filename(skill_url: &str) -> Result<(Url, String, String), PedelecError> {
    let lower_url = skill_url.to_ascii_lowercase();
    if lower_url.contains("/../")
        || lower_url.contains("/./")
        || lower_url.ends_with("/..")
        || lower_url.ends_with("/.")
        || lower_url.contains("%2e%2e")
        || lower_url.contains("%2e/")
    {
        return Err(PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill URL path contains traversal syntax",
            serde_json::json!({ "url": skill_url }),
        ));
    }

    let url = Url::parse(skill_url).map_err(|err| {
        PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill URL is invalid",
            serde_json::json!({ "url": skill_url, "error": err.to_string() }),
        )
    })?;

    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&url) => {}
        _ => {
            return Err(PedelecError::with_details(
                error_codes::SKILL_URL_INVALID,
                "skill URL scheme or host is not allowed",
                serde_json::json!({ "url": skill_url }),
            ));
        }
    }

    let mut original_filename = None;
    let segments = url.path_segments().ok_or_else(|| {
        PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill URL must have path segments",
            serde_json::json!({ "url": skill_url }),
        )
    })?;

    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.contains('\\') || segment.contains('/') {
            return Err(PedelecError::with_details(
                error_codes::SKILL_URL_INVALID,
                "skill URL path contains unsafe segments",
                serde_json::json!({ "url": skill_url }),
            ));
        }
        original_filename = Some(segment.to_string());
    }

    let original_filename = original_filename.ok_or_else(|| {
        PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill URL must include a filename",
            serde_json::json!({ "url": skill_url }),
        )
    })?;

    let safe_filename = sanitize_filename(&original_filename).ok_or_else(|| {
        PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill filename is not safe",
            serde_json::json!({ "url": skill_url, "filename": original_filename }),
        )
    })?;

    Ok((url, original_filename, safe_filename))
}

fn is_loopback_host(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

fn sanitize_filename(filename: &str) -> Option<String> {
    let sanitized: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('.').to_string();

    if sanitized.is_empty()
        || sanitized == "."
        || sanitized == ".."
        || sanitized.contains('/')
        || sanitized.contains('\\')
    {
        None
    } else {
        Some(sanitized)
    }
}

fn unique_filename(safe_filename: &str, used_filenames: &mut HashMap<String, usize>) -> String {
    let count = used_filenames.entry(safe_filename.to_string()).or_insert(0);
    let filename = if *count == 0 {
        safe_filename.to_string()
    } else {
        let path = Path::new(safe_filename);
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(safe_filename);
        let extension = path.extension().and_then(|extension| extension.to_str());

        match extension {
            Some(extension) if !extension.is_empty() => format!("{stem}_{count}.{extension}"),
            _ => format!("{safe_filename}_{count}"),
        }
    };
    *count += 1;
    filename
}

fn unique_available_filename(
    safe_filename: &str,
    directory: &Path,
    used_filenames: &mut HashMap<String, usize>,
) -> String {
    loop {
        let filename = unique_filename(safe_filename, used_filenames);
        if !directory.join(&filename).exists() {
            return filename;
        }
    }
}

fn write_generated_tool_specs(
    skills_dir: impl AsRef<Path>,
    registry: &ToolRegistry,
) -> Result<Vec<SkillFile>, PedelecError> {
    let skills_dir = skills_dir.as_ref();
    fs::create_dir_all(skills_dir).map_err(|err| {
        skill_download_error(
            "cannot create skills directory",
            None,
            Some(skills_dir),
            err,
        )
    })?;
    let canonical_skills_dir = skills_dir.canonicalize().map_err(|err| {
        skill_download_error(
            "cannot canonicalize skills directory",
            None,
            Some(skills_dir),
            err,
        )
    })?;

    let mut files = Vec::new();
    let mut tools: Vec<&ToolDefinition> = registry.tools().collect();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    for tool in tools {
        let filename = format!("tools-{}.json", sanitize_tool_filename_part(&tool.name));
        let content = serde_json::to_vec_pretty(tool).map_err(|err| {
            PedelecError::with_details(
                error_codes::TOOLS_MANIFEST_INVALID,
                "cannot serialize generated tool spec",
                serde_json::json!({ "toolName": tool.name, "error": err.to_string() }),
            )
        })?;
        files.push(write_generated_skill_file(
            &canonical_skills_dir,
            &filename,
            &format!("generated:{filename}"),
            &content,
        )?);
    }

    Ok(files)
}

fn write_generated_skill_file(
    canonical_skills_dir: &Path,
    filename: &str,
    original_url: &str,
    bytes: &[u8],
) -> Result<SkillFile, PedelecError> {
    let target_path = canonical_skills_dir.join(filename);
    ensure_child_path(canonical_skills_dir, &target_path)?;
    fs::write(&target_path, bytes).map_err(|err| {
        skill_download_error(
            "cannot write generated skill file",
            Some(original_url),
            Some(&target_path),
            err,
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(SkillFile {
        original_url: original_url.to_string(),
        original_filename: filename.to_string(),
        local_path: target_path,
        sha256: format!("{:x}", hasher.finalize()),
        size_bytes: bytes.len() as u64,
    })
}

fn sanitize_tool_filename_part(tool_name: &str) -> String {
    tool_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn ensure_child_path(parent: &Path, child: &Path) -> Result<(), PedelecError> {
    for component in child.components() {
        if matches!(component, Component::ParentDir) {
            return Err(PedelecError::with_details(
                error_codes::SKILL_URL_INVALID,
                "skill target path contains parent directory traversal",
                serde_json::json!({ "path": child.to_string_lossy() }),
            ));
        }
    }

    if !child.starts_with(parent) {
        return Err(PedelecError::with_details(
            error_codes::SKILL_URL_INVALID,
            "skill target path is outside skills directory",
            serde_json::json!({ "path": child.to_string_lossy() }),
        ));
    }

    Ok(())
}

fn workspace_io_error(
    code: &'static str,
    message: &'static str,
    path: &Path,
    err: std::io::Error,
) -> PedelecError {
    PedelecError::with_details(
        code,
        message,
        serde_json::json!({ "path": path_for_external_use(path), "error": err.to_string() }),
    )
}

fn workspace_open_error(
    thread_id: &str,
    workspace_path: &Path,
    message: &'static str,
    err: std::io::Error,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::WORKSPACE_OPEN_FAILED,
        message,
        serde_json::json!({
            "threadId": thread_id,
            "workspacePath": path_for_external_use(workspace_path),
            "error": err.to_string(),
        }),
    )
}

fn skill_download_error(
    message: &'static str,
    url: Option<&str>,
    path: Option<&Path>,
    err: std::io::Error,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::SKILL_DOWNLOAD_FAILED,
        message,
        serde_json::json!({
            "url": url,
            "path": path.map(|path| path.to_string_lossy().to_string()),
            "error": err.to_string()
        }),
    )
}

#[cfg(test)]
mod deno_tests {
    use super::*;

    fn file_input(thread_id: &str, entrypoint: &str, args: Vec<String>) -> DenoRunInput {
        DenoRunInput {
            thread_id: thread_id.into(),
            target: DenoRunTarget::WorkspaceFile {
                entrypoint: entrypoint.into(),
            },
            args,
        }
    }

    fn stdin_input(thread_id: &str, source: &str, args: Vec<String>) -> DenoRunInput {
        DenoRunInput {
            thread_id: thread_id.into(),
            target: DenoRunTarget::StdinSource {
                source: source.into(),
            },
            args,
        }
    }

    fn runtime_with_thread(workspace: &Path, thread_id: &str, status: ThreadStatus) -> CoreRuntime {
        let now = Utc::now();
        let mut runtime = CoreRuntime::new();
        runtime
            .register_workspace_for_test(thread_id, workspace.to_path_buf(), WorkspaceKind::Custom)
            .unwrap();
        fs::create_dir_all(thread_skills_root(workspace, thread_id)).unwrap();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                workspace_id: thread_id.into(),
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Default),
                effort_args: Vec::new(),
                skills: Vec::new(),
                status,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: Some("turn-deno-test".into()),
            },
        );
        runtime
    }

    #[test]
    fn deno_admission_uses_the_thread_workspace_and_does_not_mutate_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("authoritative");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("script.ts"), "console.log('ok')").unwrap();
        let runtime = runtime_with_thread(&workspace, "thread-deno-core", ThreadStatus::Running);

        let intent = runtime
            .prepare_deno_run_intent(file_input(
                "thread-deno-core",
                "script.ts",
                vec!["--allow-net".into()],
            ))
            .unwrap();

        assert_eq!(intent.thread_id, "thread-deno-core");
        assert_eq!(intent.workspace_path, workspace.canonicalize().unwrap());
        assert_eq!(
            intent.target,
            DenoExecutionTarget::WorkspaceFile {
                entrypoint: intent.workspace_path.join("script.ts")
            }
        );
        assert_eq!(intent.args, vec!["--allow-net"]);
        assert_eq!(intent.import_map_path, None);
        assert_eq!(
            runtime.thread_status("thread-deno-core"),
            Some(ThreadStatus::Running)
        );
        assert!(!runtime
            .tool_request_broker
            .has_pending_for_thread("thread-deno-core"));
    }

    #[test]
    fn deno_admission_rejects_inactive_and_missing_entrypoints() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("authoritative");
        fs::create_dir_all(&workspace).unwrap();

        for status in [
            ThreadStatus::Idle,
            ThreadStatus::Starting,
            ThreadStatus::Stopping,
        ] {
            let runtime = runtime_with_thread(&workspace, "thread-deno-status", status);
            let error = runtime
                .prepare_deno_run_intent(file_input("thread-deno-status", "script.ts", Vec::new()))
                .unwrap_err();
            assert_eq!(error.code, error_codes::DENO_THREAD_NOT_ACTIVE);
            let stdin_error = runtime
                .prepare_deno_run_intent(stdin_input(
                    "thread-deno-status",
                    "console.log('stdin')",
                    Vec::new(),
                ))
                .unwrap_err();
            assert_eq!(stdin_error.code, error_codes::DENO_THREAD_NOT_ACTIVE);
        }

        let runtime = runtime_with_thread(&workspace, "thread-deno-ended", ThreadStatus::Ended);
        let error = runtime
            .prepare_deno_run_intent(file_input("thread-deno-ended", "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::THREAD_ENDED);
        let stdin_error = runtime
            .prepare_deno_run_intent(stdin_input(
                "thread-deno-ended",
                "console.log('stdin')",
                Vec::new(),
            ))
            .unwrap_err();
        assert_eq!(stdin_error.code, error_codes::THREAD_ENDED);
        assert_eq!(
            runtime.thread_status("thread-deno-ended"),
            Some(ThreadStatus::Ended)
        );

        let runtime = runtime_with_thread(&workspace, "thread-deno-missing", ThreadStatus::Running);
        let error = runtime
            .prepare_deno_run_intent(file_input("thread-deno-missing", "missing.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ENTRYPOINT_INVALID);
        assert_eq!(
            runtime.thread_status("thread-deno-missing"),
            Some(ThreadStatus::Running)
        );
    }

    #[test]
    fn deno_admission_requires_an_active_provider_turn() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("authoritative");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("script.ts"), "console.log('ok')").unwrap();
        let mut runtime =
            runtime_with_thread(&workspace, "thread-deno-no-turn", ThreadStatus::Running);
        runtime
            .thread_manager
            .provider_state_mut("thread-deno-no-turn")
            .unwrap()
            .active_provider_turn_id = None;

        let error = runtime
            .prepare_deno_run_intent(file_input("thread-deno-no-turn", "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_THREAD_NOT_ACTIVE);
        let stdin_error = runtime
            .prepare_deno_run_intent(stdin_input(
                "thread-deno-no-turn",
                "console.log('stdin')",
                Vec::new(),
            ))
            .unwrap_err();
        assert_eq!(stdin_error.code, error_codes::DENO_THREAD_NOT_ACTIVE);
    }

    #[test]
    fn deno_module_names_use_the_shared_safe_package_contract() {
        for name in ["sprite-tools", "gsap", "@example/sprite-tools", "a.b_c-2"] {
            assert!(
                is_valid_deno_module_name(name),
                "expected valid name: {name}"
            );
        }
        for name in [
            "",
            " ",
            ".",
            "..",
            "./sprite-tools",
            "../sprite-tools",
            "/tmp/sprite-tools",
            "C:sprite-tools",
            "https://example.test/module",
            "file:module",
            "@/sprite-tools",
            "@example",
            "@example/",
            "example/sprite-tools/extra",
            "example\\sprite-tools",
        ] {
            assert!(
                !is_valid_deno_module_name(name),
                "expected invalid name: {name}"
            );
        }

        let duplicate = CreateThreadSkillsInput {
            guidance: String::new(),
            tools: Vec::new(),
            deno_modules: vec![
                CreateThreadDenoModuleInput {
                    name: "sprite-tools".into(),
                    description: "one".into(),
                    usage: "one".into(),
                    prefer_stdin_execution: false,
                },
                CreateThreadDenoModuleInput {
                    name: "sprite-tools".into(),
                    description: "two".into(),
                    usage: "two".into(),
                    prefer_stdin_execution: false,
                },
            ],
        };
        let error = normalize_deno_module_inputs(Some(&duplicate)).unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_NAME_INVALID);
    }

    #[test]
    fn persistent_host_context_describes_modules_without_authoring_entries() {
        let temp = tempfile::tempdir().unwrap();
        let thread = ThreadState {
            thread_id: "thread-deno-context".into(),
            workspace_id: "thread-deno-context".into(),
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            effort_args: Vec::new(),
            skills: Vec::new(),
            status: ThreadStatus::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            sdk_origin: None,
        };
        let modules = vec![DenoModuleState {
            name: "sprite-tools".into(),
            description: "Sprite authoring utilities".into(),
            usage: "import { preview } from \"sprite-tools\";".into(),
            prefer_stdin_execution: true,
            state: DenoModuleSetupState::Ready,
        }];
        let context = build_provider_host_context_with_configuration_and_modules(
            &thread,
            temp.path(),
            &ToolRegistry::default(),
            false,
            &modules,
        );
        assert!(context.contains("[Pedelec Deno Modules]"));
        assert!(context.contains("sprite-tools"));
        assert!(context.contains("Sprite authoring utilities"));
        assert!(context.contains("import { preview } from \\\"sprite-tools\\\";"));
        assert!(context.contains(
            ".pedelec-runtime/deno/threads/thread-deno-context/modules/sprite-tools/index.d.ts"
        ));
        let workspace_path = format!("Workspace Path: {}", path_for_external_use(temp.path()));
        assert!(context.contains(&workspace_path));
        assert!(context.contains(
            "runFileCommand: pedelec-deno --thread-id thread-deno-context run <workspace-relative-script-path>"
        ));
        assert!(
            context.contains("runStdinCommand: pedelec-deno --thread-id thread-deno-context run -")
        );
        assert!(context.contains("index.d.ts"));
        assert!(context.contains(
            "\"runCommandTemplate\": \"@'\\n<typescript-source>\\n'@ | pedelec-deno --thread-id thread-deno-context run -\""
        ));
        assert!(!context.contains("preferStdinExecution"));
        assert!(context.contains("pedelec-deno"));
        assert!(!context.contains("canonical JavaScript/TypeScript runtime"));
        assert!(!context.contains("Do not silently fall back"));
        assert!(!context.contains("authoritative Pedelec workspace root"));
        assert!(!context.contains("For script arguments, append"));
        assert!(!context.contains("entry"));
        assert!(!context.contains("runtimeSource"));
        assert!(!context.contains("contentHash"));
        assert!(!context.contains("index.mjs"));
    }

    #[test]
    fn host_context_omits_empty_module_block_and_sorts_module_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let thread = ThreadState {
            thread_id: "thread-deno-order".into(),
            workspace_id: "thread-deno-order".into(),
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            effort_args: Vec::new(),
            skills: Vec::new(),
            status: ThreadStatus::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            sdk_origin: None,
        };

        let empty = build_provider_host_context_with_configuration_and_modules(
            &thread,
            temp.path(),
            &ToolRegistry::default(),
            false,
            &[],
        );
        assert!(!empty.contains("[Pedelec Deno Modules]"));
        assert!(empty.contains(
            "runFileCommand: pedelec-deno --thread-id thread-deno-order run <workspace-relative-script-path>"
        ));
        assert!(empty.contains("runStdinCommand: pedelec-deno --thread-id thread-deno-order run -"));

        let modules = vec![
            DenoModuleState {
                name: "zeta-tools".into(),
                description: "Zeta".into(),
                usage: "import \\\"zeta-tools\\\";".into(),
                prefer_stdin_execution: true,
                state: DenoModuleSetupState::Ready,
            },
            DenoModuleState {
                name: "alpha-tools".into(),
                description: "Alpha".into(),
                usage: "import \\\"alpha-tools\\\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Ready,
            },
        ];
        let context = build_provider_host_context_with_configuration_and_modules(
            &thread,
            temp.path(),
            &ToolRegistry::default(),
            false,
            &modules,
        );
        assert!(context.find("alpha-tools").unwrap() < context.find("zeta-tools").unwrap());
        assert_eq!(context.matches("\"runCommandTemplate\"").count(), 1);
        assert!(context.contains(
            "\"runCommandTemplate\": \"@'\\n<typescript-source>\\n'@ | pedelec-deno --thread-id thread-deno-order run -\""
        ));
        assert!(context.contains(
            ".pedelec-runtime/deno/threads/thread-deno-order/modules/alpha-tools/index.d.ts"
        ));
        assert!(!context.contains("[Pedelec App Tool Configuration]"));
    }

    #[test]
    fn module_only_host_context_keeps_shared_guidance_without_app_tools() {
        let temp = tempfile::tempdir().unwrap();
        let thread = ThreadState {
            thread_id: "thread-deno-guidance".into(),
            workspace_id: "thread-deno-guidance".into(),
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            effort_args: Vec::new(),
            skills: Vec::new(),
            status: ThreadStatus::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            sdk_origin: None,
        };
        let registry = ToolRegistry::from_skills_input(Some(&CreateThreadSkillsInput {
            guidance: "Use sprite-tools for sprite authoring tasks.".into(),
            tools: Vec::new(),
            deno_modules: vec![],
        }))
        .unwrap();
        let modules = vec![DenoModuleState {
            name: "sprite-tools".into(),
            description: "Sprite utilities".into(),
            usage: "import { preview } from \\\"sprite-tools\\\";".into(),
            prefer_stdin_execution: false,
            state: DenoModuleSetupState::Ready,
        }];
        let context = build_provider_host_context_with_configuration_and_modules(
            &thread,
            temp.path(),
            &registry,
            registry.has_skills_configuration(),
            &modules,
        );
        assert!(context.contains("Use sprite-tools for sprite authoring tasks."));
        assert!(context.contains("[Pedelec Deno Modules]"));
        assert!(context.contains("\"tools\": []"));
    }

    #[test]
    fn deno_module_stdin_preference_defaults_to_false_and_survives_normalization() {
        let missing: CreateThreadDenoModuleInput = serde_json::from_value(serde_json::json!({
            "name": "default-module",
            "description": "Default",
            "usage": "import \\\"default-module\\\";"
        }))
        .unwrap();
        assert!(!missing.prefer_stdin_execution);

        let opted_in = CreateThreadDenoModuleInput {
            name: "stdin-module".into(),
            description: "Stdin preference".into(),
            usage: "import \\\"stdin-module\\\";".into(),
            prefer_stdin_execution: true,
        };
        let modules = normalize_deno_module_inputs(Some(&CreateThreadSkillsInput {
            guidance: String::new(),
            tools: Vec::new(),
            deno_modules: vec![missing, opted_in],
        }))
        .unwrap();
        assert!(!modules[0].prefer_stdin_execution);
        assert!(modules[1].prefer_stdin_execution);
        assert_eq!(modules[1].state, DenoModuleSetupState::Pending);
    }

    #[test]
    fn bootstrap_keeps_the_compact_invariant_contract() {
        let instruction = build_pedelec_bootstrap_instruction();
        assert!(instruction.contains("For JavaScript or TypeScript execution, use `pedelec-deno`"));
        assert!(instruction.contains("do not fall back to Node.js, Bun, raw Deno, npx"));
        assert!(instruction.contains("Deno Modules are imported from `pedelec-deno` scripts"));
        assert!(instruction.contains("readSpecCommand` / `callCommand"));
        assert!(instruction.contains("a received structured `TOOL_TIMEOUT` is final"));
        assert!(!instruction.contains("authoritative Pedelec workspace root"));
        assert!(!instruction.contains("pedelec-deno module-spec"));
        assert!(!instruction.contains("ambiguous transport failure"));
        assert!(!instruction.contains("PEDELEC_PREPARED"));
    }

    #[test]
    fn persistent_prepare_prompt_uses_brief_acknowledgement_without_exact_token() {
        let prompt = build_persistent_prepare_prompt(None);
        assert!(prompt.contains("[Session Preparation]"));
        assert!(prompt.contains("Do not call tools or modify files."));
        assert!(prompt.contains("A brief acknowledgement is sufficient."));
        assert!(!prompt.contains("PEDELEC_PREPARED"));
    }

    #[test]
    fn antigravity_dynamic_bootstrap_can_omit_the_static_invariant_contract() {
        let instructions = "compact invariant\n\n[Pedelec Host Context]\nWorkspace Path: workspace\n[/Pedelec Host Context]";
        assert_eq!(
            pedelec_host_context_from_persistent_instructions(instructions),
            "[Pedelec Host Context]\nWorkspace Path: workspace\n[/Pedelec Host Context]"
        );
        assert_eq!(
            pedelec_host_context_from_persistent_instructions("context only"),
            "context only"
        );
    }

    #[test]
    fn deno_module_upload_materializes_a_ready_thread_scoped_package() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let mut runtime = runtime_with_thread(&workspace, "thread-deno-upload", ThreadStatus::Idle);
        runtime
            .thread_manager
            .provider_state_mut("thread-deno-upload")
            .unwrap()
            .active_provider_turn_id = None;
        runtime.asset_upload_port = Some(43123);
        runtime.deno_modules.insert(
            "thread-deno-upload".into(),
            vec![DenoModuleState {
                name: "@example/sprite-tools".into(),
                description: "Sprite helpers".into(),
                usage: "import { preview } from \"@example/sprite-tools\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Pending,
            }],
        );

        let envelope = serde_json::json!({
            "version": 1,
            "format": "esm",
            "runtimeSource": "export const preview = () => 'ok';",
            "typesSource": "export declare const preview: () => string;",
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let ticket = runtime
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: "thread-deno-upload".into(),
                module_name: "@example/sprite-tools".into(),
                expected_size_bytes: bytes.len() as u64,
            })
            .unwrap();
        runtime
            .deno_module_upload_tickets
            .get_mut(&ticket.upload_id)
            .unwrap()
            .state = DenoModuleUploadState::Uploading;
        let temporary_path = workspace_tmp_root(&workspace).join("module-envelope.json");
        fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
        fs::write(&temporary_path, bytes).unwrap();

        let completion = runtime
            .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
            .unwrap();
        assert_eq!(completion.module_name, "@example/sprite-tools");
        assert!(completion.ready);
        assert!(runtime
            .deno_module_setup_ready("thread-deno-upload")
            .unwrap());
        let package = workspace_deno_modules_root(&workspace, "thread-deno-upload")
            .join("@example/sprite-tools");
        assert_eq!(
            fs::read_to_string(package.join("index.mjs")).unwrap(),
            "export const preview = () => 'ok';"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(package.join("package.json")).unwrap())
                .unwrap()["exports"]["."]["import"],
            "./index.mjs"
        );
        assert_eq!(
            fs::read_to_string(workspace_deno_import_map_path(
                &workspace,
                "thread-deno-upload"
            ))
            .unwrap(),
            format!(
                "{{\n  \"imports\": {{\n    \"@example/sprite-tools\": \"./modules/@example/sprite-tools/index.mjs\"\n  }}\n}}"
            )
        );
        let import_map = runtime
            .validate_deno_module_runtime_snapshot("thread-deno-upload")
            .unwrap()
            .unwrap();
        assert_eq!(
            import_map,
            PathBuf::from(path_for_external_use(
                &workspace_deno_import_map_path(&workspace, "thread-deno-upload")
                    .canonicalize()
                    .unwrap(),
            ))
        );
        assert!(!import_map.to_string_lossy().contains("entry"));
        let module_entries = fs::read_dir(workspace_deno_modules_root(
            &workspace,
            "thread-deno-upload",
        ))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
        assert_eq!(module_entries, vec![std::ffi::OsString::from("@example")]);
        fs::write(workspace.join("script.ts"), "console.log('ok')").unwrap();
        runtime
            .thread_manager
            .thread_mut("thread-deno-upload")
            .unwrap()
            .status = ThreadStatus::Running;
        runtime
            .thread_manager
            .provider_state_mut("thread-deno-upload")
            .unwrap()
            .active_provider_turn_id = Some("turn-ready".into());
        let intent = runtime
            .prepare_deno_run_intent(file_input(
                "thread-deno-upload",
                "script.ts",
                vec!["--allow-net".into()],
            ))
            .unwrap();
        assert_eq!(intent.import_map_path, Some(import_map.clone()));
        let stdin_intent = runtime
            .prepare_deno_run_intent(stdin_input(
                "thread-deno-upload",
                "import \"@example/sprite-tools\";",
                Vec::new(),
            ))
            .unwrap();
        assert_eq!(stdin_intent.import_map_path, Some(import_map));
        runtime
            .thread_manager
            .thread_mut("thread-deno-upload")
            .unwrap()
            .status = ThreadStatus::Idle;
        runtime
            .thread_manager
            .provider_state_mut("thread-deno-upload")
            .unwrap()
            .active_provider_turn_id = None;
        assert_eq!(
            runtime
                .deno_module_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            DenoModuleUploadState::Completed
        );
        let error = runtime
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: "thread-deno-upload".into(),
                module_name: "@example/sprite-tools".into(),
                expected_size_bytes: 1,
            })
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_ALREADY_READY);
    }

    #[test]
    fn final_module_upload_fails_when_import_map_cannot_be_committed() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let mut runtime =
            runtime_with_thread(&workspace, "thread-deno-map-failure", ThreadStatus::Idle);
        runtime.asset_upload_port = Some(43125);
        runtime.deno_modules.insert(
            "thread-deno-map-failure".into(),
            vec![DenoModuleState {
                name: "sprite-tools".into(),
                description: "Sprite helpers".into(),
                usage: "import \"sprite-tools\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Pending,
            }],
        );
        let import_map_path = workspace_deno_import_map_path(&workspace, "thread-deno-map-failure");
        fs::create_dir_all(&import_map_path).unwrap();

        let envelope = DenoModuleArtifactEnvelope {
            version: 1,
            format: "esm".into(),
            runtime_source: "export const ready = true;".into(),
            types_source: "export declare const ready: boolean;".into(),
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let ticket = runtime
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: "thread-deno-map-failure".into(),
                module_name: "sprite-tools".into(),
                expected_size_bytes: bytes.len() as u64,
            })
            .unwrap();
        runtime
            .deno_module_upload_tickets
            .get_mut(&ticket.upload_id)
            .unwrap()
            .state = DenoModuleUploadState::Uploading;
        let temporary_path = workspace_tmp_root(&workspace).join("map-failure-envelope.json");
        fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
        fs::write(&temporary_path, bytes).unwrap();

        let error = runtime
            .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_MATERIALIZATION_FAILED);
        assert_eq!(
            runtime.deno_modules["thread-deno-map-failure"][0].state,
            DenoModuleSetupState::Failed
        );
        assert_eq!(
            runtime.deno_module_upload_tickets[&ticket.upload_id].state,
            DenoModuleUploadState::Failed
        );
        assert!(
            !workspace_deno_modules_root(&workspace, "thread-deno-map-failure")
                .join("sprite-tools/index.mjs")
                .exists()
        );
    }

    #[test]
    fn missing_module_runtime_snapshot_blocks_execution_and_resume_without_mutating_state() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("script.ts"), "console.log('ok')").unwrap();
        let mut runtime = runtime_with_thread(
            &workspace,
            "thread-deno-runtime-integrity",
            ThreadStatus::Running,
        );
        runtime.deno_modules.insert(
            "thread-deno-runtime-integrity".into(),
            vec![DenoModuleState {
                name: "sprite-tools".into(),
                description: "Sprite helpers".into(),
                usage: "import \"sprite-tools\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Ready,
            }],
        );
        let import_map_path =
            workspace_deno_import_map_path(&workspace, "thread-deno-runtime-integrity");
        let package = workspace_deno_modules_root(&workspace, "thread-deno-runtime-integrity")
            .join("sprite-tools");
        fs::create_dir_all(&package).unwrap();
        fs::write(&package.join("index.mjs"), "export const ready = true;").unwrap();
        fs::write(
            &package.join("index.d.ts"),
            "export declare const ready: boolean;",
        )
        .unwrap();
        fs::write(
            package.join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "sprite-tools",
                "type": "module",
                "types": "./index.d.ts",
                "exports": {
                    ".": {
                        "types": "./index.d.ts",
                        "import": "./index.mjs"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(import_map_path.parent().unwrap()).unwrap();
        fs::write(
            &import_map_path,
            serde_json::to_vec_pretty(&DenoModuleImportMap {
                imports: BTreeMap::from([(
                    "sprite-tools".into(),
                    "./modules/sprite-tools/index.mjs".into(),
                )]),
            })
            .unwrap(),
        )
        .unwrap();

        let intent = runtime
            .prepare_deno_run_intent(file_input(
                "thread-deno-runtime-integrity",
                "script.ts",
                Vec::new(),
            ))
            .unwrap();
        assert_eq!(
            intent.import_map_path,
            Some(PathBuf::from(path_for_external_use(
                &import_map_path.canonicalize().unwrap(),
            )))
        );

        fs::remove_file(&import_map_path).unwrap();
        let error = runtime
            .prepare_deno_run_intent(file_input(
                "thread-deno-runtime-integrity",
                "script.ts",
                Vec::new(),
            ))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_SETUP_FAILED);
        let error = runtime
            .build_persistent_session_intent("thread-deno-runtime-integrity")
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_SETUP_FAILED);

        runtime
            .thread_manager
            .thread_mut("thread-deno-runtime-integrity")
            .unwrap()
            .status = ThreadStatus::Ended;
        let error = runtime
            .resume_thread(ResumeThreadInput {
                thread_id: "thread-deno-runtime-integrity".into(),
            })
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_SETUP_FAILED);
        assert_eq!(
            runtime.thread_status("thread-deno-runtime-integrity"),
            Some(ThreadStatus::Ended)
        );
    }

    #[cfg(unix)]
    #[test]
    fn module_snapshot_rejects_symlinked_scoped_package_parent() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let mut runtime = runtime_with_thread(
            &workspace,
            "thread-deno-symlinked-module",
            ThreadStatus::Running,
        );
        runtime.deno_modules.insert(
            "thread-deno-symlinked-module".into(),
            vec![DenoModuleState {
                name: "@example/sprite-tools".into(),
                description: "Sprite helpers".into(),
                usage: "import \"@example/sprite-tools\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Ready,
            }],
        );

        let modules_root = workspace_deno_modules_root(&workspace, "thread-deno-symlinked-module");
        fs::create_dir_all(&modules_root).unwrap();
        let outside_scope = temp.path().join("outside-scope");
        fs::create_dir_all(&outside_scope).unwrap();
        symlink(&outside_scope, modules_root.join("@example")).unwrap();
        let import_map_path =
            workspace_deno_import_map_path(&workspace, "thread-deno-symlinked-module");
        fs::create_dir_all(import_map_path.parent().unwrap()).unwrap();
        fs::write(
            import_map_path,
            serde_json::to_vec_pretty(&DenoModuleImportMap {
                imports: BTreeMap::from([(
                    "@example/sprite-tools".into(),
                    "./modules/@example/sprite-tools/index.mjs".into(),
                )]),
            })
            .unwrap(),
        )
        .unwrap();

        let error = runtime
            .validate_deno_module_runtime_snapshot("thread-deno-symlinked-module")
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_MODULE_SETUP_FAILED);
    }

    #[test]
    fn module_snapshots_are_isolated_for_threads_sharing_one_custom_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("managed");
        let workspace = temp.path().join("shared-workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&managed_root),
            settings_file_path: Some(temp.path().join("settings.json")),
            asset_upload_port: Some(43124),
            ..CoreRuntime::default()
        };
        let input = |workspace_id: &str| CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            model: None,
            effort: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: String::new(),
                tools: Vec::new(),
                deno_modules: vec![CreateThreadDenoModuleInput {
                    name: "sprite-tools".into(),
                    description: "Sprite helpers".into(),
                    usage: "import { preview } from \"sprite-tools\";".into(),
                    prefer_stdin_execution: false,
                }],
            }),
            workspace_id: Some(workspace_id.to_string()),
        };
        let workspace_id = runtime
            .open_workspace(
                OpenWorkspaceInput {
                    path: workspace.clone(),
                },
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .workspace_id;
        let thread_a = runtime
            .create_sdk_thread(
                input(&workspace_id),
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .thread_id;
        let thread_b = runtime
            .create_sdk_thread(
                input(&workspace_id),
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .thread_id;
        assert_ne!(thread_a, thread_b);

        for (thread_id, source) in [
            (&thread_a, "export const preview = () => 'A';"),
            (&thread_b, "export const preview = () => 'B';"),
        ] {
            runtime.deno_modules.get_mut(thread_id).unwrap()[0].state =
                DenoModuleSetupState::Pending;
            let envelope = serde_json::json!({
                "version": 1,
                "format": "esm",
                "runtimeSource": source,
                "typesSource": "export declare const preview: () => string;",
            });
            let bytes = serde_json::to_vec(&envelope).unwrap();
            let ticket = runtime
                .create_deno_module_upload(CreateDenoModuleUploadInput {
                    thread_id: thread_id.clone(),
                    module_name: "sprite-tools".into(),
                    expected_size_bytes: bytes.len() as u64,
                })
                .unwrap();
            runtime
                .deno_module_upload_tickets
                .get_mut(&ticket.upload_id)
                .unwrap()
                .state = DenoModuleUploadState::Uploading;
            let temporary_path =
                workspace_tmp_root(&workspace).join(format!("{thread_id}-envelope.json"));
            fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
            fs::write(&temporary_path, bytes).unwrap();
            runtime
                .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
                .unwrap();
        }

        let package_a = workspace_deno_modules_root(&workspace, &thread_a).join("sprite-tools");
        let package_b = workspace_deno_modules_root(&workspace, &thread_b).join("sprite-tools");
        assert_eq!(
            fs::read_to_string(package_a.join("index.mjs")).unwrap(),
            "export const preview = () => 'A';"
        );
        assert_eq!(
            fs::read_to_string(package_b.join("index.mjs")).unwrap(),
            "export const preview = () => 'B';"
        );
        assert_ne!(
            workspace_deno_import_map_path(&workspace, &thread_a),
            workspace_deno_import_map_path(&workspace, &thread_b)
        );
        assert!(workspace_deno_import_map_path(&workspace, &thread_a).is_file());
        assert!(workspace_deno_import_map_path(&workspace, &thread_b).is_file());
    }

    fn custom_workspace_runtime(temp: &tempfile::TempDir) -> CoreRuntime {
        CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            settings_file_path: Some(temp.path().join("settings.json")),
            asset_upload_port: Some(43124),
            ..CoreRuntime::default()
        }
    }

    fn sprite_tools_thread_input(workspace_id: &str) -> CreateThreadInput {
        CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            model: None,
            effort: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: String::new(),
                tools: Vec::new(),
                deno_modules: vec![CreateThreadDenoModuleInput {
                    name: "sprite-tools".into(),
                    description: "Sprite helpers".into(),
                    usage: "import { preview } from \"sprite-tools\";".into(),
                    prefer_stdin_execution: false,
                }],
            }),
            workspace_id: Some(workspace_id.to_string()),
        }
    }

    fn materialize_sprite_tools(
        runtime: &mut CoreRuntime,
        workspace: &Path,
        thread_id: &str,
        runtime_source: &str,
    ) {
        let envelope = serde_json::json!({
            "version": 1,
            "format": "esm",
            "runtimeSource": runtime_source,
            "typesSource": "export declare const preview: () => string;",
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let ticket = runtime
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: thread_id.to_string(),
                module_name: "sprite-tools".into(),
                expected_size_bytes: bytes.len() as u64,
            })
            .unwrap();
        runtime
            .deno_module_upload_tickets
            .get_mut(&ticket.upload_id)
            .unwrap()
            .state = DenoModuleUploadState::Uploading;
        let temporary_path =
            workspace_tmp_root(workspace).join(format!("{thread_id}-envelope.json"));
        fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
        fs::write(&temporary_path, bytes).unwrap();
        runtime
            .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
            .unwrap();
    }

    #[test]
    fn custom_workspace_without_deno_thread_root_keeps_the_first_short_id() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("custom-workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("t000001"), "user file, not a Deno root").unwrap();
        let mut runtime = custom_workspace_runtime(&temp);
        let workspace_id = runtime
            .open_workspace(
                OpenWorkspaceInput {
                    path: workspace.clone(),
                },
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .workspace_id;
        let thread_id = runtime
            .create_sdk_thread(
                sprite_tools_thread_input(&workspace_id),
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .thread_id;
        assert_eq!(thread_id, "t000001");
        assert!(!workspace_deno_thread_root_occupied(&workspace, "t000001"));
    }

    #[test]
    fn restarted_core_skips_stale_custom_workspace_deno_thread_roots() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("custom-workspace");
        let stale_source = "export const preview = () => 'stale-process';";
        let first_thread_id = {
            let mut runtime = custom_workspace_runtime(&temp);
            let workspace_id = runtime
                .open_workspace(
                    OpenWorkspaceInput {
                        path: workspace.clone(),
                    },
                    "https://app.example.test",
                    Some("0.3.3"),
                )
                .unwrap()
                .workspace_id;
            let thread_id = runtime
                .create_sdk_thread(
                    sprite_tools_thread_input(&workspace_id),
                    "https://app.example.test",
                    Some("0.3.3"),
                )
                .unwrap()
                .thread_id;
            assert_eq!(thread_id, "t000001");
            materialize_sprite_tools(&mut runtime, &workspace, &thread_id, stale_source);
            thread_id
        };

        let stale_package =
            workspace_deno_modules_root(&workspace, &first_thread_id).join("sprite-tools");
        let stale_runtime = fs::read_to_string(stale_package.join("index.mjs")).unwrap();
        let stale_types = fs::read_to_string(stale_package.join("index.d.ts")).unwrap();
        let stale_import_map =
            fs::read(workspace_deno_import_map_path(&workspace, &first_thread_id)).unwrap();
        assert_eq!(stale_runtime, stale_source);
        assert!(workspace_deno_thread_root_occupied(
            &workspace,
            &first_thread_id
        ));

        let mut restarted = custom_workspace_runtime(&temp);
        let workspace_id = restarted
            .open_workspace(
                OpenWorkspaceInput {
                    path: workspace.clone(),
                },
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .workspace_id;
        let second_thread_id = restarted
            .create_sdk_thread(
                sprite_tools_thread_input(&workspace_id),
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .thread_id;
        assert_ne!(second_thread_id, first_thread_id);
        assert_eq!(second_thread_id, "t000002");

        materialize_sprite_tools(
            &mut restarted,
            &workspace,
            &second_thread_id,
            "export const preview = () => 'fresh-process';",
        );

        assert_eq!(
            fs::read_to_string(stale_package.join("index.mjs")).unwrap(),
            stale_runtime
        );
        assert_eq!(
            fs::read_to_string(stale_package.join("index.d.ts")).unwrap(),
            stale_types
        );
        assert_eq!(
            fs::read(workspace_deno_import_map_path(&workspace, &first_thread_id)).unwrap(),
            stale_import_map
        );
        assert!(workspace_deno_thread_root(&workspace, &first_thread_id).is_dir());
        assert_eq!(
            fs::read_to_string(
                workspace_deno_modules_root(&workspace, &second_thread_id)
                    .join("sprite-tools")
                    .join("index.mjs")
            )
            .unwrap(),
            "export const preview = () => 'fresh-process';"
        );
    }

    #[test]
    fn deno_module_setup_blocks_admission_until_every_module_is_ready() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let mut runtime =
            runtime_with_thread(&workspace, "thread-deno-pending", ThreadStatus::Running);
        runtime.deno_modules.insert(
            "thread-deno-pending".into(),
            vec![DenoModuleState {
                name: "pending-module".into(),
                description: "Pending".into(),
                usage: "import \"pending-module\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Pending,
            }],
        );
        fs::write(workspace.join("script.ts"), "console.log('ok')").unwrap();

        let send_error = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: "thread-deno-pending".into(),
                message: "hello".into(),
                operation_id: Some("op-pending".into()),
            })
            .unwrap_err();
        assert_eq!(send_error.code, error_codes::DENO_MODULE_SETUP_INCOMPLETE);
        let prepare_error = runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: "thread-deno-pending".into(),
                operation_id: Some("op-prepare-pending".into()),
            })
            .unwrap_err();
        assert_eq!(
            prepare_error.code,
            error_codes::DENO_MODULE_SETUP_INCOMPLETE
        );
        let deno_error = runtime
            .prepare_deno_run_intent(file_input("thread-deno-pending", "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(deno_error.code, error_codes::DENO_MODULE_SETUP_INCOMPLETE);
    }

    #[test]
    fn abort_session_setup_removes_managed_thread_and_preserves_custom_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("managed");
        fs::create_dir_all(&managed_root).unwrap();
        let settings_path = temp.path().join("settings.json");
        let mut managed_runtime = CoreRuntime {
            settings_file_path: Some(settings_path),
            workspace_manager: WorkspaceManager::with_workspace_root(&managed_root),
            ..CoreRuntime::default()
        };
        let input = CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            model: None,
            effort: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: String::new(),
                tools: Vec::new(),
                deno_modules: vec![CreateThreadDenoModuleInput {
                    name: "managed-module".into(),
                    description: "Managed".into(),
                    usage: "import \"managed-module\";".into(),
                    prefer_stdin_execution: false,
                }],
            }),
            workspace_id: None,
        };
        let managed_thread = managed_runtime
            .create_sdk_thread(input, "https://app.example.test", Some("0.3.3"))
            .unwrap()
            .thread_id;
        let managed_workspace = managed_runtime
            .thread_workspace_path(&managed_thread)
            .unwrap();
        managed_runtime
            .abort_session_setup(AbortSessionSetupInput {
                thread_id: managed_thread.clone(),
            })
            .unwrap();
        assert!(!managed_workspace.exists());
        assert!(managed_runtime
            .thread_manager
            .thread(&managed_thread)
            .is_err());
        assert!(!managed_runtime.deno_modules.contains_key(&managed_thread));
        managed_runtime
            .abort_session_setup(AbortSessionSetupInput {
                thread_id: managed_thread,
            })
            .unwrap();

        let custom_root = temp.path().join("custom");
        let mut custom_runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("custom-settings.json")),
            workspace_manager: WorkspaceManager::with_workspace_root(&managed_root),
            ..CoreRuntime::default()
        };
        let custom_workspace_id = custom_runtime
            .open_workspace(
                OpenWorkspaceInput {
                    path: custom_root.clone(),
                },
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .workspace_id;
        let custom_thread = custom_runtime
            .create_sdk_thread(
                CreateThreadInput {
                    provider: ProviderCode::Codex,
                    effort_level: Some(EffortLevel::Default),
                    model: None,
                    effort: None,
                    skills: Some(CreateThreadSkillsInput {
                        guidance: String::new(),
                        tools: Vec::new(),
                        deno_modules: vec![CreateThreadDenoModuleInput {
                            name: "custom-module".into(),
                            description: "Custom".into(),
                            usage: "import \"custom-module\";".into(),
                            prefer_stdin_execution: false,
                        }],
                    }),
                    workspace_id: Some(custom_workspace_id),
                },
                "https://app.example.test",
                Some("0.3.3"),
            )
            .unwrap()
            .thread_id;
        let custom_sentinel = custom_root.join("keep-me.txt");
        fs::write(&custom_sentinel, "application data").unwrap();
        let custom_deno_root = workspace_deno_thread_root(&custom_root, &custom_thread);
        fs::create_dir_all(custom_deno_root.join("modules")).unwrap();
        custom_runtime
            .abort_session_setup(AbortSessionSetupInput {
                thread_id: custom_thread.clone(),
            })
            .unwrap();
        assert!(custom_root.exists());
        assert!(custom_sentinel.exists());
        assert!(!custom_deno_root.exists());
        assert!(custom_runtime
            .thread_manager
            .thread(&custom_thread)
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn deno_admission_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("authoritative");
        let outside = temp.path().join("outside.ts");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&outside, "console.log('outside')").unwrap();
        symlink(&outside, workspace.join("escape.ts")).unwrap();
        let runtime = runtime_with_thread(&workspace, "thread-deno-symlink", ThreadStatus::Running);

        let error = runtime
            .prepare_deno_run_intent(file_input("thread-deno-symlink", "escape.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ENTRYPOINT_INVALID);
    }

    #[test]
    fn stdin_admission_uses_thread_workspace_without_resolving_an_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("authoritative");
        fs::create_dir_all(&workspace).unwrap();
        let runtime = runtime_with_thread(&workspace, "thread-deno-stdin", ThreadStatus::Running);

        let intent = runtime
            .prepare_deno_run_intent(stdin_input(
                "thread-deno-stdin",
                "console.log('stdin')",
                vec!["arg1".into()],
            ))
            .unwrap();

        assert_eq!(intent.workspace_path, workspace.canonicalize().unwrap());
        assert_eq!(
            intent.target,
            DenoExecutionTarget::StdinSource {
                source: "console.log('stdin')".into()
            }
        );
        assert_eq!(intent.args, vec!["arg1"]);
    }
}

#[cfg(test)]
#[path = "../../../tauri/src/pedelec_core/tests/mod.rs"]
mod tests;
