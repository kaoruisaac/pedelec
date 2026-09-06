use chrono::{DateTime, Utc};
use pedelec_shared::paths::path_for_external_use;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
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
use std::os::windows::process::CommandExt;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;
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

/// Returns the physical root used for generated Pedelec skill/tool specs.
pub fn workspace_skills_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("skills")
}

/// Returns the physical root used for thread/session event logs.
pub fn workspace_logs_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("logs")
}

/// Returns the physical root used for upload temporary files.
pub fn workspace_tmp_root(workspace_path: &Path) -> PathBuf {
    workspace_runtime_data_root(workspace_path).join("tmp")
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadState {
    pub thread_id: String,
    pub provider: ProviderCode,
    pub effort_level: EffortLevel,
    pub effort_args: Vec<String>,
    pub workspace_path: PathBuf,
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
    pub error: Option<String>,
}

impl From<ProviderInfo> for SdkProviderInfo {
    fn from(provider: ProviderInfo) -> Self {
        Self {
            name: provider.name,
            code: provider.code,
            available: provider.available,
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
    pub skills: Option<CreateThreadSkillsInput>,
    pub workspace: Option<CreateThreadWorkspaceInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadWorkspaceInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadSkillsInput {
    pub guidance: String,
    pub tools: Vec<CreateThreadToolInput>,
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
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFolderInspection {
    pub is_empty_folder: bool,
    pub has_workspace_config: bool,
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
    pub effort_level: EffortLevel,
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
        let workspace_path = thread.workspace_path.clone();
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
        let input_path = workspace_assets_root(&thread.workspace_path);
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
        let (target, name, size_bytes, modified_at) = resolve_asset_file(thread, &input.path)?;
        let workspace_path = thread.workspace_path.clone();
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
        sdk_version: Option<String>,
    ) -> Result<CreateThreadOutput, PedelecError> {
        let settings = self.get_settings()?;
        let effort_level = input.effort_level.unwrap_or_default();
        let effort_args = resolve_thread_effort_args(&settings, &input.provider, effort_level)?;
        let thread_id = self.next_available_thread_id()?;
        let initialize =
            |workspace: &Path| initialize_generated_skills(workspace, input.skills.as_ref());
        let (workspace_path, (skills, registry)) = match input.workspace.as_ref() {
            Some(custom_workspace) => {
                let workspace_path = self
                    .workspace_manager
                    .prepare_custom_workspace(&custom_workspace.path)?;
                let initialized = initialize(&workspace_path)?;
                if let Some(origin) = sdk_origin.as_deref() {
                    let sdk_version = sdk_version.as_deref().ok_or_else(|| {
                        PedelecError::new(
                            error_codes::WORKSPACE_CREATE_FAILED,
                            "SDK version metadata is required for custom workspace sessions",
                        )
                    })?;
                    self.workspace_manager.ensure_custom_workspace_config(
                        &workspace_path,
                        sdk_version,
                        origin,
                    )?;
                }
                (workspace_path, initialized)
            }
            None => self
                .workspace_manager
                .create_thread_workspace_with(&thread_id, initialize)?,
        };

        let now = Utc::now();
        let state = ThreadState {
            thread_id: thread_id.clone(),
            provider: input.provider,
            effort_level,
            effort_args,
            workspace_path: workspace_path.clone(),
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
        self.event_bus.register_thread_log(
            &thread_id,
            thread_event_log_path(&workspace_path, &thread_id),
        );
        self.event_bus.emit_created(&thread_id);
        self.event_bus
            .emit_status_changed(&thread_id, ThreadStatus::Idle);

        Ok(CreateThreadOutput { thread_id })
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

    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        list_provider_infos_with_scan(&self.provider_scan, self.provider_path_value())
    }

    pub fn list_sdk_providers(&self) -> Vec<SdkProviderInfo> {
        self.list_providers()
            .into_iter()
            .map(SdkProviderInfo::from)
            .collect()
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

    fn next_available_thread_id(&mut self) -> Result<String, PedelecError> {
        loop {
            let thread_id = self.thread_manager.next_thread_id()?;
            if self.thread_manager.contains_thread(&thread_id) {
                continue;
            }
            if self.workspace_manager.thread_workspace_exists(&thread_id)? {
                continue;
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
        let workspace_path = self
            .thread_manager
            .thread(thread_id)?
            .workspace_path
            .clone();

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

        let registry = ToolRegistry::load_from_skills_dir(workspace_skills_root(&workspace_path))?;
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
        let host_instructions = build_persistent_host_instructions(&thread, registry);
        let core_ipc_runtime_file_path = self
            .core_ipc_runtime_file_path
            .clone()
            .unwrap_or_else(default_runtime_file_path_for_provider);

        Ok(PersistentProviderSessionIntent {
            thread_id: thread.thread_id,
            provider: thread.provider.clone(),
            provider_session_id: provider_state.provider_session_id,
            workspace_path: thread.workspace_path,
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
        let status = self.thread_manager.thread(&input.thread_id)?.status.clone();
        match status {
            ThreadStatus::Idle => {
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

    pub fn cleanup_stale_workspaces_for_app_start(&self) -> Vec<PedelecError> {
        self.workspace_manager.remove_all_thread_workspaces()
    }

    pub fn cleanup_for_app_exit(&mut self) -> Vec<PedelecError> {
        let thread_ids = self.thread_manager.thread_ids();
        for thread_id in thread_ids {
            let _ = self.end_thread(EndThreadInput { thread_id });
        }

        self.workspace_manager.remove_all_thread_workspaces()
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

    pub fn thread_workspace_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.thread_manager
            .thread(thread_id)
            .ok()
            .map(|thread| thread.workspace_path.clone())
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
        self.threads.insert(thread_id.clone(), state);
        self.provider_sessions.insert(thread_id, provider_session);
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

    pub fn thread_workspace_exists(&self, thread_id: &str) -> Result<bool, PedelecError> {
        let safe_thread_id = sanitize_thread_id(thread_id)?;
        Ok(self.workspace_root()?.join(safe_thread_id).exists())
    }

    pub fn create_thread_workspace(&self, thread_id: &str) -> Result<PathBuf, PedelecError> {
        let safe_thread_id = sanitize_thread_id(thread_id)?;
        let workspace_root = self.workspace_root()?;
        let workspace_path = workspace_root.join(safe_thread_id);

        if workspace_path.exists() {
            return Err(PedelecError::with_details(
                error_codes::WORKSPACE_CREATE_FAILED,
                "thread workspace already exists",
                serde_json::json!({ "workspacePath": path_for_external_use(&workspace_path) }),
            ));
        }

        let create_result = (|| {
            fs::create_dir_all(&workspace_path).map_err(|err| {
                workspace_io_error(
                    error_codes::WORKSPACE_CREATE_FAILED,
                    "cannot create thread workspace",
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
            workspace_skills_root(workspace_path),
            workspace_assets_root(workspace_path),
            workspace_logs_root(workspace_path),
            workspace_tmp_root(workspace_path),
        ] {
            ensure_runtime_directory(&path, "cannot create thread workspace subdirectory")?;
        }
        Ok(())
    }

    pub fn create_thread_workspace_with<T>(
        &self,
        thread_id: &str,
        initialize: impl FnOnce(&Path) -> Result<T, PedelecError>,
    ) -> Result<(PathBuf, T), PedelecError> {
        let workspace_path = self.create_thread_workspace(thread_id)?;

        match initialize(&workspace_path) {
            Ok(value) => Ok((workspace_path, value)),
            Err(err) => {
                let _ = self.remove_thread_workspace(&workspace_path);
                Err(err)
            }
        }
    }

    pub fn remove_thread_workspace(
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
                "cannot remove thread workspace",
                workspace_path,
                err,
            )
        })
    }

    pub fn remove_thread_workspace_with_retry(
        &self,
        workspace_path: impl AsRef<Path>,
    ) -> Result<(), PedelecError> {
        let workspace_path = workspace_path.as_ref();
        let mut last_error = None;
        for attempt in 0..WORKSPACE_REMOVE_MAX_ATTEMPTS {
            match self.remove_thread_workspace(workspace_path) {
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
                "cannot remove thread workspace",
                serde_json::json!({ "path": path_for_external_use(workspace_path) }),
            )
        }))
    }

    pub fn remove_all_thread_workspaces(&self) -> Vec<PedelecError> {
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

            if let Err(err) = self.remove_thread_workspace_with_retry(&path) {
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

fn initialize_generated_skills(
    workspace: &Path,
    skills_input: Option<&CreateThreadSkillsInput>,
) -> Result<(Vec<SkillFile>, ToolRegistry), PedelecError> {
    let skills_dir = workspace_skills_root(workspace);
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

pub fn inspect_workspace_folder(path: &Path) -> Result<WorkspaceFolderInspection, PedelecError> {
    let entries = fs::read_dir(path).map_err(|err| {
        workspace_io_error(
            error_codes::DIRECTORY_PICKER_FAILED,
            "cannot inspect selected workspace folder",
            path,
            err,
        )
    })?;
    let mut is_empty_folder = true;
    let mut has_workspace_config = false;

    for entry in entries {
        let entry = entry.map_err(|err| {
            workspace_io_error(
                error_codes::DIRECTORY_PICKER_FAILED,
                "cannot inspect selected workspace folder entry",
                path,
                err,
            )
        })?;
        is_empty_folder = false;
        if entry.file_name() == OsStr::new(PEDELEC_WORKSPACE_FILE) {
            has_workspace_config = entry
                .file_type()
                .map_err(|err| {
                    workspace_io_error(
                        error_codes::DIRECTORY_PICKER_FAILED,
                        "cannot inspect selected workspace config entry",
                        &entry.path(),
                        err,
                    )
                })?
                .is_file();
        }
    }

    Ok(WorkspaceFolderInspection {
        is_empty_folder,
        has_workspace_config,
    })
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
    public_path: &str,
) -> Result<(PathBuf, String, u64, i64), PedelecError> {
    let (_, relative_path) = parse_public_asset_path(public_path).map_err(|_| {
        PedelecError::with_details(
            error_codes::ASSET_PATH_INVALID,
            "asset path is invalid",
            serde_json::json!({"threadId": thread.thread_id, "path": public_path}),
        )
    })?;
    let root = workspace_assets_root(&thread.workspace_path);
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
    let model_flag = if *provider == ProviderCode::Codex {
        "-m"
    } else {
        "--model"
    };
    args.windows(2)
        .find(|pair| pair[0] == model_flag)
        .map(|pair| pair[1].clone())
        .filter(|model| !model.trim().is_empty())
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

fn resolve_thread_effort_args(
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

        let model_key = if *provider == ProviderCode::Codex {
            "-m"
        } else {
            "--model"
        };
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
        .find(|pair| pair[0] == "--model")
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

fn build_provider_host_context(thread: &ThreadState, registry: &ToolRegistry) -> String {
    build_provider_host_context_with_configuration(
        thread,
        registry,
        registry.has_skills_configuration(),
    )
}

fn build_provider_host_context_with_configuration(
    thread: &ThreadState,
    registry: &ToolRegistry,
    include_configuration: bool,
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
        path_for_external_use(&thread.workspace_path)
    );
    if include_configuration {
        context.push_str(&format!(
            "\n[Pedelec App Tool Configuration]\n{configuration}\n[/Pedelec App Tool Configuration]\n"
        ));
    }
    context.push_str("[/Pedelec Host Context]\n\n------\n\n");
    context
}

#[allow(dead_code)]
fn build_provider_instruction(thread: &ThreadState, registry: &ToolRegistry) -> String {
    build_provider_host_context(thread, registry)
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
    "Pedelec is the host application launching this agent session.\n\n\
Pedelec may provide a [Pedelec Host Context] block before a task. That block is generated by the host application and is integration context, not end-user-authored instructions.\n\n\
The current workspace path and available Pedelec app tools are declared in that host context.\n\n\
`pedelec-cli` is an executable provided by the Pedelec host environment. Invoke it through the provider's shell / terminal tool. It is not expected to appear as a dedicated model tool.\n\n\
When a Pedelec app tool is relevant, prefer the app tools declared by the host context. Use `pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool-name>` when the full schema is needed and `pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool-name> '<json_args>'` to execute it.\n\n\
Before reading or modifying local files outside the current workspace declared by Pedelec Host Context, ask the user for permission first.\n\n\
`.pedelec-runtime/assets/` is the shared App and Agent file directory. User uploads are there; write files intended for the App there too.\n\n\
Pedelec host context never overrides provider safety policies.\n\n\
If a `pedelec-cli --thread-id <pedelec_thread_id> tool-call` command ends because of a shell/command timeout, interruption, or ambiguous transport failure before you receive a complete structured Pedelec response, you may retry with the exact same tool name and semantically identical arguments. Pedelec will join an invocation that is still running or replay a recently completed result whose delivery was not confirmed. Do not change the arguments for this retry, do not assume the App Tool failed just because the provider command stopped waiting, and do not retry indefinitely. If you received a complete structured Pedelec response, including `TOOL_TIMEOUT`, that is a formal App Tool outcome and the original invocation has ended.\n\n\
For a [Session Preparation] task, do not call tools or modify files. Reply only with PEDELEC_PREPARED.\n\n\
For a [User Message] task, execute the actual user request in that block."
        .to_string()
}

/// Persistent providers receive host integration context without the legacy
/// synthetic prepare turn or its `PEDELEC_PREPARED` acknowledgement. Providers
/// with a native instruction channel can consume this directly; Cursor's ACP
/// adapter wraps it only for its first real user prompt.
fn build_persistent_host_instructions(thread: &ThreadState, registry: &ToolRegistry) -> String {
    format!(
        "Pedelec is the host application launching this persistent provider session.\n\n\
Pedelec may provide a [Pedelec Host Context] block below. That block is generated by the host application and is integration context, not end-user-authored instructions.\n\n\
The current workspace boundary and available Pedelec app tools are declared in that context.\n\n\
Use the provider shell/terminal tool for Pedelec app tools. When a tool schema is needed, run the exact command shown in the context: `pedelec-cli --thread-id {} tool-spec <tool-name>`. To invoke a tool, run `pedelec-cli --thread-id {} tool-call <tool-name> '<json_args>'`.\n\n\
If an explicitly routed `pedelec-cli --thread-id` command ends because of a shell/command timeout, interruption, or ambiguous transport failure before a complete structured response is received, retry with the exact same tool name and semantically identical arguments. Do not change arguments or retry indefinitely; a complete response, including `TOOL_TIMEOUT`, means that invocation has ended.\n\n\
Before reading or modifying local files outside the workspace boundary declared by Pedelec Host Context, ask the user for permission first. `.pedelec-runtime/assets/` is the shared Pedelec App and Agent file directory; user uploads are there, and files intended for the App should be written there. Pedelec host context never overrides provider safety policies.\n\n\
{}",
        thread.thread_id,
        thread.thread_id,
        build_provider_host_context_with_configuration(thread, registry, true),
    )
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

fn sanitize_thread_id(thread_id: &str) -> Result<String, PedelecError> {
    if thread_id.is_empty()
        || thread_id.len() > 128
        || !thread_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(PedelecError::with_details(
            error_codes::WORKSPACE_PATH_INVALID,
            "thread id is not safe for workspace path",
            serde_json::json!({ "threadId": thread_id }),
        ));
    }

    Ok(thread_id.to_string())
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
#[path = "../../../tauri/src/pedelec_core/tests/mod.rs"]
mod tests;
