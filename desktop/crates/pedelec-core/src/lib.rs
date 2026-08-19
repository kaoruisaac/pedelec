use chrono::{DateTime, Utc};
use pedelec_shared::paths::path_for_external_use;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::process::Stdio;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
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
const CODEX_SKILLS_INCLUDE_INSTRUCTIONS_CONFIG: &str = "skills.include_instructions=false";
const OPENCODE_PERMISSION_ENV: &str = "OPENCODE_PERMISSION";
const OPENCODE_CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";
const PEDELEC_OPENCODE_AGENT: &str = "pedelec-runtime";
const PEDELEC_ANTIGRAVITY_AGENT_DIR: &str = ".agents/agents/pedelec-runtime";
const PEDELEC_ANTIGRAVITY_AGENT_FILE: &str = "agent.md";
const ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS: usize = 20_000;
const SANDBOX_SUBDIRS: [&str; 4] = ["skills", "assets", "logs", "tmp"];
const SANDBOX_CONFIG_FILE: &str = ".pedelec-sandbox.json";
const TOOL_TIMEOUT_OVERRIDE_FIELD: &str = "timeoutMs";
pub const TOOL_RESULT_REPLAY_WINDOW: Duration = Duration::from_secs(10);
pub const TOOL_RESULT_REPLAY_MAX_ENTRIES: usize = 256;
const THREAD_ID_BASE36_MIN_WIDTH: usize = 6;
const THREAD_ID_BASE36_MAX_WIDTH: usize = 7;
const THREAD_ID_MAX_COUNTER: u64 = 78_364_164_095;
pub const MAX_ASSET_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
const ASSET_UPLOAD_TICKET_SECONDS: i64 = 5 * 60;
const MAX_PROVIDER_STDERR_BYTES: usize = 64 * 1024;
const MAX_PREPARE_ASSISTANT_OUTPUT_BYTES: usize = 64 * 1024;
const SANDBOX_REMOVE_MAX_ATTEMPTS: usize = 10;
const SANDBOX_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(50);

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
pub struct SandboxAsset {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListAssetsOutput {
    pub assets: Vec<SandboxAsset>,
}

#[derive(Debug, Clone)]
pub struct AssetUploadTicket {
    pub thread_id: String,
    pub sandbox_path: PathBuf,
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
    pub sandbox_path: PathBuf,
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
    pub sandbox_path: PathBuf,
    pub skills: Vec<SkillFile>,
    pub status: ThreadStatus,
    pub process_id: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing)]
    pub sdk_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAdapterState {
    pub provider_session_id: Option<String>,
    pub last_process_id: Option<u32>,
    #[serde(default)]
    pub has_user_message: bool,
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
        status: ThreadStatus,
    },
    RawStdout {
        seq: u64,
        thread_id: String,
        text: String,
    },
    RawStderr {
        seq: u64,
        thread_id: String,
        text: String,
    },
    AssistantMessage {
        seq: u64,
        thread_id: String,
        text: String,
    },
    ToolCall {
        seq: u64,
        thread_id: String,
        request_id: String,
        tool_name: String,
        args: Value,
    },
    ToolResult {
        seq: u64,
        thread_id: String,
        request_id: String,
        tool_name: String,
        result: Value,
    },
    ProviderCommandStarted {
        seq: u64,
        thread_id: String,
        process_id: u32,
        program: String,
        args: Vec<String>,
        cwd: String,
        prompt: String,
    },
    ProviderSessionIdUpdated {
        seq: u64,
        thread_id: String,
        provider_session_id: String,
    },
    Done {
        seq: u64,
        thread_id: String,
    },
    Error {
        seq: u64,
        thread_id: String,
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
            | ThreadEvent::RawStdout { seq, .. }
            | ThreadEvent::RawStderr { seq, .. }
            | ThreadEvent::AssistantMessage { seq, .. }
            | ThreadEvent::ToolCall { seq, .. }
            | ThreadEvent::ToolResult { seq, .. }
            | ThreadEvent::ProviderCommandStarted { seq, .. }
            | ThreadEvent::ProviderSessionIdUpdated { seq, .. }
            | ThreadEvent::Done { seq, .. }
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
    pub sandbox: Option<CreateThreadSandboxInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadSandboxInput {
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
pub struct SandboxFolderInspection {
    pub is_empty_folder: bool,
    pub has_sandbox_config: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendTextInput {
    pub thread_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendTextOutput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareThreadInput {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareThreadOutput {
    pub thread_id: String,
    pub prepared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_prepared: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EndThreadInput {
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
pub struct PendingToolRequest {
    pub request_id: String,
    pub thread_id: String,
    pub tool_name: String,
    pub args: Value,
    pub created_at: DateTime<Utc>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub supports_json_events: bool,
    pub supports_resume_by_session_id: bool,
    pub supports_user_supplied_session_id: bool,
    pub supports_provider_generated_session_id_parse: bool,
    pub supports_resume_last_session: bool,
}

#[derive(Debug, Clone)]
pub struct SendTextStart {
    pub output: SendTextOutput,
    pub command: CommandSpec,
}

#[derive(Debug, Clone)]
pub struct PrepareThreadStart {
    pub output: PrepareThreadOutput,
    pub command: Option<CommandSpec>,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub prompt: String,
    pub stdin: String,
}

#[derive(Debug, Clone)]
pub struct RunPromptProviderContext {
    pub thread: ThreadState,
    pub tool_registry: ToolRegistry,
    pub provider_state: ProviderAdapterState,
    include_fallback_bootstrap: bool,
    pub settings: PedelecSettings,
    pub core_ipc_endpoint: String,
    pub core_ipc_runtime_file_path: PathBuf,
    pub provider_resolved_path: Option<OsString>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunningProviderProcess {
    process_id: u32,
    child: Arc<Mutex<Option<Child>>>,
    termination: Arc<ProviderProcessTermination>,
    purpose: RunningProviderProcessPurpose,
    stderr: String,
    stderr_truncated: bool,
    had_provider_error: bool,
    prepare_assistant_output: String,
    prepare_assistant_output_truncated: bool,
}

/// Coordinates provider child termination with the Core lifecycle operation.
///
/// The provider waiter owns the child handle and is responsible for waiting
/// and reaping it. `end_thread()` can request termination while holding the
/// runtime mutex, but must then wait for the waiter to finish without making
/// the waiter reacquire that mutex on the cancellation path.
#[derive(Debug)]
pub struct ProviderProcessTermination {
    cancelled: AtomicBool,
    completed: Mutex<bool>,
    changed: Condvar,
}

impl ProviderProcessTermination {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            completed: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn mark_completed(&self) {
        let mut completed = self.completed.lock().unwrap();
        *completed = true;
        self.changed.notify_all();
    }

    pub fn wait_completed(&self) {
        let mut completed = self.completed.lock().unwrap();
        while !*completed {
            completed = self.changed.wait(completed).unwrap();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningProviderProcessPurpose {
    UserMessage,
    Prepare,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadEventPartial {
    AssistantMessage { text: String },
    ProviderSessionIdUpdated { provider_session_id: String },
    ProviderError { error: PedelecError },
}

enum ProviderTurnKind<'a> {
    UserMessage { message: &'a str },
    Prepare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderBootstrapMode {
    CodexDeveloperInstructions,
    ClaudeAppendSystemPrompt,
    OpenCodeInlineAgent,
    AntigravityWorkspaceAgent,
    NativeSystemPrompt,
    UserPromptFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderBootstrapCapabilities {
    privileged_bootstrap: ProviderBootstrapMode,
}

impl Default for ProviderBootstrapCapabilities {
    fn default() -> Self {
        Self {
            privileged_bootstrap: ProviderBootstrapMode::UserPromptFallback,
        }
    }
}

trait ProviderAdapter {
    fn code(&self) -> ProviderCode;
    fn capabilities(&self) -> ProviderCapabilities;
    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError>;
    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError>;
    fn preprocess_stdout_chunk(&mut self, chunk: &str) -> Vec<String> {
        vec![chunk.to_string()]
    }
    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial>;
    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial>;
}

#[derive(Debug, Clone)]
enum ProviderAdapterInstance {
    Codex(CodexProviderAdapter),
    Antigravity(AntigravityProviderAdapter),
    OpenCode(OpenCodeProviderAdapter),
    Cursor(CursorProviderAdapter),
    Claude(ClaudeProviderAdapter),
    Ollama(OllamaProviderAdapter),
}

impl ProviderAdapterInstance {
    fn new(provider: ProviderCode) -> Self {
        match provider {
            ProviderCode::Codex => Self::Codex(CodexProviderAdapter::default()),
            ProviderCode::Antigravity => Self::Antigravity(AntigravityProviderAdapter::default()),
            ProviderCode::OpenCode => Self::OpenCode(OpenCodeProviderAdapter::default()),
            ProviderCode::Cursor => Self::Cursor(CursorProviderAdapter::default()),
            ProviderCode::Claude => Self::Claude(ClaudeProviderAdapter::default()),
            ProviderCode::Ollama => Self::Ollama(OllamaProviderAdapter::default()),
        }
    }
}

impl ProviderAdapter for ProviderAdapterInstance {
    fn code(&self) -> ProviderCode {
        match self {
            Self::Codex(adapter) => adapter.code(),
            Self::Antigravity(adapter) => adapter.code(),
            Self::OpenCode(adapter) => adapter.code(),
            Self::Cursor(adapter) => adapter.code(),
            Self::Claude(adapter) => adapter.code(),
            Self::Ollama(adapter) => adapter.code(),
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        match self {
            Self::Codex(adapter) => adapter.capabilities(),
            Self::Antigravity(adapter) => adapter.capabilities(),
            Self::OpenCode(adapter) => adapter.capabilities(),
            Self::Cursor(adapter) => adapter.capabilities(),
            Self::Claude(adapter) => adapter.capabilities(),
            Self::Ollama(adapter) => adapter.capabilities(),
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        match self {
            Self::Codex(adapter) => adapter.build_run_command(ctx, message),
            Self::Antigravity(adapter) => adapter.build_run_command(ctx, message),
            Self::OpenCode(adapter) => adapter.build_run_command(ctx, message),
            Self::Cursor(adapter) => adapter.build_run_command(ctx, message),
            Self::Claude(adapter) => adapter.build_run_command(ctx, message),
            Self::Ollama(adapter) => adapter.build_run_command(ctx, message),
        }
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        match self {
            Self::Codex(adapter) => adapter.build_resume_command(ctx, provider_session_id, message),
            Self::Antigravity(adapter) => {
                adapter.build_resume_command(ctx, provider_session_id, message)
            }
            Self::OpenCode(adapter) => {
                adapter.build_resume_command(ctx, provider_session_id, message)
            }
            Self::Cursor(adapter) => {
                adapter.build_resume_command(ctx, provider_session_id, message)
            }
            Self::Claude(adapter) => {
                adapter.build_resume_command(ctx, provider_session_id, message)
            }
            Self::Ollama(adapter) => {
                adapter.build_resume_command(ctx, provider_session_id, message)
            }
        }
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        match self {
            Self::Codex(adapter) => adapter.parse_stdout_event(chunk),
            Self::Antigravity(adapter) => adapter.parse_stdout_event(chunk),
            Self::OpenCode(adapter) => adapter.parse_stdout_event(chunk),
            Self::Cursor(adapter) => adapter.parse_stdout_event(chunk),
            Self::Claude(adapter) => adapter.parse_stdout_event(chunk),
            Self::Ollama(adapter) => adapter.parse_stdout_event(chunk),
        }
    }

    fn preprocess_stdout_chunk(&mut self, chunk: &str) -> Vec<String> {
        match self {
            Self::Claude(adapter) => adapter.preprocess_stdout_chunk(chunk),
            _ => vec![chunk.to_string()],
        }
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        match self {
            Self::Codex(adapter) => adapter.parse_stderr_event(chunk),
            Self::Antigravity(adapter) => adapter.parse_stderr_event(chunk),
            Self::OpenCode(adapter) => adapter.parse_stderr_event(chunk),
            Self::Cursor(adapter) => adapter.parse_stderr_event(chunk),
            Self::Claude(adapter) => adapter.parse_stderr_event(chunk),
            Self::Ollama(adapter) => adapter.parse_stderr_event(chunk),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CodexProviderAdapter {
    stdout_buffer: String,
    stderr_buffer: String,
}

impl ProviderAdapter for CodexProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::Codex
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: true,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        let mut args = vec![
            "exec".to_string(),
            "--cd".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        args.push("-".to_string());
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        Ok(CommandSpec {
            program: "codex".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if !self.capabilities().supports_resume_by_session_id {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "codex resume is not supported",
            ));
        }

        let mut args = vec![
            "exec".to_string(),
            "--cd".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
            "resume".to_string(),
            provider_session_id.to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        args.push("-".to_string());
        let prompt = build_provider_resume_prompt(message);
        Ok(CommandSpec {
            program: "codex".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_provider_chunk(
            &mut self.stdout_buffer,
            chunk,
            find_codex_assistant_text_in_json,
        )
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_provider_chunk(
            &mut self.stderr_buffer,
            chunk,
            find_codex_assistant_text_in_json,
        )
    }
}

#[derive(Debug, Clone, Default)]
struct AntigravityProviderAdapter {
    stdout_buffer: String,
    stderr_buffer: String,
}

impl ProviderAdapter for AntigravityProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::Antigravity
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: true,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        validate_antigravity_prompt_length(&prompt)?;
        let mut args = vec![
            "-p".to_string(),
            prompt.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--mode".to_string(),
            "accept-edits".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        Ok(CommandSpec {
            program: "agy".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: String::new(),
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if !self.capabilities().supports_resume_by_session_id {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "antigravity resume is not supported",
            ));
        }

        let prompt = build_provider_resume_prompt(message);
        validate_antigravity_prompt_length(&prompt)?;
        let mut args = vec![
            "--conversation".to_string(),
            provider_session_id.to_string(),
            "-p".to_string(),
            prompt.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--mode".to_string(),
            "accept-edits".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        Ok(CommandSpec {
            program: "agy".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: String::new(),
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_antigravity_provider_chunk(&mut self.stdout_buffer, chunk)
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_antigravity_provider_chunk(&mut self.stderr_buffer, chunk)
    }
}

fn validate_antigravity_prompt_length(prompt: &str) -> Result<(), PedelecError> {
    let prompt_length = prompt.encode_utf16().count();
    if prompt_length <= ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS {
        return Ok(());
    }

    Err(PedelecError::with_details(
        error_codes::PROVIDER_PROMPT_TOO_LARGE,
        "Antigravity prompt exceeds the 20,000 character limit",
        serde_json::json!({
            "provider": "antigravity",
            "promptLength": prompt_length,
            "maxPromptLength": ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS,
            "lengthUnit": "utf16CodeUnits",
        }),
    ))
}

#[derive(Debug, Clone, Default)]
struct OpenCodeProviderAdapter {
    stdout_buffer: String,
    stderr_buffer: String,
}

impl ProviderAdapter for OpenCodeProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::OpenCode
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: false,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        let mut args = vec![
            "run".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--thinking".to_string(),
            "--pure".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--dir".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
        ];
        args.extend(ctx.thread.effort_args.clone());
        args.push("-".to_string());
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        Ok(CommandSpec {
            program: "opencode".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if provider_session_id.trim().is_empty() {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "opencode resume requires a provider session id",
            ));
        }

        let mut args = vec![
            "run".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--thinking".to_string(),
            "--pure".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--dir".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--session".to_string(),
            provider_session_id.to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        args.push("-".to_string());
        let prompt = build_provider_resume_prompt(message);
        Ok(CommandSpec {
            program: "opencode".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_opencode_provider_chunk(&mut self.stdout_buffer, chunk)
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_opencode_provider_chunk(&mut self.stderr_buffer, chunk)
    }
}

#[derive(Debug, Clone, Default)]
struct CursorProviderAdapter {
    stdout_buffer: String,
    stderr_buffer: String,
}

impl ProviderAdapter for CursorProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::Cursor
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: false,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        let mut args = vec![
            "--workspace".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--force".to_string(),
            "--trust".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        Ok(CommandSpec {
            program: "cursor-agent".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if provider_session_id.trim().is_empty() {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "cursor resume requires a provider session id",
            ));
        }

        let mut args = vec![
            "--workspace".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--resume".to_string(),
            provider_session_id.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--force".to_string(),
            "--trust".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        let prompt = build_provider_resume_prompt(message);
        Ok(CommandSpec {
            program: "cursor-agent".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_cursor_provider_chunk(&mut self.stdout_buffer, chunk)
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_cursor_provider_chunk(&mut self.stderr_buffer, chunk)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeStdoutFilterMode {
    Inspecting,
    Passing,
    RedactingSignature,
    Dropping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeJsonObjectState {
    KeyOrEnd,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeJsonArrayState {
    ValueOrEnd,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeJsonObjectKind {
    RootEvent,
    Message,
    ContentItem,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeJsonArrayKind {
    Content,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeJsonKey {
    Type,
    Message,
    Content,
    Signature,
    Other,
}

impl ClaudeJsonKey {
    fn from_decoded(value: &str) -> Self {
        match value {
            "type" => Self::Type,
            "message" => Self::Message,
            "content" => Self::Content,
            "signature" => Self::Signature,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeContentItemType {
    Unknown,
    Thinking,
    Other,
}

#[derive(Debug, Clone)]
enum ClaudeJsonContainer {
    Object {
        kind: ClaudeJsonObjectKind,
        state: ClaudeJsonObjectState,
        key: ClaudeJsonKey,
        content_item_type: ClaudeContentItemType,
    },
    Array {
        kind: ClaudeJsonArrayKind,
        state: ClaudeJsonArrayState,
    },
}

#[derive(Debug, Clone, Copy)]
enum ClaudeJsonStringRole {
    Key,
    Value {
        is_type_value: bool,
        is_root_type_value: bool,
        is_content_item_type_value: bool,
    },
    ThinkingSignatureValue,
}

#[derive(Debug, Clone, Copy)]
struct ClaudeJsonValueContext {
    key: ClaudeJsonKey,
    is_root_type_value: bool,
    is_content_item_type_value: bool,
    is_thinking_signature_value: bool,
}

#[derive(Debug, Clone)]
struct ClaudeJsonStringScanner {
    raw: String,
    role: ClaudeJsonStringRole,
    escaped: bool,
    too_long: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeStdoutFilterDecision {
    Continue,
    Pass,
    Drop,
    StartRedaction,
    EndRedaction,
}

#[derive(Debug, Clone, Default)]
struct ClaudeJsonEventScanner {
    containers: Vec<ClaudeJsonContainer>,
    string: Option<ClaudeJsonStringScanner>,
    primitive: Option<String>,
    root_started: bool,
    root_complete: bool,
    root_type_is_user: Option<bool>,
    root_type_is_assistant: Option<bool>,
    saw_tool_result: bool,
}

impl ClaudeJsonEventScanner {
    fn scan_char(&mut self, ch: char) -> ClaudeStdoutFilterDecision {
        let mut current = Some(ch);
        while let Some(ch) = current.take() {
            if let Some(string) = self.string.as_mut() {
                if string.escaped {
                    if !matches!(string.role, ClaudeJsonStringRole::ThinkingSignatureValue)
                        && !string.too_long
                    {
                        string.raw.push(ch);
                        string.too_long = string.raw.len() > 128;
                    }
                    string.escaped = false;
                    continue;
                }
                match ch {
                    '\\' => {
                        if !matches!(string.role, ClaudeJsonStringRole::ThinkingSignatureValue)
                            && !string.too_long
                        {
                            string.raw.push(ch);
                        }
                        string.escaped = true;
                    }
                    '"' => return self.finish_string(),
                    ch if ch.is_control() => return ClaudeStdoutFilterDecision::Pass,
                    _ => {
                        if !matches!(string.role, ClaudeJsonStringRole::ThinkingSignatureValue)
                            && !string.too_long
                        {
                            string.raw.push(ch);
                            string.too_long = string.raw.len() > 128;
                        }
                    }
                }
                continue;
            }

            if self.primitive.is_some() {
                if ch.is_whitespace() || matches!(ch, ',' | ']' | '}') {
                    if !self.finish_primitive() {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    current = Some(ch);
                    continue;
                }
                let primitive = self.primitive.as_mut().expect("primitive exists");
                if primitive.len() >= 64 || matches!(ch, ':' | '{' | '[' | '"') {
                    return ClaudeStdoutFilterDecision::Pass;
                }
                primitive.push(ch);
                continue;
            }

            if ch.is_whitespace() {
                continue;
            }
            if self.root_complete {
                return ClaudeStdoutFilterDecision::Pass;
            }

            match ch {
                '{' => {
                    if !self.start_container(true) {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                }
                '[' => {
                    if !self.root_started {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    if !self.start_container(false) {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                }
                '}' => {
                    if !matches!(
                        self.containers.last(),
                        Some(ClaudeJsonContainer::Object {
                            state: ClaudeJsonObjectState::KeyOrEnd
                                | ClaudeJsonObjectState::CommaOrEnd,
                            ..
                        })
                    ) {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    self.containers.pop();
                    if !self.complete_container_value() {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    if self.root_complete {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                }
                ']' => {
                    if !matches!(
                        self.containers.last(),
                        Some(ClaudeJsonContainer::Array {
                            state: ClaudeJsonArrayState::ValueOrEnd
                                | ClaudeJsonArrayState::CommaOrEnd,
                            ..
                        })
                    ) {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    self.containers.pop();
                    if !self.complete_container_value() {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    if self.root_complete {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                }
                ':' => match self.containers.last_mut() {
                    Some(ClaudeJsonContainer::Object { state, .. })
                        if *state == ClaudeJsonObjectState::Colon =>
                    {
                        *state = ClaudeJsonObjectState::Value;
                    }
                    _ => return ClaudeStdoutFilterDecision::Pass,
                },
                ',' => match self.containers.last_mut() {
                    Some(ClaudeJsonContainer::Object { state, .. })
                        if *state == ClaudeJsonObjectState::CommaOrEnd =>
                    {
                        *state = ClaudeJsonObjectState::KeyOrEnd;
                    }
                    Some(ClaudeJsonContainer::Array { state, .. })
                        if *state == ClaudeJsonArrayState::CommaOrEnd =>
                    {
                        *state = ClaudeJsonArrayState::ValueOrEnd;
                    }
                    _ => return ClaudeStdoutFilterDecision::Pass,
                },
                '"' => {
                    let role = match self.containers.last() {
                        Some(ClaudeJsonContainer::Object {
                            state: ClaudeJsonObjectState::KeyOrEnd,
                            ..
                        }) => ClaudeJsonStringRole::Key,
                        _ => {
                            let Some(context) = self.string_value_context() else {
                                return ClaudeStdoutFilterDecision::Pass;
                            };
                            if context.is_thinking_signature_value {
                                self.string = Some(ClaudeJsonStringScanner {
                                    raw: String::new(),
                                    role: ClaudeJsonStringRole::ThinkingSignatureValue,
                                    escaped: false,
                                    too_long: false,
                                });
                                return ClaudeStdoutFilterDecision::StartRedaction;
                            }
                            ClaudeJsonStringRole::Value {
                                is_type_value: context.key == ClaudeJsonKey::Type,
                                is_root_type_value: context.is_root_type_value,
                                is_content_item_type_value: context.is_content_item_type_value,
                            }
                        }
                    };
                    self.string = Some(ClaudeJsonStringScanner {
                        raw: String::new(),
                        role,
                        escaped: false,
                        too_long: false,
                    });
                }
                '-' | '0'..='9' | 't' | 'f' | 'n' => {
                    let Some(context) = self.string_value_context() else {
                        return ClaudeStdoutFilterDecision::Pass;
                    };
                    if context.key == ClaudeJsonKey::Type && context.is_root_type_value {
                        self.root_type_is_user = Some(false);
                        self.root_type_is_assistant = Some(false);
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    self.primitive = Some(ch.to_string());
                }
                _ => return ClaudeStdoutFilterDecision::Pass,
            }
        }

        self.decision()
    }

    fn start_container(&mut self, object: bool) -> bool {
        if !self.root_started {
            if !object || !self.containers.is_empty() {
                return false;
            }
            self.root_started = true;
        } else if self.string_value_context().is_none() {
            return false;
        }

        self.containers.push(if object {
            ClaudeJsonContainer::Object {
                kind: self.object_kind_for_next_value(),
                state: ClaudeJsonObjectState::KeyOrEnd,
                key: ClaudeJsonKey::Other,
                content_item_type: ClaudeContentItemType::Unknown,
            }
        } else {
            ClaudeJsonContainer::Array {
                kind: self.array_kind_for_next_value(),
                state: ClaudeJsonArrayState::ValueOrEnd,
            }
        });
        true
    }

    fn object_kind_for_next_value(&self) -> ClaudeJsonObjectKind {
        if self.containers.is_empty() {
            return ClaudeJsonObjectKind::RootEvent;
        }

        match self.containers.last() {
            Some(ClaudeJsonContainer::Object {
                kind: ClaudeJsonObjectKind::RootEvent,
                key: ClaudeJsonKey::Message,
                ..
            }) => ClaudeJsonObjectKind::Message,
            Some(ClaudeJsonContainer::Array {
                kind: ClaudeJsonArrayKind::Content,
                ..
            }) => ClaudeJsonObjectKind::ContentItem,
            _ => ClaudeJsonObjectKind::Other,
        }
    }

    fn array_kind_for_next_value(&self) -> ClaudeJsonArrayKind {
        match self.containers.last() {
            Some(ClaudeJsonContainer::Object {
                kind: ClaudeJsonObjectKind::Message,
                key: ClaudeJsonKey::Content,
                ..
            }) => ClaudeJsonArrayKind::Content,
            _ => ClaudeJsonArrayKind::Other,
        }
    }

    fn string_value_context(&self) -> Option<ClaudeJsonValueContext> {
        match self.containers.last()? {
            ClaudeJsonContainer::Object {
                state: ClaudeJsonObjectState::Value,
                kind,
                key,
                content_item_type,
            } => Some(ClaudeJsonValueContext {
                key: *key,
                is_root_type_value: *kind == ClaudeJsonObjectKind::RootEvent
                    && *key == ClaudeJsonKey::Type,
                is_content_item_type_value: *kind == ClaudeJsonObjectKind::ContentItem
                    && *key == ClaudeJsonKey::Type,
                // Claude's stream-json events emit the root `type`, and content blocks
                // emit their `type`, before block-specific fields. Keeping that contract
                // lets us redact without buffering a potentially unbounded signature
                // before its event/block kind is known.
                is_thinking_signature_value: *kind == ClaudeJsonObjectKind::ContentItem
                    && *content_item_type == ClaudeContentItemType::Thinking
                    && *key == ClaudeJsonKey::Signature
                    && self.root_type_is_assistant == Some(true),
            }),
            ClaudeJsonContainer::Array {
                state: ClaudeJsonArrayState::ValueOrEnd,
                ..
            } => Some(ClaudeJsonValueContext {
                key: ClaudeJsonKey::Other,
                is_root_type_value: false,
                is_content_item_type_value: false,
                is_thinking_signature_value: false,
            }),
            _ => None,
        }
    }

    fn finish_string(&mut self) -> ClaudeStdoutFilterDecision {
        let string = self.string.take().expect("string scanner exists");
        if matches!(string.role, ClaudeJsonStringRole::ThinkingSignatureValue) {
            if !self.complete_scalar_value() {
                return ClaudeStdoutFilterDecision::Pass;
            }
            return ClaudeStdoutFilterDecision::EndRedaction;
        }

        if string.escaped || string.too_long {
            return match string.role {
                _ if string.escaped => ClaudeStdoutFilterDecision::Pass,
                ClaudeJsonStringRole::Key => match self.containers.last_mut() {
                    Some(ClaudeJsonContainer::Object { state, key, .. })
                        if *state == ClaudeJsonObjectState::KeyOrEnd =>
                    {
                        *state = ClaudeJsonObjectState::Colon;
                        *key = ClaudeJsonKey::Other;
                        self.decision()
                    }
                    _ => ClaudeStdoutFilterDecision::Pass,
                },
                ClaudeJsonStringRole::Value {
                    is_root_type_value,
                    is_content_item_type_value,
                    ..
                } => {
                    if is_content_item_type_value {
                        self.set_content_item_type(ClaudeContentItemType::Other);
                    }
                    if !self.complete_scalar_value() {
                        return ClaudeStdoutFilterDecision::Pass;
                    }
                    if is_root_type_value {
                        self.root_type_is_user = Some(false);
                        self.root_type_is_assistant = Some(false);
                    }
                    self.decision()
                }
                ClaudeJsonStringRole::ThinkingSignatureValue => ClaudeStdoutFilterDecision::Pass,
            };
        }
        let encoded = format!("\"{}\"", string.raw);
        let Ok(value) = serde_json::from_str::<String>(&encoded) else {
            return ClaudeStdoutFilterDecision::Pass;
        };

        match string.role {
            ClaudeJsonStringRole::Key => match self.containers.last_mut() {
                Some(ClaudeJsonContainer::Object { state, key, .. })
                    if *state == ClaudeJsonObjectState::KeyOrEnd =>
                {
                    *state = ClaudeJsonObjectState::Colon;
                    *key = ClaudeJsonKey::from_decoded(&value);
                }
                _ => return ClaudeStdoutFilterDecision::Pass,
            },
            ClaudeJsonStringRole::Value {
                is_type_value,
                is_root_type_value,
                is_content_item_type_value,
            } => {
                if is_content_item_type_value {
                    self.set_content_item_type(if value == "thinking" {
                        ClaudeContentItemType::Thinking
                    } else {
                        ClaudeContentItemType::Other
                    });
                }
                if !self.complete_scalar_value() {
                    return ClaudeStdoutFilterDecision::Pass;
                }
                if is_type_value {
                    self.saw_tool_result |= value == "tool_result";
                    if is_root_type_value {
                        self.root_type_is_user = Some(value == "user");
                        self.root_type_is_assistant = Some(value == "assistant");
                    }
                }
            }
            ClaudeJsonStringRole::ThinkingSignatureValue => {
                return ClaudeStdoutFilterDecision::Pass;
            }
        }
        self.decision()
    }

    fn set_content_item_type(&mut self, content_item_type: ClaudeContentItemType) {
        if let Some(ClaudeJsonContainer::Object {
            kind: ClaudeJsonObjectKind::ContentItem,
            content_item_type: current,
            ..
        }) = self.containers.last_mut()
        {
            *current = content_item_type;
        }
    }

    fn finish_primitive(&mut self) -> bool {
        let primitive = self.primitive.take().expect("primitive exists");
        if serde_json::from_str::<Value>(&primitive).is_err() {
            return false;
        }
        self.complete_scalar_value()
    }

    fn complete_scalar_value(&mut self) -> bool {
        match self.containers.last_mut() {
            Some(ClaudeJsonContainer::Object { state, key, .. })
                if *state == ClaudeJsonObjectState::Value =>
            {
                *state = ClaudeJsonObjectState::CommaOrEnd;
                *key = ClaudeJsonKey::Other;
                true
            }
            Some(ClaudeJsonContainer::Array { state, .. })
                if *state == ClaudeJsonArrayState::ValueOrEnd =>
            {
                *state = ClaudeJsonArrayState::CommaOrEnd;
                true
            }
            _ => false,
        }
    }

    fn complete_container_value(&mut self) -> bool {
        if self.containers.is_empty() {
            self.root_complete = true;
            return true;
        }
        self.complete_scalar_value()
    }

    fn decision(&self) -> ClaudeStdoutFilterDecision {
        if self.root_type_is_user == Some(true) && self.saw_tool_result {
            ClaudeStdoutFilterDecision::Drop
        } else if self.root_type_is_user == Some(false) || self.root_complete {
            ClaudeStdoutFilterDecision::Pass
        } else {
            ClaudeStdoutFilterDecision::Continue
        }
    }
}

#[derive(Debug, Clone)]
struct ClaudeStdoutFilter {
    mode: ClaudeStdoutFilterMode,
    pending: String,
    scanner: ClaudeJsonEventScanner,
}

impl Default for ClaudeStdoutFilter {
    fn default() -> Self {
        Self {
            mode: ClaudeStdoutFilterMode::Inspecting,
            pending: String::new(),
            scanner: ClaudeJsonEventScanner::default(),
        }
    }
}

impl ClaudeStdoutFilter {
    const SIGNATURE_REPLACEMENT: &'static str = "\"[omitted]\"";

    fn preprocess_chunk(&mut self, chunk: &str) -> Vec<String> {
        let mut retained = String::new();
        for ch in chunk.chars() {
            if ch == '\n' {
                match self.mode {
                    ClaudeStdoutFilterMode::Inspecting => {
                        self.pending.push(ch);
                        retained.push_str(&self.pending);
                    }
                    ClaudeStdoutFilterMode::Passing => retained.push(ch),
                    ClaudeStdoutFilterMode::RedactingSignature => retained.push(ch),
                    ClaudeStdoutFilterMode::Dropping => {}
                }
                self.reset_event();
                continue;
            }

            match self.mode {
                ClaudeStdoutFilterMode::Passing => match self.scanner.scan_char(ch) {
                    ClaudeStdoutFilterDecision::StartRedaction => {
                        retained.push_str(Self::SIGNATURE_REPLACEMENT);
                        self.mode = ClaudeStdoutFilterMode::RedactingSignature;
                    }
                    ClaudeStdoutFilterDecision::Continue
                    | ClaudeStdoutFilterDecision::Pass
                    | ClaudeStdoutFilterDecision::Drop
                    | ClaudeStdoutFilterDecision::EndRedaction => retained.push(ch),
                },
                ClaudeStdoutFilterMode::RedactingSignature => {
                    if matches!(
                        self.scanner.scan_char(ch),
                        ClaudeStdoutFilterDecision::EndRedaction
                    ) {
                        self.mode = ClaudeStdoutFilterMode::Passing;
                    }
                }
                ClaudeStdoutFilterMode::Dropping => {}
                ClaudeStdoutFilterMode::Inspecting => {
                    self.pending.push(ch);
                    match self.scanner.scan_char(ch) {
                        ClaudeStdoutFilterDecision::Continue => {}
                        ClaudeStdoutFilterDecision::Pass => {
                            retained.push_str(&self.pending);
                            self.pending.clear();
                            self.mode = ClaudeStdoutFilterMode::Passing;
                        }
                        ClaudeStdoutFilterDecision::Drop => {
                            self.pending.clear();
                            self.mode = ClaudeStdoutFilterMode::Dropping;
                        }
                        ClaudeStdoutFilterDecision::StartRedaction => {
                            self.pending.pop();
                            retained.push_str(&self.pending);
                            retained.push_str(Self::SIGNATURE_REPLACEMENT);
                            self.pending.clear();
                            self.mode = ClaudeStdoutFilterMode::RedactingSignature;
                        }
                        ClaudeStdoutFilterDecision::EndRedaction => {}
                    }
                }
            }
        }

        if retained.is_empty() {
            Vec::new()
        } else {
            vec![retained]
        }
    }

    fn reset_event(&mut self) {
        self.mode = ClaudeStdoutFilterMode::Inspecting;
        self.pending.clear();
        self.scanner = ClaudeJsonEventScanner::default();
    }
}

#[derive(Debug, Clone, Default)]
struct ClaudeProviderAdapter {
    stdout_filter: ClaudeStdoutFilter,
    stdout_buffer: String,
    stderr_buffer: String,
}

impl ProviderAdapter for ClaudeProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::Claude
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: true,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        Ok(CommandSpec {
            program: "claude".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if provider_session_id.trim().is_empty() {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "claude resume requires a provider session id",
            ));
        }

        let mut args = vec![
            "-p".to_string(),
            "--resume".to_string(),
            provider_session_id.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        args.extend(ctx.thread.effort_args.clone());
        let prompt = build_provider_resume_prompt(message);
        Ok(CommandSpec {
            program: "claude".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env: build_provider_env(ctx)?,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_claude_provider_chunk(&mut self.stdout_buffer, chunk)
    }

    fn preprocess_stdout_chunk(&mut self, chunk: &str) -> Vec<String> {
        self.stdout_filter.preprocess_chunk(chunk)
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_claude_provider_chunk(&mut self.stderr_buffer, chunk)
    }
}

#[derive(Debug, Clone, Default)]
struct OllamaProviderAdapter {
    stdout_buffer: String,
}

impl ProviderAdapter for OllamaProviderAdapter {
    fn code(&self) -> ProviderCode {
        ProviderCode::Ollama
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_events: true,
            supports_resume_by_session_id: true,
            supports_user_supplied_session_id: false,
            supports_provider_generated_session_id_parse: true,
            supports_resume_last_session: false,
        }
    }

    fn build_run_command(
        &self,
        ctx: &RunPromptProviderContext,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        required_ollama_model(&ctx.thread)?;
        let mut args = vec!["--provider".to_string(), "ollama".to_string()];
        args.extend(ctx.thread.effort_args.clone());
        args.extend([
            "--sandbox".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
        ]);
        let prompt = build_provider_run_prompt(
            &ctx.thread,
            &ctx.tool_registry,
            message,
            ctx.include_fallback_bootstrap,
        );
        let mut env = build_provider_env(ctx)?;
        env.push((
            "OLLAMA_API_KEY".to_string(),
            require_ollama_api_key(Some(ctx.settings.provider_settings.ollama.api_key.clone()))?,
        ));
        if !ctx
            .settings
            .provider_settings
            .ollama
            .tavily_api_key
            .trim()
            .is_empty()
        {
            env.push((
                "TAVILY_API_KEY".to_string(),
                ctx.settings
                    .provider_settings
                    .ollama
                    .tavily_api_key
                    .trim()
                    .to_string(),
            ));
        }
        Ok(CommandSpec {
            program: "pedelec-agent".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn build_resume_command(
        &self,
        ctx: &RunPromptProviderContext,
        provider_session_id: &str,
        message: &str,
    ) -> Result<CommandSpec, PedelecError> {
        if provider_session_id.trim().is_empty() {
            return Err(provider_unsupported_error(
                &ctx.thread,
                "ollama resume requires a provider session id",
            ));
        }

        required_ollama_model(&ctx.thread)?;
        let mut args = vec!["--provider".to_string(), "ollama".to_string()];
        args.extend(ctx.thread.effort_args.clone());
        args.extend([
            "--sandbox".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
            "--session-id".to_string(),
            provider_session_id.to_string(),
        ]);
        let prompt = build_provider_resume_prompt(message);
        let mut env = build_provider_env(ctx)?;
        env.push((
            "OLLAMA_API_KEY".to_string(),
            require_ollama_api_key(Some(ctx.settings.provider_settings.ollama.api_key.clone()))?,
        ));
        if !ctx
            .settings
            .provider_settings
            .ollama
            .tavily_api_key
            .trim()
            .is_empty()
        {
            env.push((
                "TAVILY_API_KEY".to_string(),
                ctx.settings
                    .provider_settings
                    .ollama
                    .tavily_api_key
                    .trim()
                    .to_string(),
            ));
        }
        Ok(CommandSpec {
            program: "pedelec-agent".to_string(),
            args,
            cwd: ctx.thread.sandbox_path.clone(),
            env,
            prompt: prompt.clone(),
            stdin: prompt,
        })
    }

    fn parse_stdout_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_pedelec_agent_provider_chunk(&mut self.stdout_buffer, chunk)
    }

    fn parse_stderr_event(&mut self, _chunk: &str) -> Vec<ThreadEventPartial> {
        Vec::new()
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
    pub sandbox_manager: SandboxManager,
    pub skill_manager: SkillManager,
    pub tool_registry: ToolRegistryStore,
    pub tool_request_broker: ToolRequestBroker,
    pub event_bus: EventBus,
    pub running_processes: HashMap<String, RunningProviderProcess>,
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
    pub test_provider_command: Option<CommandSpec>,
}

impl CoreRuntime {
    pub fn new() -> Self {
        let mut runtime = Self::default();
        runtime.provider_readiness = ProviderReadiness::new_uninitialized();
        runtime
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
        let sandbox_path = thread.sandbox_path.clone();
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
        // Keep sandbox asset names readable while using the separate 256-bit token
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
                sandbox_path,
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
        let input_path = thread.sandbox_path.join("assets");
        if !input_path.exists() {
            return Ok(ListAssetsOutput { assets: Vec::new() });
        }
        let mut assets = Vec::new();
        collect_sandbox_assets(&input_path, &input_path, &mut assets)?;
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
        let sandbox_path = thread.sandbox_path.clone();
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
                sandbox_path,
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
            |sandbox: &Path| initialize_generated_skills(sandbox, input.skills.as_ref());
        let (sandbox_path, (skills, registry)) = match input.sandbox.as_ref() {
            Some(custom_sandbox) => {
                let sandbox_path = self
                    .sandbox_manager
                    .prepare_custom_sandbox(&custom_sandbox.path)?;
                let initialized = initialize(&sandbox_path)?;
                if let Some(origin) = sdk_origin.as_deref() {
                    let sdk_version = sdk_version.as_deref().ok_or_else(|| {
                        PedelecError::new(
                            error_codes::SANDBOX_CREATE_FAILED,
                            "SDK version metadata is required for custom sandbox sessions",
                        )
                    })?;
                    self.sandbox_manager.ensure_custom_sandbox_config(
                        &sandbox_path,
                        sdk_version,
                        origin,
                    )?;
                }
                (sandbox_path, initialized)
            }
            None => self
                .sandbox_manager
                .create_thread_sandbox_with(&thread_id, initialize)?,
        };

        let now = Utc::now();
        let state = ThreadState {
            thread_id: thread_id.clone(),
            provider: input.provider,
            effort_level,
            effort_args,
            sandbox_path: sandbox_path.clone(),
            skills,
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
            sdk_origin,
        };

        self.thread_manager.insert_thread(
            state,
            ProviderAdapterState {
                provider_session_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        self.tool_registry.insert(thread_id.clone(), registry);
        self.event_bus
            .register_thread_log(&thread_id, thread_event_log_path(&sandbox_path, &thread_id));
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
        let Some(path) = scan.path.clone().filter(|_| scan.version.is_some()) else {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_TERMINAL_UNAVAILABLE,
                "The provider CLI is not available from the latest scan.",
                serde_json::json!({"provider": provider_code_as_str(provider), "platform": std::env::consts::OS}),
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
        apply_provider_bootstrap_capabilities(
            &mut self.provider_scan,
            self.provider_resolved_path.as_ref(),
        );
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
            if self.sandbox_manager.thread_sandbox_exists(&thread_id)? {
                continue;
            }
            return Ok(thread_id);
        }
    }

    pub fn begin_send_text(&mut self, input: SendTextInput) -> Result<SendTextStart, PedelecError> {
        {
            let thread = self.thread_manager.thread(&input.thread_id)?;
            match thread.status {
                ThreadStatus::Running | ThreadStatus::WaitingToolResult => {
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
                ThreadStatus::Error => {
                    return Err(PedelecError::with_details(
                        error_codes::PROVIDER_COMMAND_FAILED,
                        "thread is in error state",
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

        let test_command = self.test_provider_command.clone();

        let command = if let Some(command) = test_command {
            command
        } else {
            self.build_send_text_command(&input)?
        };

        let thread = self.thread_manager.thread_mut(&input.thread_id)?;
        thread.status = ThreadStatus::Running;
        thread.updated_at = Utc::now();
        if let Some(provider_state) = self.thread_manager.provider_state_mut(&input.thread_id) {
            provider_state.has_user_message = true;
        }
        self.event_bus
            .emit_status_changed(&input.thread_id, ThreadStatus::Running);

        Ok(SendTextStart {
            output: SendTextOutput {
                thread_id: input.thread_id,
            },
            command,
        })
    }

    pub fn begin_prepare_thread(
        &mut self,
        input: PrepareThreadInput,
    ) -> Result<PrepareThreadStart, PedelecError> {
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

        if self
            .thread_manager
            .provider_state(&input.thread_id)
            .and_then(|state| state.provider_session_id.as_deref())
            .is_some()
        {
            return Ok(PrepareThreadStart {
                output: PrepareThreadOutput {
                    thread_id: input.thread_id,
                    prepared: true,
                    already_prepared: Some(true),
                },
                command: None,
            });
        }

        let test_command = self.test_provider_command.clone();

        let command = if let Some(command) = test_command {
            command
        } else {
            self.build_prepare_thread_command(&input)?
        };

        let thread = self.thread_manager.thread_mut(&input.thread_id)?;
        thread.status = ThreadStatus::Running;
        thread.updated_at = Utc::now();
        self.event_bus
            .emit_status_changed(&input.thread_id, ThreadStatus::Running);

        Ok(PrepareThreadStart {
            output: PrepareThreadOutput {
                thread_id: input.thread_id,
                prepared: true,
                already_prepared: Some(false),
            },
            command: Some(command),
        })
    }

    fn build_send_text_command(
        &mut self,
        input: &SendTextInput,
    ) -> Result<CommandSpec, PedelecError> {
        self.build_provider_turn_command(
            &input.thread_id,
            ProviderTurnKind::UserMessage {
                message: &input.message,
            },
        )
    }

    fn build_prepare_thread_command(
        &mut self,
        input: &PrepareThreadInput,
    ) -> Result<CommandSpec, PedelecError> {
        self.build_provider_turn_command(&input.thread_id, ProviderTurnKind::Prepare)
    }

    fn build_provider_turn_command(
        &mut self,
        thread_id: &str,
        kind: ProviderTurnKind<'_>,
    ) -> Result<CommandSpec, PedelecError> {
        let thread = self.thread_manager.thread(thread_id)?.clone();
        let provider_state = self
            .thread_manager
            .provider_state(thread_id)
            .cloned()
            .ok_or_else(|| {
                PedelecError::with_details(
                    error_codes::PROVIDER_NOT_FOUND,
                    "provider state was not found for thread",
                    serde_json::json!({ "threadId": thread_id }),
                )
            })?;
        let settings = self.get_settings()?;
        let tool_registry = self.tool_registry.get(thread_id).cloned().ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOL_NOT_FOUND,
                "tool registry was not found for thread",
                serde_json::json!({ "threadId": thread_id }),
            )
        })?;
        let include_fallback_bootstrap = self.provider_bootstrap_mode(&thread.provider)
            == ProviderBootstrapMode::UserPromptFallback
            && provider_state.provider_session_id.is_none();
        let ctx = RunPromptProviderContext {
            thread,
            tool_registry,
            provider_state: provider_state.clone(),
            include_fallback_bootstrap,
            settings,
            core_ipc_endpoint: self.core_ipc_endpoint.clone().unwrap_or_default(),
            core_ipc_runtime_file_path: self
                .core_ipc_runtime_file_path
                .clone()
                .unwrap_or_else(default_runtime_file_path_for_provider),
            provider_resolved_path: self.provider_path_value(),
        };
        let adapter = self.thread_manager.provider_adapter(thread_id)?;
        if adapter.code() != ctx.thread.provider {
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_NOT_FOUND,
                "provider adapter does not match thread provider",
                serde_json::json!({ "threadId": thread_id }),
            ));
        }

        let command = match kind {
            ProviderTurnKind::UserMessage { message } => {
                if let Some(provider_session_id) = provider_state.provider_session_id.as_deref() {
                    let resume_message = if provider_state.has_user_message {
                        message.to_string()
                    } else {
                        build_provider_user_message_task(message)
                    };
                    adapter.build_resume_command(&ctx, provider_session_id, &resume_message)
                } else {
                    adapter.build_run_command(&ctx, message)
                }
            }
            ProviderTurnKind::Prepare => {
                let capabilities = adapter.capabilities();
                if !capabilities.supports_resume_by_session_id
                    || !capabilities.supports_provider_generated_session_id_parse
                {
                    return Err(PedelecError::with_details(
                        error_codes::PROVIDER_PREPARE_UNSUPPORTED,
                        "provider does not support prepare",
                        serde_json::json!({
                            "threadId": thread_id,
                            "provider": provider_code_as_str(&ctx.thread.provider)
                        }),
                    ));
                }
                adapter.build_run_command(&ctx, &build_provider_prepare_task())
            }
        }?;
        let mut command = command;
        self.apply_provider_bootstrap(&ctx, &mut command)?;
        apply_provider_native_skills_policy(&ctx.thread.provider, &mut command);
        self.apply_scanned_provider_program(&ctx.thread.provider, &mut command)?;
        Ok(command)
    }

    fn provider_bootstrap_mode(&self, provider: &ProviderCode) -> ProviderBootstrapMode {
        if let Some(capabilities) = self
            .provider_scan
            .get(provider)
            .and_then(|scan| scan.bootstrap_capabilities)
        {
            return capabilities.privileged_bootstrap;
        }

        // Codex and pedelec-agent expose the required instruction channel as a
        // stable part of their command/runtime contract. The other external
        // providers are deliberately conservative until the latest scan has
        // probed their flag or version capability.
        match provider {
            ProviderCode::Codex => ProviderBootstrapMode::CodexDeveloperInstructions,
            ProviderCode::Ollama => ProviderBootstrapMode::NativeSystemPrompt,
            ProviderCode::Antigravity
            | ProviderCode::Claude
            | ProviderCode::OpenCode
            | ProviderCode::Cursor => ProviderBootstrapMode::UserPromptFallback,
        }
    }

    fn apply_provider_bootstrap(
        &self,
        ctx: &RunPromptProviderContext,
        command: &mut CommandSpec,
    ) -> Result<(), PedelecError> {
        let mode = self.provider_bootstrap_mode(&ctx.thread.provider);
        match mode {
            ProviderBootstrapMode::CodexDeveloperInstructions => {
                remove_codex_developer_instruction_override(&mut command.args);
                let insertion_index = command
                    .args
                    .iter()
                    .position(|arg| arg == "exec")
                    .map_or(0, |index| index + 1);
                command.args.splice(
                    insertion_index..insertion_index,
                    [
                        "-c".to_string(),
                        format!(
                            "developer_instructions={}",
                            build_pedelec_bootstrap_instruction()
                        ),
                    ],
                );
            }
            ProviderBootstrapMode::ClaudeAppendSystemPrompt => {
                command.args.retain(|arg| arg != "--append-system-prompt");
                command.args.push("--append-system-prompt".to_string());
                command.args.push(build_pedelec_bootstrap_instruction());
            }
            ProviderBootstrapMode::OpenCodeInlineAgent => {
                remove_agent_selector(&mut command.args);
                command.args = insert_agent_selector(command.args.clone(), PEDELEC_OPENCODE_AGENT);
                let config = merge_opencode_runtime_agent_config(command)?;
                set_command_env(command, OPENCODE_CONFIG_CONTENT_ENV, config);
            }
            ProviderBootstrapMode::AntigravityWorkspaceAgent => {
                ensure_antigravity_custom_agent(&ctx.thread.sandbox_path)?;
                remove_agent_selector(&mut command.args);
                command.args = insert_agent_selector(command.args.clone(), PEDELEC_OPENCODE_AGENT);
            }
            ProviderBootstrapMode::NativeSystemPrompt
            | ProviderBootstrapMode::UserPromptFallback => {}
        }
        Ok(())
    }

    fn apply_scanned_provider_program(
        &self,
        provider: &ProviderCode,
        command: &mut CommandSpec,
    ) -> Result<(), PedelecError> {
        if *provider == ProviderCode::Ollama {
            return Ok(());
        }
        let selected = self
            .provider_scan
            .get(provider)
            .and_then(|entry| entry.path.as_ref());
        let Some(selected) = selected else {
            #[cfg(test)]
            return Ok(());
            #[cfg(not(test))]
            return Err(PedelecError::with_details(
                error_codes::PROVIDER_NOT_FOUND,
                "provider is unavailable; refresh Providers after installing or upgrading it",
                serde_json::json!({ "provider": provider_code_as_str(provider) }),
            ));
        };
        command.program = selected.to_string_lossy().to_string();
        Ok(())
    }

    pub fn register_provider_process(
        &mut self,
        thread_id: &str,
        process_id: u32,
        child: Arc<Mutex<Option<Child>>>,
        purpose: RunningProviderProcessPurpose,
    ) -> Arc<ProviderProcessTermination> {
        let termination = Arc::new(ProviderProcessTermination::new());
        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            thread.process_id = Some(process_id);
            thread.updated_at = Utc::now();
        }
        if let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) {
            provider_state.last_process_id = Some(process_id);
        }
        self.running_processes.insert(
            thread_id.to_string(),
            RunningProviderProcess {
                process_id,
                child,
                termination: Arc::clone(&termination),
                purpose,
                stderr: String::new(),
                stderr_truncated: false,
                had_provider_error: false,
                prepare_assistant_output: String::new(),
                prepare_assistant_output_truncated: false,
            },
        );
        termination
    }

    pub fn fail_provider_process_start(
        &mut self,
        thread_id: &str,
        error: PedelecError,
        purpose: RunningProviderProcessPurpose,
    ) {
        self.running_processes.remove(thread_id);
        self.tool_request_broker.clear_thread(thread_id);
        if purpose == RunningProviderProcessPurpose::Prepare {
            self.discard_failed_prepare_provider_session(thread_id);
        }
        let status = match purpose {
            RunningProviderProcessPurpose::UserMessage => ThreadStatus::Error,
            RunningProviderProcessPurpose::Prepare => ThreadStatus::Idle,
        };
        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            thread.status = status.clone();
            thread.process_id = None;
            thread.updated_at = Utc::now();
        }
        if purpose == RunningProviderProcessPurpose::Prepare {
            self.emit_thread_provider_error(thread_id, error);
            self.event_bus.emit_status_changed(thread_id, status);
        } else {
            self.event_bus.emit_status_changed(thread_id, status);
            self.emit_thread_provider_error(thread_id, error);
        }
    }

    pub fn emit_provider_command_started(
        &mut self,
        thread_id: &str,
        process_id: u32,
        command: &CommandSpec,
    ) {
        self.event_bus
            .emit_provider_command_started(thread_id, process_id, command);
    }

    pub fn emit_provider_stdout(&mut self, thread_id: &str, text: String) {
        let retained = self
            .thread_manager
            .provider_adapter_mut(thread_id)
            .map(|adapter| adapter.preprocess_stdout_chunk(&text))
            .unwrap_or_else(|| vec![text]);
        for fragment in retained {
            self.event_bus.emit_raw_stdout(thread_id, fragment.clone());
            let events = self
                .thread_manager
                .provider_adapter_mut(thread_id)
                .map(|adapter| adapter.parse_stdout_event(&fragment))
                .unwrap_or_default();
            self.emit_provider_partials(thread_id, events);
        }
    }

    pub fn emit_provider_stderr(&mut self, thread_id: &str, text: String) {
        self.event_bus.emit_raw_stderr(thread_id, text.clone());
        if let Some(running) = self.running_processes.get_mut(thread_id) {
            append_provider_stderr(&mut running.stderr, &mut running.stderr_truncated, &text);
        }
        let events = self
            .thread_manager
            .provider_adapter_mut(thread_id)
            .map(|adapter| adapter.parse_stderr_event(&text))
            .unwrap_or_default();
        self.emit_provider_partials(thread_id, events);
    }

    pub fn complete_provider_process(
        &mut self,
        thread_id: &str,
        process_id: u32,
        status: ExitStatus,
    ) {
        let running = if self
            .running_processes
            .get(thread_id)
            .is_some_and(|running| running.process_id == process_id)
        {
            self.running_processes.remove(thread_id)
        } else {
            None
        };
        let Some(running) = running else {
            return;
        };
        // A completed provider process means the provider turn is over. This
        // is distinct from a provider's child shell timing out while the
        // provider process remains alive, which does not reach this path.
        self.tool_request_broker.clear_thread(thread_id);
        let purpose = running.purpose;
        let had_provider_error = running.had_provider_error;
        let prepare_assistant_output = running.prepare_assistant_output.clone();
        let prepare_assistant_output_truncated = running.prepare_assistant_output_truncated;

        let prepare_missing_provider_session_id = purpose == RunningProviderProcessPurpose::Prepare
            && self
                .thread_manager
                .provider_state(thread_id)
                .and_then(|state| state.provider_session_id.as_deref())
                .is_none();
        let prepare_ack_invalid = purpose == RunningProviderProcessPurpose::Prepare
            && !prepare_missing_provider_session_id
            && (prepare_assistant_output_truncated
                || prepare_assistant_output.trim() != "PEDELEC_PREPARED");

        if purpose == RunningProviderProcessPurpose::Prepare
            && (had_provider_error
                || !status.success()
                || prepare_missing_provider_session_id
                || prepare_ack_invalid)
        {
            self.discard_failed_prepare_provider_session(thread_id);
        }

        let Ok(thread) = self.thread_manager.thread_mut(thread_id) else {
            return;
        };
        if thread.process_id == Some(process_id) {
            thread.process_id = None;
        }
        if matches!(thread.status, ThreadStatus::Ended | ThreadStatus::Stopping) {
            thread.updated_at = Utc::now();
            return;
        }

        if status.success() {
            let is_prepare = purpose == RunningProviderProcessPurpose::Prepare;
            if had_provider_error && !is_prepare {
                self.tool_request_broker.clear_thread(thread_id);
                thread.updated_at = Utc::now();
                return;
            }
            thread.status = ThreadStatus::Idle;
            thread.updated_at = Utc::now();
            if prepare_missing_provider_session_id && !had_provider_error {
                self.tool_request_broker.clear_thread(thread_id);
                self.emit_thread_provider_error(
                    thread_id,
                    PedelecError::with_details(
                        error_codes::PREPARE_SESSION_ID_MISSING,
                        "provider session id was not found after prepare",
                        serde_json::json!({ "threadId": thread_id }),
                    ),
                );
            } else if prepare_ack_invalid && !had_provider_error {
                let mut details = serde_json::json!({
                    "threadId": thread_id,
                    "provider": provider_code_as_str(&thread.provider),
                    "assistantOutput": prepare_assistant_output
                });
                if prepare_assistant_output_truncated {
                    details["assistantOutputTruncated"] = Value::Bool(true);
                }
                self.emit_thread_provider_error(
                    thread_id,
                    PedelecError::with_details(
                        error_codes::PREPARE_ACK_INVALID,
                        "provider did not acknowledge session preparation",
                        details,
                    ),
                );
            }
            if !had_provider_error || is_prepare {
                self.event_bus
                    .emit_status_changed(thread_id, ThreadStatus::Idle);
            }
        } else {
            let is_prepare = purpose == RunningProviderProcessPurpose::Prepare;
            thread.status = if is_prepare {
                ThreadStatus::Idle
            } else {
                ThreadStatus::Error
            };
            thread.updated_at = Utc::now();
            self.tool_request_broker.clear_thread(thread_id);
            if had_provider_error {
                if is_prepare {
                    self.event_bus
                        .emit_status_changed(thread_id, ThreadStatus::Idle);
                }
                return;
            }
            let next_status = thread.status.clone();
            let stderr_message = running.stderr.trim().to_string();
            let mut details = serde_json::json!({
                "threadId": thread_id,
                "processId": process_id,
                "exitCode": status.code()
            });
            let message = if stderr_message.is_empty() {
                "provider command failed"
            } else {
                if let Some(details) = details.as_object_mut() {
                    details.insert("stderr".to_string(), Value::String(running.stderr));
                    if running.stderr_truncated {
                        details.insert("stderrTruncated".to_string(), Value::Bool(true));
                    }
                }
                stderr_message.as_str()
            };
            let error =
                PedelecError::with_details(error_codes::PROVIDER_COMMAND_FAILED, message, details);
            if is_prepare {
                self.emit_thread_provider_error(thread_id, error);
                self.event_bus.emit_status_changed(thread_id, next_status);
            } else {
                self.event_bus.emit_status_changed(thread_id, next_status);
                self.emit_thread_provider_error(thread_id, error);
            }
        }
    }

    pub fn fail_provider_process_wait(&mut self, thread_id: &str, process_id: u32, err: String) {
        let purpose = if self
            .running_processes
            .get(thread_id)
            .is_some_and(|running| running.process_id == process_id)
        {
            self.running_processes
                .remove(thread_id)
                .map(|running| running.purpose)
        } else {
            None
        };
        if purpose == Some(RunningProviderProcessPurpose::Prepare) {
            self.discard_failed_prepare_provider_session(thread_id);
        }
        if purpose.is_some() {
            self.tool_request_broker.clear_thread(thread_id);
        }
        if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
            if thread.process_id == Some(process_id) {
                thread.process_id = None;
            }
            if !matches!(thread.status, ThreadStatus::Ended | ThreadStatus::Stopping) {
                let is_prepare = purpose == Some(RunningProviderProcessPurpose::Prepare);
                thread.status = if is_prepare {
                    ThreadStatus::Idle
                } else {
                    ThreadStatus::Error
                };
                thread.updated_at = Utc::now();
                let next_status = thread.status.clone();
                let error = PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "provider command wait failed",
                    serde_json::json!({
                        "threadId": thread_id,
                        "processId": process_id,
                        "error": err
                    }),
                );
                if is_prepare {
                    self.emit_thread_provider_error(thread_id, error);
                    self.event_bus.emit_status_changed(thread_id, next_status);
                } else {
                    self.event_bus.emit_status_changed(thread_id, next_status);
                    self.emit_thread_provider_error(thread_id, error);
                }
            }
        }
    }

    pub fn running_process_id(&self, thread_id: &str) -> Option<u32> {
        self.running_processes
            .get(thread_id)
            .map(|running| running.process_id)
    }

    pub fn running_process_count(&self) -> usize {
        self.running_processes.len()
    }

    fn update_provider_session_id(&mut self, thread_id: &str, provider_session_id: String) {
        let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) else {
            return;
        };
        if provider_state.provider_session_id.as_deref() == Some(provider_session_id.as_str()) {
            return;
        }
        provider_state.provider_session_id = Some(provider_session_id.clone());
        self.event_bus
            .emit_provider_session_id_updated(thread_id, provider_session_id);
    }

    fn emit_provider_partials(&mut self, thread_id: &str, events: Vec<ThreadEventPartial>) {
        for event in events {
            match event {
                ThreadEventPartial::AssistantMessage { text } => {
                    if let Some(running) = self.running_processes.get_mut(thread_id) {
                        if running.purpose == RunningProviderProcessPurpose::Prepare {
                            append_prepare_assistant_output(
                                &mut running.prepare_assistant_output,
                                &mut running.prepare_assistant_output_truncated,
                                &text,
                            );
                        }
                    }
                    self.event_bus.emit_assistant_message(thread_id, text);
                }
                ThreadEventPartial::ProviderSessionIdUpdated {
                    provider_session_id,
                } => self.update_provider_session_id(thread_id, provider_session_id),
                ThreadEventPartial::ProviderError { error } => {
                    if let Some(running) = self.running_processes.get_mut(thread_id) {
                        running.had_provider_error = true;
                    }
                    if let Ok(thread) = self.thread_manager.thread_mut(thread_id) {
                        thread.status = ThreadStatus::Error;
                    }
                    self.event_bus
                        .emit_status_changed(thread_id, ThreadStatus::Error);
                    self.emit_thread_provider_error(thread_id, error);
                }
            }
        }
    }

    fn emit_thread_provider_error(&mut self, thread_id: &str, error: PedelecError) {
        let Ok(thread) = self.thread_manager.thread(thread_id) else {
            return;
        };
        self.event_bus
            .emit_provider_error(thread_id, thread.provider.clone(), error);
    }

    fn discard_failed_prepare_provider_session(&mut self, thread_id: &str) {
        if let Some(provider_state) = self.thread_manager.provider_state_mut(thread_id) {
            provider_state.provider_session_id = None;
            provider_state.has_user_message = false;
        }
    }

    fn stop_running_process(&mut self, thread_id: &str) {
        let Some(running) = self.running_processes.remove(thread_id) else {
            return;
        };

        // The provider waiter owns the child wait/reap path. Mark the
        // cancellation before terminating it so reader/waiter workers skip
        // callbacks that would otherwise need Core's runtime mutex.
        running.termination.cancel();
        let killed_directly = running
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.as_mut().map(|child| child.kill().is_ok()))
            .unwrap_or(false);
        if !killed_directly {
            let _ = kill_process_by_id(running.process_id);
        }
        running.termination.wait_completed();
    }

    pub fn end_thread(&mut self, input: EndThreadInput) -> Result<(), PedelecError> {
        self.invalidate_asset_uploads_for_thread(&input.thread_id);
        self.invalidate_asset_downloads_for_thread(&input.thread_id);
        {
            let thread = self.thread_manager.thread_mut(&input.thread_id)?;
            if thread.status != ThreadStatus::Ended {
                thread.status = ThreadStatus::Stopping;
                thread.updated_at = Utc::now();
                self.event_bus
                    .emit_status_changed(&input.thread_id, ThreadStatus::Stopping);
            }
        }

        self.stop_running_process(&input.thread_id);
        self.tool_request_broker.clear_thread(&input.thread_id);
        self.tool_registry.remove(&input.thread_id);

        if let Ok(thread) = self.thread_manager.thread_mut(&input.thread_id) {
            thread.status = ThreadStatus::Ended;
            thread.process_id = None;
            thread.updated_at = Utc::now();
        }
        self.event_bus
            .emit_status_changed(&input.thread_id, ThreadStatus::Ended);
        self.event_bus.emit_ended(&input.thread_id);
        self.event_bus.unregister_thread_log(&input.thread_id);
        Ok(())
    }

    pub fn cleanup_stale_sandboxes_for_app_start(&self) -> Vec<PedelecError> {
        self.sandbox_manager.remove_all_thread_sandboxes()
    }

    pub fn cleanup_for_app_exit(&mut self) -> Vec<PedelecError> {
        let thread_ids = self.thread_manager.thread_ids();
        for thread_id in thread_ids {
            let _ = self.end_thread(EndThreadInput { thread_id });
        }

        self.sandbox_manager.remove_all_thread_sandboxes()
    }
    pub fn active_process_id(&self, thread_id: &str) -> Option<u32> {
        self.thread_manager
            .thread(thread_id)
            .ok()
            .and_then(|thread| thread.process_id)
    }

    pub fn thread_status(&self, thread_id: &str) -> Option<ThreadStatus> {
        self.thread_manager
            .thread(thread_id)
            .ok()
            .map(|thread| thread.status.clone())
    }

    pub fn provider_state(&self, thread_id: &str) -> Option<&ProviderAdapterState> {
        self.thread_manager.provider_state(thread_id)
    }

    pub fn event_log_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.event_bus.event_log_path(thread_id)
    }

    pub fn thread_sandbox_path(&self, thread_id: &str) -> Option<PathBuf> {
        self.thread_manager
            .thread(thread_id)
            .ok()
            .map(|thread| thread.sandbox_path.clone())
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
        let registration = self.tool_request_broker.begin_or_join(
            input.thread_id.clone(),
            input.tool_name.clone(),
            normalized.args.clone(),
            normalized.timeout_ms,
        )?;

        if let ToolInvocationRegistration::Created(wait) = &registration {
            let thread = self.thread_manager.thread_mut(&input.thread_id)?;
            thread.status = ThreadStatus::WaitingToolResult;
            thread.updated_at = Utc::now();
            self.event_bus
                .emit_status_changed(&input.thread_id, ThreadStatus::WaitingToolResult);
            self.event_bus.emit_tool_call(
                &input.thread_id,
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
                self.event_bus
                    .emit_status_changed(&pending.request.thread_id, ThreadStatus::Running);
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
                self.event_bus
                    .emit_status_changed(&input.thread_id, ThreadStatus::Running);
            }
        }
        self.event_bus.emit_tool_result(
            &input.thread_id,
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
        self.thread_manager.thread(&input.thread_id)?;
        Ok(self.event_bus.subscribe(&input.thread_id))
    }

    pub fn subscribe_all_threads(&mut self) -> mpsc::Receiver<ThreadEvent> {
        self.event_bus.subscribe_all()
    }
}

fn append_provider_stderr(stderr: &mut String, truncated: &mut bool, text: &str) {
    stderr.push_str(text);
    if stderr.len() <= MAX_PROVIDER_STDERR_BYTES {
        return;
    }

    let mut drop_until = stderr.len() - MAX_PROVIDER_STDERR_BYTES;
    while !stderr.is_char_boundary(drop_until) {
        drop_until += 1;
    }
    stderr.drain(..drop_until);
    *truncated = true;
}

fn append_prepare_assistant_output(output: &mut String, truncated: &mut bool, text: &str) {
    output.push_str(text);
    if output.len() <= MAX_PREPARE_ASSISTANT_OUTPUT_BYTES {
        return;
    }

    let mut drop_until = output.len() - MAX_PREPARE_ASSISTANT_OUTPUT_BYTES;
    while !output.is_char_boundary(drop_until) {
        drop_until += 1;
    }
    output.drain(..drop_until);
    *truncated = true;
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
        let mut provider_scan = scan_external_providers(Some(path_value.clone()));
        apply_provider_bootstrap_capabilities(&mut provider_scan, Some(&path_value));
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
            runtime: Arc::new(Mutex::new(CoreRuntime::new())),
        }
    }

    pub fn runtime(&self) -> SharedCoreRuntime {
        Arc::clone(&self.runtime)
    }
}

pub type SharedCoreRuntime = Arc<Mutex<CoreRuntime>>;

#[derive(Debug, Default)]
pub struct ThreadManager {
    threads: HashMap<String, ThreadState>,
    provider_states: HashMap<String, ProviderAdapterState>,
    provider_adapters: HashMap<String, ProviderAdapterInstance>,
    next_thread_number: u64,
}

impl ThreadManager {
    fn next_thread_id(&mut self) -> Result<String, PedelecError> {
        if self.next_thread_number >= THREAD_ID_MAX_COUNTER {
            return Err(PedelecError::new(
                error_codes::SANDBOX_CREATE_FAILED,
                "thread id counter was exhausted",
            ));
        }

        self.next_thread_number += 1;
        let encoded = to_base36(self.next_thread_number);
        if encoded.len() > THREAD_ID_BASE36_MAX_WIDTH {
            return Err(PedelecError::new(
                error_codes::SANDBOX_CREATE_FAILED,
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

    pub fn insert_thread(&mut self, state: ThreadState, provider_state: ProviderAdapterState) {
        let thread_id = state.thread_id.clone();
        let provider_adapter = ProviderAdapterInstance::new(state.provider.clone());
        self.threads.insert(thread_id.clone(), state);
        self.provider_states
            .insert(thread_id.clone(), provider_state);
        self.provider_adapters.insert(thread_id, provider_adapter);
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

    pub fn provider_state(&self, thread_id: &str) -> Option<&ProviderAdapterState> {
        self.provider_states.get(thread_id)
    }

    fn provider_state_mut(&mut self, thread_id: &str) -> Option<&mut ProviderAdapterState> {
        self.provider_states.get_mut(thread_id)
    }

    fn provider_adapter(&self, thread_id: &str) -> Result<&ProviderAdapterInstance, PedelecError> {
        self.provider_adapters.get(thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::PROVIDER_NOT_FOUND,
                "provider adapter was not found for thread",
                serde_json::json!({ "threadId": thread_id }),
            )
        })
    }

    fn provider_adapter_mut(&mut self, thread_id: &str) -> Option<&mut ProviderAdapterInstance> {
        self.provider_adapters.get_mut(thread_id)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SandboxManager {
    sandbox_root: Option<PathBuf>,
}

impl SandboxManager {
    pub fn with_sandbox_root(sandbox_root: impl Into<PathBuf>) -> Self {
        Self {
            sandbox_root: Some(sandbox_root.into()),
        }
    }

    pub fn thread_sandbox_exists(&self, thread_id: &str) -> Result<bool, PedelecError> {
        let safe_thread_id = sanitize_thread_id(thread_id)?;
        Ok(self.sandbox_root()?.join(safe_thread_id).exists())
    }

    pub fn create_thread_sandbox(&self, thread_id: &str) -> Result<PathBuf, PedelecError> {
        let safe_thread_id = sanitize_thread_id(thread_id)?;
        let sandbox_root = self.sandbox_root()?;
        let sandbox_path = sandbox_root.join(safe_thread_id);

        if sandbox_path.exists() {
            return Err(PedelecError::with_details(
                error_codes::SANDBOX_CREATE_FAILED,
                "thread sandbox already exists",
                serde_json::json!({ "sandboxPath": path_for_external_use(&sandbox_path) }),
            ));
        }

        let create_result = (|| {
            fs::create_dir_all(&sandbox_path).map_err(|err| {
                sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "cannot create thread sandbox",
                    &sandbox_path,
                    err,
                )
            })?;

            self.create_sandbox_subdirectories(&sandbox_path)?;

            Ok(sandbox_path.clone())
        })();

        if create_result.is_err() {
            let _ = fs::remove_dir_all(&sandbox_path);
        }

        create_result
    }

    pub fn prepare_custom_sandbox(
        &self,
        custom_path: impl AsRef<Path>,
    ) -> Result<PathBuf, PedelecError> {
        let custom_path = custom_path.as_ref();
        let managed_root = self.sandbox_root()?;
        if !custom_path.is_absolute() {
            return Err(sandbox_path_invalid_error(
                "custom sandbox path must be absolute",
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
                sandbox_path_invalid_io_error(
                    "cannot inspect custom sandbox path",
                    custom_path,
                    err,
                )
            })?;
            if !metadata.is_dir() {
                return Err(sandbox_path_invalid_error(
                    "custom sandbox path is not a directory",
                    custom_path,
                    &managed_root,
                ));
            }
        } else {
            fs::create_dir_all(custom_path).map_err(|err| {
                sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "cannot create custom sandbox directory",
                    custom_path,
                    err,
                )
            })?;
        }

        let resolved_custom_path = custom_path.canonicalize().map_err(|err| {
            sandbox_path_invalid_io_error(
                "cannot canonicalize custom sandbox path",
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

        self.create_sandbox_subdirectories(&resolved_custom_path)?;
        Ok(resolved_custom_path)
    }

    fn ensure_custom_sandbox_config(
        &self,
        sandbox_path: &Path,
        sdk_version: &str,
        origin: &str,
    ) -> Result<(), PedelecError> {
        #[derive(Serialize)]
        struct SandboxConfig<'a> {
            #[serde(rename = "sdk-version")]
            sdk_version: &'a str,
            origin: &'a str,
        }

        let config_path = sandbox_path.join(SANDBOX_CONFIG_FILE);
        let contents = serde_json::to_vec_pretty(&SandboxConfig {
            sdk_version,
            origin,
        })
        .expect("sandbox config serialization should not fail");

        match fs::symlink_metadata(&config_path) {
            Ok(metadata) if metadata.is_file() => return Ok(()),
            Ok(_) => {
                return Err(sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "sandbox config path is not a regular file",
                    &config_path,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "sandbox config path is occupied",
                    ),
                ));
            }
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "cannot inspect sandbox config path",
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
                    Ok(_) => Err(sandbox_io_error(
                        error_codes::SANDBOX_CREATE_FAILED,
                        "sandbox config path is not a regular file",
                        &config_path,
                        io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "sandbox config path is occupied",
                        ),
                    )),
                    Err(metadata_err) => Err(sandbox_io_error(
                        error_codes::SANDBOX_CREATE_FAILED,
                        "cannot inspect sandbox config path",
                        &config_path,
                        metadata_err,
                    )),
                };
            }
            Err(err) => {
                return Err(sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "cannot create sandbox config",
                    &config_path,
                    err,
                ));
            }
        };

        if let Err(err) = file.write_all(&contents).and_then(|_| file.flush()) {
            drop(file);
            let _ = fs::remove_file(&config_path);
            return Err(sandbox_io_error(
                error_codes::SANDBOX_CREATE_FAILED,
                "cannot write sandbox config",
                &config_path,
                err,
            ));
        }

        Ok(())
    }

    fn create_sandbox_subdirectories(&self, sandbox_path: &Path) -> Result<(), PedelecError> {
        for subdir in SANDBOX_SUBDIRS {
            let path = sandbox_path.join(subdir);
            fs::create_dir_all(&path).map_err(|err| {
                sandbox_io_error(
                    error_codes::SANDBOX_CREATE_FAILED,
                    "cannot create thread sandbox subdirectory",
                    &path,
                    err,
                )
            })?;
        }
        Ok(())
    }

    pub fn create_thread_sandbox_with<T>(
        &self,
        thread_id: &str,
        initialize: impl FnOnce(&Path) -> Result<T, PedelecError>,
    ) -> Result<(PathBuf, T), PedelecError> {
        let sandbox_path = self.create_thread_sandbox(thread_id)?;

        match initialize(&sandbox_path) {
            Ok(value) => Ok((sandbox_path, value)),
            Err(err) => {
                let _ = self.remove_thread_sandbox(&sandbox_path);
                Err(err)
            }
        }
    }

    pub fn remove_thread_sandbox(
        &self,
        sandbox_path: impl AsRef<Path>,
    ) -> Result<(), PedelecError> {
        let sandbox_path = sandbox_path.as_ref();
        if !sandbox_path.exists() {
            return Ok(());
        }

        self.ensure_path_inside_sandbox_root(sandbox_path)?;
        fs::remove_dir_all(sandbox_path).map_err(|err| {
            sandbox_io_error(
                error_codes::SANDBOX_REMOVE_FAILED,
                "cannot remove thread sandbox",
                sandbox_path,
                err,
            )
        })
    }

    pub fn remove_thread_sandbox_with_retry(
        &self,
        sandbox_path: impl AsRef<Path>,
    ) -> Result<(), PedelecError> {
        let sandbox_path = sandbox_path.as_ref();
        let mut last_error = None;
        for attempt in 0..SANDBOX_REMOVE_MAX_ATTEMPTS {
            match self.remove_thread_sandbox(sandbox_path) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_error = Some(err);
                    if attempt + 1 < SANDBOX_REMOVE_MAX_ATTEMPTS {
                        std::thread::sleep(SANDBOX_REMOVE_RETRY_DELAY);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            PedelecError::with_details(
                error_codes::SANDBOX_REMOVE_FAILED,
                "cannot remove thread sandbox",
                serde_json::json!({ "path": path_for_external_use(sandbox_path) }),
            )
        }))
    }

    pub fn remove_all_thread_sandboxes(&self) -> Vec<PedelecError> {
        let sandbox_root = match self.sandbox_root() {
            Ok(root) => root,
            Err(err) => return vec![err],
        };
        if !sandbox_root.exists() {
            return vec![];
        }

        let entries = match fs::read_dir(&sandbox_root) {
            Ok(entries) => entries,
            Err(err) => {
                return vec![sandbox_io_error(
                    error_codes::SANDBOX_REMOVE_FAILED,
                    "cannot read sandbox root",
                    &sandbox_root,
                    err,
                )];
            }
        };

        let mut errors = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    errors.push(sandbox_io_error(
                        error_codes::SANDBOX_REMOVE_FAILED,
                        "cannot read sandbox root entry",
                        &sandbox_root,
                        err,
                    ));
                    continue;
                }
            };

            let path = entry.path();
            let is_dir = match entry.file_type() {
                Ok(file_type) => file_type.is_dir(),
                Err(err) => {
                    errors.push(sandbox_io_error(
                        error_codes::SANDBOX_REMOVE_FAILED,
                        "cannot inspect sandbox root entry",
                        &path,
                        err,
                    ));
                    continue;
                }
            };
            if !is_dir {
                continue;
            }

            if let Err(err) = self.remove_thread_sandbox_with_retry(&path) {
                errors.push(err);
            }
        }

        errors
    }

    fn sandbox_root(&self) -> Result<PathBuf, PedelecError> {
        match &self.sandbox_root {
            Some(root) => Ok(root.clone()),
            None => dirs::home_dir()
                .map(|home| home.join(".pedelec").join("sandbox"))
                .ok_or_else(|| {
                    PedelecError::new(
                        error_codes::SANDBOX_PATH_INVALID,
                        "cannot resolve user home directory for sandbox root",
                    )
                }),
        }
    }

    fn ensure_path_inside_sandbox_root(&self, path: &Path) -> Result<(), PedelecError> {
        let sandbox_root = self.sandbox_root()?;
        let root = sandbox_root.canonicalize().map_err(|err| {
            sandbox_io_error(
                error_codes::SANDBOX_PATH_INVALID,
                "cannot canonicalize sandbox root",
                &sandbox_root,
                err,
            )
        })?;
        let target = path.canonicalize().map_err(|err| {
            sandbox_io_error(
                error_codes::SANDBOX_PATH_INVALID,
                "cannot canonicalize thread sandbox",
                path,
                err,
            )
        })?;

        if !target.starts_with(root) {
            return Err(PedelecError::with_details(
                error_codes::SANDBOX_PATH_INVALID,
                "thread sandbox is outside sandbox root",
                serde_json::json!({ "sandboxPath": path_for_external_use(path) }),
            ));
        }

        Ok(())
    }
}

fn initialize_generated_skills(
    sandbox: &Path,
    skills_input: Option<&CreateThreadSkillsInput>,
) -> Result<(Vec<SkillFile>, ToolRegistry), PedelecError> {
    let skills_dir = sandbox.join("skills");
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

pub fn inspect_sandbox_folder(path: &Path) -> Result<SandboxFolderInspection, PedelecError> {
    let entries = fs::read_dir(path).map_err(|err| {
        sandbox_io_error(
            error_codes::DIRECTORY_PICKER_FAILED,
            "cannot inspect selected sandbox folder",
            path,
            err,
        )
    })?;
    let mut is_empty_folder = true;
    let mut has_sandbox_config = false;

    for entry in entries {
        let entry = entry.map_err(|err| {
            sandbox_io_error(
                error_codes::DIRECTORY_PICKER_FAILED,
                "cannot inspect selected sandbox folder entry",
                path,
                err,
            )
        })?;
        is_empty_folder = false;
        if entry.file_name() == OsStr::new(SANDBOX_CONFIG_FILE) {
            has_sandbox_config = entry
                .file_type()
                .map_err(|err| {
                    sandbox_io_error(
                        error_codes::DIRECTORY_PICKER_FAILED,
                        "cannot inspect selected sandbox config entry",
                        &entry.path(),
                        err,
                    )
                })?
                .is_file();
        }
    }

    Ok(SandboxFolderInspection {
        is_empty_folder,
        has_sandbox_config,
    })
}

fn thread_event_log_path(sandbox_path: &Path, thread_id: &str) -> PathBuf {
    sandbox_path
        .join("logs")
        .join(format!("events-{thread_id}-{}.jsonl", Uuid::new_v4()))
}

fn sandbox_path_invalid_error(
    message: &'static str,
    custom_path: &Path,
    managed_root: &Path,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::SANDBOX_PATH_INVALID,
        message,
        serde_json::json!({
            "sandboxPath": path_for_external_use(custom_path),
            "managedSandboxRoot": path_for_external_use(managed_root),
        }),
    )
}

fn sandbox_path_invalid_io_error(
    message: &'static str,
    path: &Path,
    err: std::io::Error,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::SANDBOX_PATH_INVALID,
        message,
        serde_json::json!({
            "sandboxPath": path_for_external_use(path),
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
                sandbox_path_invalid_io_error("cannot resolve current directory", path, err)
            })?
            .join(path)
    };
    let normalized_path = normalize_absolute_path(&absolute_path)?;
    let mut existing_ancestor = normalized_path.clone();
    let mut missing_components = Vec::new();

    while !existing_ancestor.exists() {
        let component = existing_ancestor.file_name().ok_or_else(|| {
            PedelecError::with_details(
                error_codes::SANDBOX_PATH_INVALID,
                "cannot resolve sandbox path ancestor",
                serde_json::json!({ "path": path_for_external_use(path) }),
            )
        })?;
        missing_components.push(component.to_os_string());
        if !existing_ancestor.pop() {
            return Err(PedelecError::with_details(
                error_codes::SANDBOX_PATH_INVALID,
                "cannot resolve sandbox path ancestor",
                serde_json::json!({ "path": path_for_external_use(path) }),
            ));
        }
    }

    let mut resolved = existing_ancestor.canonicalize().map_err(|err| {
        sandbox_path_invalid_io_error(
            "cannot canonicalize sandbox path ancestor",
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
            error_codes::SANDBOX_PATH_INVALID,
            "sandbox path must be absolute",
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
        return Err(sandbox_path_invalid_error(
            "custom sandbox path overlaps the managed sandbox root",
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
        let tools_json_path = skills_dir.as_ref().join("tools.json");
        if !tools_json_path.exists() {
            return Ok(Self::default());
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
    pub fn begin_or_join(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
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

        Ok(ToolInvocationRegistration::Created(
            self.create_new(thread_id, tool_name, args, timeout_ms),
        ))
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

        let wait = self.create_new(thread_id, tool_name, args, timeout_ms);
        Ok((wait.request_id, wait.result_rx))
    }

    fn create_new(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
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
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::StatusChanged {
                seq,
                thread_id: thread_id.to_string(),
                status,
            },
        );
    }

    pub fn emit_raw_stdout(&mut self, thread_id: &str, text: String) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::RawStdout {
                seq,
                thread_id: thread_id.to_string(),
                text,
            },
        );
    }

    pub fn emit_raw_stderr(&mut self, thread_id: &str, text: String) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::RawStderr {
                seq,
                thread_id: thread_id.to_string(),
                text,
            },
        );
    }

    pub fn emit_assistant_message(&mut self, thread_id: &str, text: String) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::AssistantMessage {
                seq,
                thread_id: thread_id.to_string(),
                text,
            },
        );
    }

    pub fn emit_tool_call(
        &mut self,
        thread_id: &str,
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
                request_id: request_id.to_string(),
                tool_name: tool_name.to_string(),
                args,
            },
        );
    }

    pub fn emit_tool_result(
        &mut self,
        thread_id: &str,
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
                request_id: request_id.to_string(),
                tool_name: tool_name.to_string(),
                result,
            },
        );
    }

    pub fn emit_provider_command_started(
        &mut self,
        thread_id: &str,
        process_id: u32,
        command: &CommandSpec,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::ProviderCommandStarted {
                seq,
                thread_id: thread_id.to_string(),
                process_id,
                program: command.program.clone(),
                args: command.args.clone(),
                cwd: path_for_external_use(&command.cwd),
                prompt: command.prompt.clone(),
            },
        );
    }

    pub fn emit_provider_session_id_updated(
        &mut self,
        thread_id: &str,
        provider_session_id: String,
    ) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::ProviderSessionIdUpdated {
                seq,
                thread_id: thread_id.to_string(),
                provider_session_id,
            },
        );
    }

    pub fn emit_provider_error(
        &mut self,
        thread_id: &str,
        provider: ProviderCode,
        error: PedelecError,
    ) {
        self.emit_error(thread_id, ThreadErrorSource::Provider { provider }, error);
    }

    pub fn emit_core_error(&mut self, thread_id: &str, error: PedelecError) {
        self.emit_error(thread_id, ThreadErrorSource::Core, error);
    }

    fn emit_error(&mut self, thread_id: &str, source: ThreadErrorSource, error: PedelecError) {
        let seq = self.next_seq(thread_id);
        self.emit(
            thread_id,
            ThreadEvent::Error {
                seq,
                thread_id: thread_id.to_string(),
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

fn collect_sandbox_assets(
    root: &Path,
    directory: &Path,
    assets: &mut Vec<SandboxAsset>,
) -> Result<(), PedelecError> {
    let relative_directory = asset_relative_path(root, directory)?;
    let entries = fs::read_dir(directory).map_err(|err| {
        PedelecError::with_details(
            error_codes::ASSET_LIST_FAILED,
            "failed to read sandbox assets",
            serde_json::json!({ "path": relative_directory, "error": err.to_string() }),
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to read sandbox asset",
                serde_json::json!({ "path": relative_directory, "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        let public_path = asset_relative_path(root, &path)?;
        let file_type = entry.file_type().map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to inspect sandbox asset",
                serde_json::json!({ "path": public_path, "error": err.to_string() }),
            )
        })?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().into_string().map_err(|_| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "sandbox asset filename cannot be encoded",
                serde_json::json!({ "path": public_path }),
            )
        })?;
        if name.starts_with(".pedelec-") {
            continue;
        }
        if file_type.is_dir() {
            collect_sandbox_assets(root, &path, assets)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to read sandbox asset metadata",
                serde_json::json!({ "path": public_path, "error": err.to_string() }),
            )
        })?;
        let modified_at = metadata
            .modified()
            .map_err(|err| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "failed to read sandbox asset modified time",
                    serde_json::json!({ "path": public_path, "error": err.to_string() }),
                )
            })?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "sandbox asset modified time predates Unix epoch",
                    serde_json::json!({ "path": public_path }),
                )
            })?
            .as_millis();
        let modified_at = i64::try_from(modified_at).map_err(|_| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "sandbox asset modified time is out of range",
                serde_json::json!({ "path": public_path }),
            )
        })?;
        assets.push(SandboxAsset {
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
            "sandbox asset path is outside the asset root",
        )
    })?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => components.push(part.to_str().ok_or_else(|| {
                PedelecError::new(
                    error_codes::ASSET_LIST_FAILED,
                    "sandbox asset path cannot be encoded",
                )
            })?),
            _ => {
                return Err(PedelecError::new(
                    error_codes::ASSET_LIST_FAILED,
                    "sandbox asset path is invalid",
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
    let root = thread.sandbox_path.join("assets");
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
    pub const SANDBOX_CREATE_FAILED: &str = "SANDBOX_CREATE_FAILED";
    pub const SANDBOX_REMOVE_FAILED: &str = "SANDBOX_REMOVE_FAILED";
    pub const SANDBOX_PATH_INVALID: &str = "SANDBOX_PATH_INVALID";
    pub const SANDBOX_OPEN_FAILED: &str = "SANDBOX_OPEN_FAILED";
    pub const DIRECTORY_PICKER_FAILED: &str = "DIRECTORY_PICKER_FAILED";
    pub const TOOLS_JSON_NOT_FOUND: &str = "TOOLS_JSON_NOT_FOUND";
    pub const TOOLS_JSON_INVALID: &str = "TOOLS_JSON_INVALID";
    pub const TOOLS_MANIFEST_INVALID: &str = "TOOLS_MANIFEST_INVALID";
    pub const TOOLS_MD_NOT_FOUND: &str = "TOOLS_MD_NOT_FOUND";
    pub const TOOL_NOT_FOUND: &str = "TOOL_NOT_FOUND";
    pub const TOOL_ARGS_INVALID: &str = "TOOL_ARGS_INVALID";
    pub const PEDELEC_THREAD_ID_NOT_FOUND: &str = "PEDELEC_THREAD_ID_NOT_FOUND";
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
    bootstrap_capabilities: Option<ProviderBootstrapCapabilities>,
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

fn apply_provider_bootstrap_capabilities(
    provider_scan: &mut HashMap<ProviderCode, ProviderCli>,
    path_value: Option<&OsString>,
) {
    for provider in external_provider_codes() {
        let capability = provider_bootstrap_capability_for_scan(
            &provider,
            provider_scan.get(&provider),
            path_value,
        );
        if let Some(scan) = provider_scan.get_mut(&provider) {
            scan.bootstrap_capabilities = Some(capability);
        }
    }
}

fn provider_bootstrap_capability_for_scan(
    provider: &ProviderCode,
    scan: Option<&ProviderCli>,
    path_value: Option<&OsString>,
) -> ProviderBootstrapCapabilities {
    let claude_flag_supported = scan.is_some_and(|scan| {
        provider_cli_supports_flag(scan, path_value, "--append-system-prompt", false)
    });
    let opencode_flag_supported =
        scan.is_some_and(|scan| provider_cli_supports_flag(scan, path_value, "--agent", true));
    provider_bootstrap_capability_from_probe(
        provider,
        scan,
        claude_flag_supported,
        opencode_flag_supported,
    )
}

fn provider_bootstrap_capability_from_probe(
    provider: &ProviderCode,
    scan: Option<&ProviderCli>,
    claude_flag_supported: bool,
    opencode_flag_supported: bool,
) -> ProviderBootstrapCapabilities {
    let privileged_bootstrap = match provider {
        ProviderCode::Codex => ProviderBootstrapMode::CodexDeveloperInstructions,
        ProviderCode::Claude => {
            if claude_flag_supported {
                ProviderBootstrapMode::ClaudeAppendSystemPrompt
            } else {
                ProviderBootstrapMode::UserPromptFallback
            }
        }
        ProviderCode::OpenCode => {
            if opencode_flag_supported {
                ProviderBootstrapMode::OpenCodeInlineAgent
            } else {
                ProviderBootstrapMode::UserPromptFallback
            }
        }
        ProviderCode::Antigravity => scan
            .and_then(|scan| scan.version.as_ref())
            .filter(|version| antigravity_custom_agent_version_supported(version))
            .map(|_| ProviderBootstrapMode::AntigravityWorkspaceAgent)
            .unwrap_or(ProviderBootstrapMode::UserPromptFallback),
        ProviderCode::Cursor | ProviderCode::Ollama => ProviderBootstrapMode::UserPromptFallback,
    };
    ProviderBootstrapCapabilities {
        privileged_bootstrap,
    }
}

fn provider_cli_supports_flag(
    scan: &ProviderCli,
    path_value: Option<&OsString>,
    flag: &str,
    is_opencode_run_flag: bool,
) -> bool {
    if scan.path.is_none() || scan.version.is_none() {
        return false;
    }
    #[cfg(test)]
    {
        let _ = (path_value, flag, is_opencode_run_flag);
        true
    }
    #[cfg(not(test))]
    {
        let Some(path) = scan.path.as_deref() else {
            return false;
        };
        let mut command = provider_version_command(path, path_value);
        if is_opencode_run_flag {
            command.arg("run");
        }
        let output = command.arg("--help").output().ok();
        output.is_some_and(|output| {
            output.status.success()
                && format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
                .contains(flag)
        })
    }
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
    let recognized = candidates
        .into_iter()
        .filter(|path| is_provider_executable(path))
        .filter_map(|path| {
            provider_cli_version(&path, Some(path_value)).map(|version| (path, version))
        })
        .max_by(|(left_path, left), (right_path, right)| {
            left.cmp(right).then_with(|| left_path.cmp(right_path))
        });
    match recognized {
        Some((path, version)) => ProviderCli {
            path: Some(path),
            version: Some(version),
            error: None,
            bootstrap_capabilities: None,
        },
        None => ProviderCli {
            path: None,
            version: None,
            error: Some(
                "no provider CLI with a recognizable version was found in PATH".to_string(),
            ),
            bootstrap_capabilities: None,
        },
    }
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
    let output = provider_version_command(path, path_value)
        .arg("--version")
        .output();
    #[cfg(test)]
    let output = output.ok().or_else(|| {
        Some(std::process::Output {
            status: success_exit_status(),
            stdout: b"0.0.0".to_vec(),
            stderr: Vec::new(),
        })
    })?;
    #[cfg(not(test))]
    let output = output.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = parse_provider_version(&text);
    #[cfg(test)]
    {
        parsed.or_else(|| Some(ProviderVersion(vec![0])))
    }
    #[cfg(not(test))]
    {
        parsed
    }
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

#[cfg(test)]
fn success_exit_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
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
        return ProviderInfo {
            name: provider_display_name(&provider).to_string(),
            code: provider,
            scanned: scanned_complete,
            version: scanned.version.as_ref().map(provider_version_display),
            path: scanned.path.map(|path| path.to_string_lossy().to_string()),
            available: scanned.version.is_some(),
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

fn remove_codex_developer_instruction_override(args: &mut Vec<String>) {
    let mut index = 0;
    while index + 1 < args.len() {
        if args[index] == "-c" && args[index + 1].starts_with("developer_instructions=") {
            args.drain(index..=index + 1);
        } else {
            index += 1;
        }
    }
}

fn remove_agent_selector(args: &mut Vec<String>) {
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--agent" {
            args.remove(index);
            if index < args.len() {
                args.remove(index);
            }
        } else if args[index].starts_with("--agent=") {
            args.remove(index);
        } else {
            index += 1;
        }
    }
}

fn insert_agent_selector(mut args: Vec<String>, agent: &str) -> Vec<String> {
    let insertion_index = args.iter().position(|arg| arg == "-").unwrap_or(args.len());
    args.splice(
        insertion_index..insertion_index,
        ["--agent".to_string(), agent.to_string()],
    );
    args
}

fn merge_opencode_runtime_agent_config(command: &CommandSpec) -> Result<String, PedelecError> {
    let existing = command
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == OPENCODE_CONFIG_CONTENT_ENV)
        .map(|(_, value)| value.clone())
        .or_else(|| env::var(OPENCODE_CONFIG_CONTENT_ENV).ok());
    let mut config = match existing.as_deref() {
        Some(existing) => serde_json::from_str::<Value>(existing).map_err(|error| {
            PedelecError::with_details(
                error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
                "OPENCODE_CONFIG_CONTENT must be valid JSON",
                serde_json::json!({
                    "provider": "opencode",
                    "key": OPENCODE_CONFIG_CONTENT_ENV,
                    "error": error.to_string()
                }),
            )
        })?,
        None => Value::Object(serde_json::Map::new()),
    };
    let Some(config_object) = config.as_object_mut() else {
        return Err(PedelecError::with_details(
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
            "OPENCODE_CONFIG_CONTENT must contain a JSON object",
            serde_json::json!({
                "provider": "opencode",
                "key": OPENCODE_CONFIG_CONTENT_ENV
            }),
        ));
    };
    let agent_value = config_object
        .entry("agent")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(agents) = agent_value.as_object_mut() else {
        return Err(PedelecError::with_details(
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
            "OPENCODE_CONFIG_CONTENT.agent must contain a JSON object",
            serde_json::json!({
                "provider": "opencode",
                "key": OPENCODE_CONFIG_CONTENT_ENV,
                "field": "agent"
            }),
        ));
    };
    agents.insert(
        PEDELEC_OPENCODE_AGENT.to_string(),
        serde_json::json!({
            "mode": "primary",
            "prompt": build_pedelec_bootstrap_instruction()
        }),
    );
    serde_json::to_string(&config).map_err(|error| {
        PedelecError::with_details(
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID,
            "failed to serialize OPENCODE_CONFIG_CONTENT",
            serde_json::json!({
                "provider": "opencode",
                "key": OPENCODE_CONFIG_CONTENT_ENV,
                "error": error.to_string()
            }),
        )
    })
}

fn insert_antigravity_custom_agent_body(bootstrap: &str) -> String {
    format!(
        "---\nname: pedelec-runtime\ndescription: Pedelec host integration bootstrap for Pedelec-managed agent sessions.\nmainAgent: true\nsubagent: false\n---\n\n# System Prompt\n\n{bootstrap}\n"
    )
}

fn ensure_antigravity_custom_agent(sandbox_path: &Path) -> Result<(), PedelecError> {
    let agent_dir = sandbox_path.join(PEDELEC_ANTIGRAVITY_AGENT_DIR);
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

/// Restrict provider-native skill discovery without changing the provider's
/// sandbox, native tools, or the Pedelec App tool registry.
fn apply_provider_native_skills_policy(provider: &ProviderCode, command: &mut CommandSpec) {
    match provider {
        ProviderCode::Codex => {
            if !command
                .args
                .windows(2)
                .any(|args| args[0] == "-c" && args[1] == CODEX_SKILLS_INCLUDE_INSTRUCTIONS_CONFIG)
            {
                let insertion_index = command
                    .args
                    .iter()
                    .position(|arg| arg == "exec")
                    .map_or(0, |index| index + 1);
                command.args.splice(
                    insertion_index..insertion_index,
                    [
                        "-c".to_string(),
                        CODEX_SKILLS_INCLUDE_INSTRUCTIONS_CONFIG.to_string(),
                    ],
                );
            }
        }
        ProviderCode::Antigravity | ProviderCode::Claude => {
            if !command
                .args
                .iter()
                .any(|arg| arg == "--disable-slash-commands")
            {
                command.args.push("--disable-slash-commands".to_string());
            }
        }
        ProviderCode::OpenCode => {
            let existing_permission = command
                .env
                .iter()
                .rev()
                .find(|(key, _)| key == OPENCODE_PERMISSION_ENV)
                .map(|(_, value)| value.clone())
                .or_else(|| env::var(OPENCODE_PERMISSION_ENV).ok());

            // An invalid or non-object parent value is left untouched. This
            // avoids replacing a user's permission configuration with a
            // potentially broader or otherwise incompatible one.
            if let Some(permission) =
                build_opencode_permission_overlay(existing_permission.as_deref())
            {
                set_command_env(command, OPENCODE_PERMISSION_ENV, permission);
            }
        }
        ProviderCode::Cursor | ProviderCode::Ollama => {}
    }
}

fn build_opencode_permission_overlay(existing: Option<&str>) -> Option<String> {
    let mut permissions = match existing {
        Some(existing) => match serde_json::from_str::<Value>(existing).ok()? {
            Value::Object(permissions) => permissions,
            _ => return None,
        },
        None => serde_json::Map::new(),
    };

    permissions.insert("skill".to_string(), Value::String("deny".to_string()));
    serde_json::to_string(&Value::Object(permissions)).ok()
}

fn set_command_env(command: &mut CommandSpec, key: &str, value: String) {
    if let Some((_, existing_value)) = command
        .env
        .iter_mut()
        .find(|(candidate, _)| candidate == key)
    {
        *existing_value = value;
    } else {
        command.env.push((key.to_string(), value));
    }
}

fn build_provider_env(
    ctx: &RunPromptProviderContext,
) -> Result<Vec<(String, String)>, PedelecError> {
    let provider = provider_code_as_str(&ctx.thread.provider).to_string();
    let mut env = vec![
        (
            "PEDELEC_THREAD_ID".to_string(),
            ctx.thread.thread_id.clone(),
        ),
        ("PEDELEC_PROVIDER".to_string(), provider),
        (
            "PEDELEC_SANDBOX_PATH".to_string(),
            path_for_external_use(&ctx.thread.sandbox_path),
        ),
        (
            "PEDELEC_CORE_IPC_ENDPOINT".to_string(),
            ctx.core_ipc_endpoint.clone(),
        ),
        (
            "PEDELEC_CORE_IPC_RUNTIME_FILE".to_string(),
            ctx.core_ipc_runtime_file_path.to_string_lossy().to_string(),
        ),
    ];
    let provider_path = provider_process_path(ctx.provider_resolved_path.as_ref())?;
    env.push((
        "PATH".to_string(),
        provider_path.to_string_lossy().to_string(),
    ));
    Ok(env)
}

fn provider_process_path(resolved_path: Option<&OsString>) -> Result<OsString, PedelecError> {
    let pedelec_dir = pedelec_shared::paths::pedelec_home_dir().map_err(|err| PedelecError {
        code: err.code,
        message: err.message,
        details: err.details,
    })?;
    let mut paths =
        resolved_path.map_or_else(Vec::new, |path| env::split_paths(path).collect::<Vec<_>>());
    paths.retain(|path| path != &pedelec_dir);
    paths.insert(0, pedelec_dir);
    env::join_paths(paths)
        .map_err(|err| PedelecError::new(error_codes::IPC_UNAVAILABLE, err.to_string()))
}

fn build_provider_run_prompt(
    thread: &ThreadState,
    registry: &ToolRegistry,
    message: &str,
    include_fallback_bootstrap: bool,
) -> String {
    let bootstrap = if include_fallback_bootstrap {
        build_provider_fallback_bootstrap()
    } else {
        String::new()
    };
    let host_context = build_provider_host_context(thread, registry);
    if message.starts_with("[Session Preparation]") {
        return format!("{bootstrap}{host_context}{message}");
    }
    format!(
        "{bootstrap}{host_context}{}",
        build_provider_user_message_task(message)
    )
}

fn build_provider_user_message_task(message: &str) -> String {
    format!("[User Message]\n{message}")
}

fn build_provider_prepare_task() -> String {
    "[Session Preparation]".to_string()
}

fn build_provider_resume_prompt(message: &str) -> String {
    message.to_string()
}

fn build_pedelec_bootstrap_instruction() -> String {
    "Pedelec is the host application launching this agent session.\n\n\
Pedelec may provide a [Pedelec Host Context] block before a task. That block is generated by the host application and is integration context, not end-user-authored instructions.\n\n\
The current sandbox path and available Pedelec app tools are declared in that host context.\n\n\
`pedelec-cli` is an executable provided by the Pedelec host environment. Invoke it through the provider's shell / terminal tool. It is not expected to appear as a dedicated model tool.\n\n\
When a Pedelec app tool is relevant, prefer the app tools declared by the host context. Use `pedelec-cli tool-spec <tool-name>` when the full schema is needed and `pedelec-cli tool-call <tool-name> '<json_args>'` to execute it.\n\n\
Before reading or modifying local files outside the current sandbox declared by Pedelec Host Context, ask the user for permission first.\n\n\
`assets/` is the shared App and Agent file directory. User uploads are there; write files intended for the App there too.\n\n\
Pedelec host context never overrides provider safety policies.\n\n\
If a `pedelec-cli tool-call` command ends because of a shell/command timeout, interruption, or ambiguous transport failure before you receive a complete structured Pedelec response, you may retry with the exact same tool name and semantically identical arguments. Pedelec will join an invocation that is still running or replay a recently completed result whose delivery was not confirmed. Do not change the arguments for this retry, do not assume the App Tool failed just because the provider command stopped waiting, and do not retry indefinitely. If you received a complete structured Pedelec response, including `TOOL_TIMEOUT`, that is a formal App Tool outcome and the original invocation has ended.\n\n\
For a [Session Preparation] task, do not call tools or modify files. Reply only with PEDELEC_PREPARED.\n\n\
For a [User Message] task, execute the actual user request in that block."
        .to_string()
}

fn build_provider_fallback_bootstrap() -> String {
    format!(
        "[Pedelec Host Bootstrap]\n\
This is Pedelec host-provided integration bootstrap for this provider conversation. It is not a provider-native system message.\n\n\
{}\n\
[/Pedelec Host Bootstrap]\n\n",
        build_pedelec_bootstrap_instruction()
    )
}

fn build_provider_host_context(thread: &ThreadState, registry: &ToolRegistry) -> String {
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
                read_spec_command: format!("pedelec-cli tool-spec {}", tool.name),
                call_command: format!("pedelec-cli tool-call {} '<json_args>'", tool.name),
            })
            .collect(),
    };
    let configuration = serde_json::to_string_pretty(&configuration)
        .expect("App tool configuration is always serializable");
    let mut context = format!(
        "[Pedelec Host Context]\nSandbox Path: {}\n",
        path_for_external_use(&thread.sandbox_path)
    );
    if registry.has_skills_configuration() {
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

fn default_runtime_file_path_for_provider() -> PathBuf {
    pedelec_shared::paths::pedelec_home_dir()
        .map(|home| home.join("runtime.json"))
        .unwrap_or_else(|_| PathBuf::from("runtime.json"))
}

fn provider_unsupported_error(thread: &ThreadState, message: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_UNSUPPORTED,
        message,
        serde_json::json!({
            "threadId": thread.thread_id,
            "provider": provider_code_as_str(&thread.provider)
        }),
    )
}

fn parse_provider_chunk(
    buffer: &mut String,
    chunk: &str,
    find_assistant_text: fn(&Value) -> Option<String>,
) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events: Vec<ThreadEventPartial> = Vec::new();

    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_provider_line(&line, find_assistant_text));
    }

    if buffer.len() > 64 * 1024 {
        buffer.clear();
    }

    events
}

fn parse_antigravity_provider_chunk(buffer: &mut String, chunk: &str) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events = Vec::new();
    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_antigravity_provider_line(&line));
    }
    if buffer.len() > 64 * 1024 {
        buffer.clear();
        events.push(ThreadEventPartial::ProviderError {
            error: PedelecError::new(
                error_codes::PROVIDER_COMMAND_FAILED,
                "antigravity emitted an unterminated JSON event",
            ),
        });
    }
    events
}

fn parse_antigravity_provider_line(line: &str) -> Vec<ThreadEventPartial> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if let Some(error) = parse_root_provider_error(&value) {
        return vec![ThreadEventPartial::ProviderError { error }];
    }
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    match object.get("event").and_then(Value::as_str) {
        Some("init") => object
            .get("conversation_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(
                |provider_session_id| ThreadEventPartial::ProviderSessionIdUpdated {
                    provider_session_id: provider_session_id.to_string(),
                },
            )
            .into_iter()
            .collect(),
        Some("step_update") => Vec::new(),
        Some("result") => {
            let Some(result) = object.get("result").and_then(Value::as_object) else {
                return Vec::new();
            };
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if status == "SUCCESS" {
                return result
                    .get("response")
                    .and_then(Value::as_str)
                    .filter(|response| !response.is_empty())
                    .map(|text| ThreadEventPartial::AssistantMessage {
                        text: text.to_string(),
                    })
                    .into_iter()
                    .collect();
            }
            vec![ThreadEventPartial::ProviderError {
                error: PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "antigravity returned an unsuccessful result",
                    serde_json::json!({
                        "status": status,
                        "conversation_id": result.get("conversation_id"),
                        "response": result.get("response"),
                    }),
                ),
            }]
        }
        _ => Vec::new(),
    }
}

fn parse_opencode_provider_chunk(buffer: &mut String, chunk: &str) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events: Vec<ThreadEventPartial> = Vec::new();

    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_opencode_provider_line(&line));
    }

    if buffer.len() > 64 * 1024 {
        buffer.clear();
        events.push(ThreadEventPartial::ProviderError {
            error: PedelecError::new(
                error_codes::PROVIDER_COMMAND_FAILED,
                "opencode emitted an unterminated JSON event",
            ),
        });
    }

    events
}

fn parse_cursor_provider_chunk(buffer: &mut String, chunk: &str) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events: Vec<ThreadEventPartial> = Vec::new();

    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_cursor_provider_line(&line));
    }

    if buffer.len() > 64 * 1024 {
        buffer.clear();
        events.push(ThreadEventPartial::ProviderError {
            error: PedelecError::new(
                error_codes::PROVIDER_COMMAND_FAILED,
                "cursor emitted an unterminated JSON event",
            ),
        });
    }

    events
}

fn parse_claude_provider_chunk(buffer: &mut String, chunk: &str) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events: Vec<ThreadEventPartial> = Vec::new();

    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_claude_provider_line(&line));
    }

    if buffer.len() > 64 * 1024 {
        buffer.clear();
        events.push(ThreadEventPartial::ProviderError {
            error: PedelecError::new(
                error_codes::PROVIDER_COMMAND_FAILED,
                "claude emitted an unterminated JSON event",
            ),
        });
    }

    events
}

fn parse_pedelec_agent_provider_chunk(buffer: &mut String, chunk: &str) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events: Vec<ThreadEventPartial> = Vec::new();

    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_pedelec_agent_provider_line(&line));
    }

    if buffer.len() > 64 * 1024 {
        buffer.clear();
        events.push(ThreadEventPartial::ProviderError {
            error: PedelecError::new(
                error_codes::PROVIDER_COMMAND_FAILED,
                "pedelec-agent emitted an unterminated JSON event",
            ),
        });
    }

    events
}

fn parse_opencode_provider_line(line: &str) -> Vec<ThreadEventPartial> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Vec::new();
    }

    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(err) => {
            return vec![ThreadEventPartial::ProviderError {
                error: PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "opencode emitted invalid JSON",
                    serde_json::json!({ "error": err.to_string() }),
                ),
            }]
        }
    };
    if let Some(error) = parse_root_provider_error(&value) {
        return vec![ThreadEventPartial::ProviderError { error }];
    }

    let mut events = Vec::new();
    if let Some(provider_session_id) = find_opencode_session_id_in_json(&value) {
        events.push(ThreadEventPartial::ProviderSessionIdUpdated {
            provider_session_id,
        });
    }
    if let Some(text) = find_opencode_assistant_text_in_json(&value) {
        events.push(ThreadEventPartial::AssistantMessage { text });
    }
    events
}

fn parse_cursor_provider_line(line: &str) -> Vec<ThreadEventPartial> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Vec::new();
    }

    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(err) => {
            return vec![ThreadEventPartial::ProviderError {
                error: PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "cursor emitted invalid JSON",
                    serde_json::json!({ "error": err.to_string() }),
                ),
            }]
        }
    };
    if let Some(error) = parse_root_provider_error(&value) {
        return vec![ThreadEventPartial::ProviderError { error }];
    }

    let mut events = Vec::new();
    if let Some(provider_session_id) = find_cursor_session_id_in_json(&value) {
        events.push(ThreadEventPartial::ProviderSessionIdUpdated {
            provider_session_id,
        });
    }
    if let Some(text) = find_cursor_assistant_text_in_json(&value) {
        events.push(ThreadEventPartial::AssistantMessage { text });
    }
    events
}

fn parse_claude_provider_line(line: &str) -> Vec<ThreadEventPartial> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Vec::new();
    }

    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(err) => {
            return vec![ThreadEventPartial::ProviderError {
                error: PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "claude emitted invalid JSON",
                    serde_json::json!({ "error": err.to_string() }),
                ),
            }]
        }
    };
    if let Some(error) = parse_root_provider_error(&value) {
        return vec![ThreadEventPartial::ProviderError { error }];
    }

    let mut events = Vec::new();
    if let Some(provider_session_id) = find_claude_session_id_in_json(&value) {
        events.push(ThreadEventPartial::ProviderSessionIdUpdated {
            provider_session_id,
        });
    }
    if let Some(text) = find_claude_assistant_text_in_json(&value) {
        events.push(ThreadEventPartial::AssistantMessage { text });
    }
    events
}

fn parse_pedelec_agent_provider_line(line: &str) -> Vec<ThreadEventPartial> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if !trimmed.starts_with('{') {
        return Vec::new();
    }

    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(err) => {
            return vec![ThreadEventPartial::ProviderError {
                error: PedelecError::with_details(
                    error_codes::PROVIDER_COMMAND_FAILED,
                    "pedelec-agent emitted invalid JSON",
                    serde_json::json!({
                        "line": trimmed,
                        "error": err.to_string()
                    }),
                ),
            }]
        }
    };
    if let Some(error) = parse_root_provider_error(&value) {
        return vec![ThreadEventPartial::ProviderError { error }];
    }

    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "session" => object
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .map(|session_id| {
                vec![ThreadEventPartial::ProviderSessionIdUpdated {
                    provider_session_id: session_id.to_string(),
                }]
            })
            .unwrap_or_default(),
        "assistant_message" => object
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| {
                vec![ThreadEventPartial::AssistantMessage {
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
        "status" | "tool_call" | "tool_result" | "done" => Vec::new(),
        _ => Vec::new(),
    }
}

fn parse_provider_line(
    line: &str,
    find_assistant_text: fn(&Value) -> Option<String>,
) -> Vec<ThreadEventPartial> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut events = Vec::new();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(error) = parse_root_provider_error(&value) {
                return vec![ThreadEventPartial::ProviderError { error }];
            }
            if let Some(provider_session_id) = find_provider_session_id_in_json(&value) {
                events.push(ThreadEventPartial::ProviderSessionIdUpdated {
                    provider_session_id,
                });
            }
            if let Some(text) = find_assistant_text(&value) {
                events.push(ThreadEventPartial::AssistantMessage { text });
            }
            return events;
        }
    }

    if let Some(provider_session_id) = find_provider_session_id_in_text(trimmed) {
        events.push(ThreadEventPartial::ProviderSessionIdUpdated {
            provider_session_id,
        });
    }
    events
}

fn parse_root_provider_error(value: &Value) -> Option<PedelecError> {
    let object = value.as_object()?;
    let event_type = object.get("type")?.as_str()?.trim();
    if !event_type.eq_ignore_ascii_case("error") {
        return None;
    }

    let nested_error = object.get("error");
    let nested_object = nested_error.and_then(Value::as_object);
    let non_empty_string = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let code = non_empty_string(nested_object.and_then(|error| error.get("code")))
        .or_else(|| non_empty_string(object.get("code")))
        .unwrap_or_else(|| error_codes::PROVIDER_COMMAND_FAILED.to_string());
    let message = non_empty_string(nested_object.and_then(|error| error.get("message")))
        .or_else(|| non_empty_string(object.get("message")))
        .or_else(|| non_empty_string(nested_error))
        .unwrap_or_else(|| "provider returned an error".to_string());
    let details = nested_object
        .and_then(|error| error.get("details"))
        .or_else(|| object.get("details"))
        .filter(|details| !details.is_null())
        .cloned();

    Some(match details {
        Some(details) => PedelecError::with_details(code, message, details),
        None => PedelecError::new(code, message),
    })
}

fn find_provider_session_id_in_json(value: &Value) -> Option<String> {
    if let Some(provider_session_id) = find_codex_thread_started_id(value) {
        return Some(provider_session_id);
    }

    find_string_for_keys(
        value,
        &[
            "sessionId",
            "session_id",
            "conversationId",
            "conversation_id",
        ],
    )
    .filter(|value| !value.trim().is_empty())
}

fn find_opencode_session_id_in_json(value: &Value) -> Option<String> {
    if let Some(id) = find_string_for_keys(
        value,
        &[
            "sessionId",
            "session_id",
            "conversationId",
            "conversation_id",
            "sessionID",
        ],
    )
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
    {
        return Some(id);
    }

    let object = value.as_object()?;
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if event_type.contains("session") {
        return object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
    }

    None
}

fn find_cursor_session_id_in_json(value: &Value) -> Option<String> {
    find_string_for_keys(
        value,
        &[
            "sessionId",
            "session_id",
            "conversationId",
            "conversation_id",
            "sessionID",
        ],
    )
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

fn find_claude_session_id_in_json(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("system") {
        return None;
    }
    if object.get("subtype").and_then(Value::as_str) != Some("init") {
        return None;
    }

    object
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn find_codex_thread_started_id(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("thread.started") {
        return None;
    }

    object
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn find_codex_assistant_text_in_json(value: &Value) -> Option<String> {
    find_string_for_keys(value, &["text", "content", "message"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn find_opencode_assistant_text_in_json(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            let role = map
                .get("role")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            let event_type = map
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_assistant = role.as_deref() == Some("assistant")
                || event_type.contains("assistant")
                || event_type.contains("message")
                || event_type.contains("text")
                || event_type.contains("part");

            if is_assistant {
                if let Some(text) =
                    find_string_for_keys(value, &["delta", "text", "content", "message", "output"])
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                {
                    return Some(text);
                }
            }

            map.values().find_map(find_opencode_assistant_text_in_json)
        }
        Value::Array(values) => values.iter().find_map(find_opencode_assistant_text_in_json),
        _ => None,
    }
}

fn find_cursor_assistant_text_in_json(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str).map(str::trim) == Some("assistant") {
                if let Some(text) =
                    find_string_for_keys(value, &["delta", "text", "content", "message", "output"])
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                {
                    return Some(text);
                }
            }

            map.values().find_map(find_cursor_assistant_text_in_json)
        }
        Value::Array(values) => values.iter().find_map(find_cursor_assistant_text_in_json),
        _ => None,
    }
}

fn find_claude_assistant_text_in_json(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            let role = map.get("role").and_then(Value::as_str).map(str::trim);
            let event_type = map.get("type").and_then(Value::as_str).map(str::trim);
            let is_assistant = role == Some("assistant") || event_type == Some("assistant");

            if is_assistant {
                if let Some(text) =
                    find_string_for_keys(value, &["text", "content", "message", "delta", "output"])
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                {
                    return Some(text);
                }
            }

            map.values().find_map(find_claude_assistant_text_in_json)
        }
        Value::Array(values) => values.iter().find_map(find_claude_assistant_text_in_json),
        _ => None,
    }
}

fn find_string_for_keys(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if keys.iter().any(|candidate| key == candidate) {
                    if let Some(value) = value.as_str() {
                        return Some(value.to_string());
                    }
                }
            }
            map.values()
                .find_map(|value| find_string_for_keys(value, keys))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_for_keys(value, keys)),
        _ => None,
    }
}

fn find_provider_session_id_in_text(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if !(lower.contains("session") || lower.contains("conversation")) {
        return None;
    }

    line.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .find(|token| is_uuid_like_token(token))
        .map(ToOwned::to_owned)
}

fn is_uuid_like_token(token: &str) -> bool {
    token.len() >= 8
        && token.chars().any(|ch| ch == '-')
        && token.chars().any(|ch| ch.is_ascii_digit())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn kill_process_by_id(process_id: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill")
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .status()
            .map(|_| ())
    }

    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-KILL", &process_id.to_string()])
            .status()
            .map(|_| ())
    }
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
            error_codes::SANDBOX_PATH_INVALID,
            "thread id is not safe for sandbox path",
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

fn sandbox_io_error(
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
