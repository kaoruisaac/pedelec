use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use url::Url;
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_SKILL_SIZE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_TIMEOUT_MS: u64 = 120_000;
const OLLAMA_CONNECTION_CHECK_TIMEOUT_MS: u64 = 3_000;
const SANDBOX_SUBDIRS: [&str; 5] = ["skills", "input", "output", "logs", "tmp"];
const TOOL_TIMEOUT_OVERRIDE_FIELD: &str = "timeoutMs";
const THREAD_ID_BASE36_MIN_WIDTH: usize = 6;
const THREAD_ID_BASE36_MAX_WIDTH: usize = 7;
const THREAD_ID_MAX_COUNTER: u64 = 78_364_164_095;
pub const MAX_ASSET_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
const ASSET_UPLOAD_TICKET_SECONDS: i64 = 5 * 60;
const MAX_PROVIDER_STDERR_BYTES: usize = 64 * 1024;

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
pub(crate) struct AssetUploadTicket {
    pub thread_id: String,
    pub sandbox_path: PathBuf,
    pub filename: String,
    pub safe_filename: String,
    pub expected_size_bytes: u64,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub state: AssetUploadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetUploadState {
    Pending,
    Uploading,
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
    pub model: Option<String>,
    pub sandbox_path: PathBuf,
    pub skills: Vec<SkillFile>,
    pub status: ThreadStatus,
    pub process_id: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
    pub default_models: HashMap<ProviderCode, String>,
    pub provider_settings: ProviderSettings,
}

impl Default for PedelecSettings {
    fn default() -> Self {
        Self {
            default_provider: None,
            default_models: HashMap::new(),
            provider_settings: ProviderSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsInput {
    pub default_provider: ProviderCode,
    pub default_models: HashMap<ProviderCode, String>,
    pub provider_settings: ProviderSettingsInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettings {
    pub ollama: OllamaProviderSettings,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            ollama: OllamaProviderSettings::default(),
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
}

impl Default for OllamaProviderSettings {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            timeout_ms: DEFAULT_OLLAMA_TIMEOUT_MS,
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettingsInput {
    pub ollama: OllamaProviderSettingsInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OllamaProviderSettingsInput {
    pub base_url: Option<String>,
    pub timeout_ms: Option<u64>,
    pub api_key: Option<String>,
}

impl Default for ProviderSettingsInput {
    fn default() -> Self {
        Self {
            ollama: OllamaProviderSettingsInput {
                base_url: Some(DEFAULT_OLLAMA_BASE_URL.to_string()),
                timeout_ms: Some(DEFAULT_OLLAMA_TIMEOUT_MS),
                api_key: Some("ollama".to_string()),
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
    pub model: Option<String>,
    pub skills: Option<CreateThreadSkillsInput>,
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
    pub settings: PedelecSettings,
    pub core_ipc_endpoint: String,
    pub core_ipc_runtime_file_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct RunningProviderProcess {
    process_id: u32,
    child: Arc<Mutex<Option<Child>>>,
    purpose: RunningProviderProcessPurpose,
    stderr: String,
    stderr_truncated: bool,
    had_provider_error: bool,
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "-m");
        args.push("-".to_string());
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
            "resume".to_string(),
            provider_session_id.to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "-m");
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
    received_agent_delta: bool,
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
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
        let mut args = vec![
            "-p".to_string(),
            prompt.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--mode".to_string(),
            "accept-edits".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "--model");
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
        add_model_args(&mut args, &ctx.thread.model, "--model");
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
        parse_antigravity_provider_chunk(
            &mut self.stdout_buffer,
            chunk,
            &mut self.received_agent_delta,
        )
    }

    fn parse_stderr_event(&mut self, chunk: &str) -> Vec<ThreadEventPartial> {
        parse_antigravity_provider_chunk(
            &mut self.stderr_buffer,
            chunk,
            &mut self.received_agent_delta,
        )
    }
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "--model");
        args.push("-".to_string());
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--session".to_string(),
            provider_session_id.to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "--model");
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--force".to_string(),
            "--trust".to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "--model");
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--resume".to_string(),
            provider_session_id.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--force".to_string(),
            "--trust".to_string(),
        ];
        add_model_args(&mut args, &ctx.thread.model, "--model");
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

#[derive(Debug, Clone, Default)]
struct ClaudeProviderAdapter {
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
        add_model_args(&mut args, &ctx.thread.model, "--model");
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
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
        add_model_args(&mut args, &ctx.thread.model, "--model");
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
        let model = required_ollama_model(&ctx.thread)?;
        let args = vec![
            "--provider".to_string(),
            "ollama".to_string(),
            "--model".to_string(),
            model,
            "--sandbox".to_string(),
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
        ];
        let prompt = build_provider_run_prompt(&ctx.thread, &ctx.tool_registry, message);
        let mut env = build_provider_env(ctx)?;
        env.push((
            "OLLAMA_API_KEY".to_string(),
            require_ollama_api_key(Some(ctx.settings.provider_settings.ollama.api_key.clone()))?,
        ));
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

        let model = required_ollama_model(&ctx.thread)?;
        let args = vec![
            "--provider".to_string(),
            "ollama".to_string(),
            "--model".to_string(),
            model,
            "--sandbox".to_string(),
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
            "--session-id".to_string(),
            provider_session_id.to_string(),
        ];
        let prompt = build_provider_resume_prompt(message);
        let mut env = build_provider_env(ctx)?;
        env.push((
            "OLLAMA_API_KEY".to_string(),
            require_ollama_api_key(Some(ctx.settings.provider_settings.ollama.api_key.clone()))?,
        ));
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

#[derive(Debug, Default)]
pub struct CoreRuntime {
    pub thread_manager: ThreadManager,
    pub sandbox_manager: SandboxManager,
    pub skill_manager: SkillManager,
    pub tool_registry: ToolRegistryStore,
    pub tool_request_broker: ToolRequestBroker,
    pub event_bus: EventBus,
    pub(crate) running_processes: HashMap<String, RunningProviderProcess>,
    pub(crate) core_ipc_endpoint: Option<String>,
    pub(crate) core_ipc_runtime_file_path: Option<PathBuf>,
    pub(crate) settings_file_path: Option<PathBuf>,
    pub(crate) provider_scan: HashMap<ProviderCode, ProviderCli>,
    pub(crate) provider_refresh_in_progress: bool,
    pub(crate) asset_upload_port: Option<u16>,
    pub(crate) asset_upload_tickets: HashMap<String, AssetUploadTicket>,
    #[cfg(test)]
    pub(crate) provider_path_value_override: Option<OsString>,
    #[cfg(test)]
    pub(crate) test_provider_command: Option<CommandSpec>,
}

impl CoreRuntime {
    pub(crate) fn new() -> Self {
        Self::default()
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
        if thread.status == ThreadStatus::Ended {
            return Err(PedelecError::new(
                error_codes::THREAD_ENDED,
                "thread has ended",
            ));
        }
        if thread.status != ThreadStatus::Idle {
            return Err(PedelecError::new(
                error_codes::THREAD_BUSY,
                "thread is busy",
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
        self.asset_upload_tickets.insert(
            upload_id.clone(),
            AssetUploadTicket {
                thread_id: input.thread_id,
                sandbox_path,
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
        let input_path = thread.sandbox_path.join("input");
        if !input_path.exists() {
            return Ok(ListAssetsOutput { assets: Vec::new() });
        }
        let entries = fs::read_dir(&input_path).map_err(|err| {
            PedelecError::with_details(
                error_codes::ASSET_LIST_FAILED,
                "failed to read sandbox assets",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
        let mut assets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "failed to read sandbox asset",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            let file_type = entry.file_type().map_err(|err| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "failed to inspect sandbox asset",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().into_string().map_err(|_| {
                PedelecError::new(
                    error_codes::ASSET_LIST_FAILED,
                    "sandbox asset filename cannot be encoded",
                )
            })?;
            if name.starts_with(".pedelec-") {
                continue;
            }
            let metadata = entry.metadata().map_err(|err| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "failed to read sandbox asset metadata",
                    serde_json::json!({ "name": name, "error": err.to_string() }),
                )
            })?;
            let modified_at = metadata
                .modified()
                .map_err(|err| {
                    PedelecError::with_details(
                        error_codes::ASSET_LIST_FAILED,
                        "failed to read sandbox asset modified time",
                        serde_json::json!({ "name": name, "error": err.to_string() }),
                    )
                })?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| {
                    PedelecError::with_details(
                        error_codes::ASSET_LIST_FAILED,
                        "sandbox asset modified time predates Unix epoch",
                        serde_json::json!({ "name": name }),
                    )
                })?
                .as_millis();
            let modified_at = i64::try_from(modified_at).map_err(|_| {
                PedelecError::with_details(
                    error_codes::ASSET_LIST_FAILED,
                    "sandbox asset modified time is out of range",
                    serde_json::json!({ "name": name }),
                )
            })?;
            assets.push(SandboxAsset {
                path: format!("input/{name}"),
                name,
                size_bytes: metadata.len(),
                modified_at,
            });
        }
        assets.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(ListAssetsOutput { assets })
    }

    pub(crate) fn expire_asset_uploads(&mut self) {
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

    pub fn create_thread(
        &mut self,
        input: CreateThreadInput,
    ) -> Result<CreateThreadOutput, PedelecError> {
        let thread_id = self.next_available_thread_id()?;
        let (sandbox_path, (skills, registry)) =
            self.sandbox_manager
                .create_thread_sandbox_with(&thread_id, |sandbox| {
                    let skills_dir = sandbox.join("skills");
                    fs::create_dir_all(&skills_dir).map_err(|err| {
                        skill_download_error(
                            "cannot create skills directory",
                            None,
                            Some(&skills_dir),
                            err,
                        )
                    })?;
                    let registry = ToolRegistry::from_skills_input(input.skills.as_ref())?;
                    let skills = write_generated_tool_specs(&skills_dir, &registry)?;
                    Ok((skills, registry))
                })?;

        let now = Utc::now();
        let state = ThreadState {
            thread_id: thread_id.clone(),
            provider: input.provider,
            model: input.model,
            sandbox_path: sandbox_path.clone(),
            skills,
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
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
            .register_thread_log(&thread_id, sandbox_path.join("logs").join("events.jsonl"));
        self.event_bus.emit_created(&thread_id);
        self.event_bus
            .emit_status_changed(&thread_id, ThreadStatus::Idle);

        Ok(CreateThreadOutput { thread_id })
    }

    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        list_provider_infos_with_scan(&self.provider_scan, self.provider_path_value())
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
        self.provider_scan = scan_external_providers(self.provider_path_value());
    }

    pub fn get_settings(&self) -> Result<PedelecSettings, PedelecError> {
        read_settings_file(&self.resolved_settings_file_path()?)
    }

    pub fn update_settings(
        &mut self,
        input: UpdateSettingsInput,
    ) -> Result<PedelecSettings, PedelecError> {
        #[cfg(test)]
        let settings = if self.provider_scan.is_empty() {
            normalize_update_settings_for_test(input, self.provider_path_value().as_ref())?
        } else {
            normalize_update_settings(input, &self.provider_scan)?
        };
        #[cfg(not(test))]
        let settings = normalize_update_settings(input, &self.provider_scan)?;
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
        #[cfg(test)]
        if let Some(path) = &self.provider_path_value_override {
            return Some(path.clone());
        }

        Some(merged_provider_path(env::var_os("PATH")))
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
                "an asset upload is in progress",
            ));
        }
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

        #[cfg(test)]
        let test_command = self.test_provider_command.clone();

        #[cfg(test)]
        let command = if let Some(command) = test_command {
            command
        } else {
            self.build_send_text_command(&input)?
        };

        #[cfg(not(test))]
        let command = self.build_send_text_command(&input)?;

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
                "an asset upload is in progress",
            ));
        }
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

        #[cfg(test)]
        let test_command = self.test_provider_command.clone();

        #[cfg(test)]
        let command = if let Some(command) = test_command {
            command
        } else {
            self.build_prepare_thread_command(&input)?
        };

        #[cfg(not(test))]
        let command = self.build_prepare_thread_command(&input)?;

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
        let ctx = RunPromptProviderContext {
            thread,
            tool_registry,
            provider_state: provider_state.clone(),
            settings,
            core_ipc_endpoint: self.core_ipc_endpoint.clone().unwrap_or_default(),
            core_ipc_runtime_file_path: self
                .core_ipc_runtime_file_path
                .clone()
                .unwrap_or_else(default_runtime_file_path_for_provider),
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
        self.apply_scanned_provider_program(&ctx.thread.provider, &mut command)?;
        Ok(command)
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
    ) {
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
                purpose,
                stderr: String::new(),
                stderr_truncated: false,
                had_provider_error: false,
            },
        );
    }

    pub fn fail_provider_process_start(
        &mut self,
        thread_id: &str,
        error: PedelecError,
        purpose: RunningProviderProcessPurpose,
    ) {
        self.running_processes.remove(thread_id);
        self.tool_request_broker.clear_thread(thread_id);
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
        self.event_bus.emit_raw_stdout(thread_id, text.clone());
        let events = self
            .thread_manager
            .provider_adapter_mut(thread_id)
            .map(|adapter| adapter.parse_stdout_event(&text))
            .unwrap_or_default();
        self.emit_provider_partials(thread_id, events);
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
        let purpose = running.purpose;
        let had_provider_error = running.had_provider_error;

        let prepare_missing_provider_session_id = purpose == RunningProviderProcessPurpose::Prepare
            && !had_provider_error
            && self
                .thread_manager
                .provider_state(thread_id)
                .and_then(|state| state.provider_session_id.as_deref())
                .is_none();

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
            if prepare_missing_provider_session_id {
                self.tool_request_broker.clear_thread(thread_id);
                self.emit_thread_provider_error(
                    thread_id,
                    PedelecError::with_details(
                        error_codes::PREPARE_SESSION_ID_MISSING,
                        "provider session id was not found after prepare",
                        serde_json::json!({ "threadId": thread_id }),
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
                    self.event_bus.emit_status_changed(thread_id, ThreadStatus::Idle);
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
            let error = PedelecError::with_details(error_codes::PROVIDER_COMMAND_FAILED, message, details);
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

    fn stop_running_process(&mut self, thread_id: &str) {
        let Some(running) = self.running_processes.remove(thread_id) else {
            return;
        };

        if let Ok(mut child) = running.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }

        let _ = kill_process_by_id(running.process_id);
        std::thread::sleep(Duration::from_millis(100));
    }

    pub fn end_thread(&mut self, input: EndThreadInput) -> Result<(), PedelecError> {
        self.invalidate_asset_uploads_for_thread(&input.thread_id);
        let sandbox_path = {
            let thread = self.thread_manager.thread_mut(&input.thread_id)?;
            if thread.status != ThreadStatus::Ended {
                thread.status = ThreadStatus::Stopping;
                thread.updated_at = Utc::now();
                self.event_bus
                    .emit_status_changed(&input.thread_id, ThreadStatus::Stopping);
            }
            thread.sandbox_path.clone()
        };

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
        let _ = self.remove_thread_sandbox_with_retry(&sandbox_path);
        self.event_bus.unregister_thread_log(&input.thread_id);
        Ok(())
    }

    pub fn cleanup_for_app_exit(&mut self) -> Vec<PedelecError> {
        let thread_ids = self.thread_manager.thread_ids();
        for thread_id in thread_ids {
            let _ = self.end_thread(EndThreadInput { thread_id });
        }

        self.sandbox_manager.remove_all_thread_sandboxes()
    }

    fn remove_thread_sandbox_with_retry(&self, sandbox_path: &Path) -> Result<(), PedelecError> {
        let mut last_error = None;
        for attempt in 0..10 {
            match self.sandbox_manager.remove_thread_sandbox(sandbox_path) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_error = Some(err);
                    if attempt < 9 {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PedelecError::with_details(
                error_codes::SANDBOX_REMOVE_FAILED,
                "cannot remove thread sandbox",
                serde_json::json!({ "path": sandbox_path.to_string_lossy() }),
            )
        }))
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
    ) -> Result<(String, u64, mpsc::Receiver<Value>), PedelecError> {
        let thread = self.thread_manager.thread_mut(&input.thread_id)?;
        match thread.status {
            ThreadStatus::Running => {}
            ThreadStatus::WaitingToolResult => {
                return Err(PedelecError::with_details(
                    error_codes::PENDING_TOOL_REQUEST_EXISTS,
                    "thread already has a pending tool request",
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
            _ => {
                return Err(PedelecError::with_details(
                    error_codes::THREAD_BUSY,
                    "thread is not running",
                    serde_json::json!({ "threadId": input.thread_id }),
                ));
            }
        }

        if self
            .tool_request_broker
            .has_pending_for_thread(&input.thread_id)
        {
            return Err(PedelecError::with_details(
                error_codes::PENDING_TOOL_REQUEST_EXISTS,
                "thread already has a pending tool request",
                serde_json::json!({ "threadId": input.thread_id }),
            ));
        }

        let registry = self.tool_registry.get(&input.thread_id).ok_or_else(|| {
            PedelecError::with_details(
                error_codes::TOOLS_MANIFEST_INVALID,
                "tool registry was not found for thread",
                serde_json::json!({ "threadId": input.thread_id }),
            )
        })?;
        let normalized = registry.normalize_tool_call(&input.tool_name, &input.args)?;
        let (request_id, receiver) = self.tool_request_broker.create_pending(
            input.thread_id.clone(),
            input.tool_name.clone(),
            normalized.args.clone(),
            normalized.timeout_ms,
        )?;

        thread.status = ThreadStatus::WaitingToolResult;
        thread.updated_at = Utc::now();
        self.event_bus
            .emit_status_changed(&input.thread_id, ThreadStatus::WaitingToolResult);
        self.event_bus.emit_tool_call(
            &input.thread_id,
            &request_id,
            &input.tool_name,
            normalized.args,
        );

        Ok((request_id, normalized.timeout_ms, receiver))
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
        let Some(pending) = self.tool_request_broker.remove(request_id) else {
            return;
        };
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

        let pending = self
            .tool_request_broker
            .remove(&input.request_id)
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

        let _ = pending.result_tx.send(input.result.clone());
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

/// Scans provider CLIs without holding the shared runtime lock. The completed
/// scan is installed atomically, so readers see either the prior complete scan
/// or the new complete scan, never partial results.
pub fn refresh_shared_providers(runtime: &SharedCoreRuntime) -> Vec<ProviderInfo> {
    let path_value = {
        let mut runtime = runtime.lock().unwrap();
        if runtime.provider_refresh_in_progress {
            return runtime.list_providers();
        }
        runtime.provider_refresh_in_progress = true;
        runtime.provider_path_value()
    };
    let provider_scan = scan_external_providers(path_value);
    let mut runtime = runtime.lock().unwrap();
    runtime.provider_scan = provider_scan;
    runtime.provider_refresh_in_progress = false;
    runtime.list_providers()
}

#[derive(Debug)]
pub struct CoreRuntimeOwner {
    runtime: SharedCoreRuntime,
}

impl CoreRuntimeOwner {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn insert_thread(
        &mut self,
        state: ThreadState,
        provider_state: ProviderAdapterState,
    ) {
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
                serde_json::json!({ "sandboxPath": sandbox_path.to_string_lossy() }),
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

            Ok(sandbox_path.clone())
        })();

        if create_result.is_err() {
            let _ = fs::remove_dir_all(&sandbox_path);
        }

        create_result
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

            if let Err(err) = self.remove_thread_sandbox(&path) {
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
                serde_json::json!({ "sandboxPath": path.to_string_lossy() }),
            ));
        }

        Ok(())
    }
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
pub struct PendingToolWait {
    pub request: PendingToolRequest,
    result_tx: mpsc::Sender<Value>,
}

#[derive(Debug, Default)]
pub struct ToolRequestBroker {
    pending: HashMap<String, PendingToolWait>,
    next_request_number: u64,
}

impl ToolRequestBroker {
    pub fn create_pending(
        &mut self,
        thread_id: String,
        tool_name: String,
        args: Value,
        timeout_ms: u64,
    ) -> Result<(String, mpsc::Receiver<Value>), PedelecError> {
        if self.has_pending_for_thread(&thread_id) {
            return Err(PedelecError::with_details(
                error_codes::PENDING_TOOL_REQUEST_EXISTS,
                "thread already has a pending tool request",
                serde_json::json!({ "threadId": thread_id }),
            ));
        }

        self.next_request_number += 1;
        let request_id = format!(
            "toolreq_{}_{}",
            Utc::now().timestamp_millis(),
            self.next_request_number
        );
        let (result_tx, result_rx) = mpsc::channel();
        let request = PendingToolRequest {
            request_id: request_id.clone(),
            thread_id,
            tool_name,
            args,
            created_at: Utc::now(),
            timeout_ms,
        };

        self.pending
            .insert(request_id.clone(), PendingToolWait { request, result_tx });

        Ok((request_id, result_rx))
    }

    pub fn has_pending_for_thread(&self, thread_id: &str) -> bool {
        self.pending
            .values()
            .any(|pending| pending.request.thread_id == thread_id)
    }

    pub fn get(&self, request_id: &str) -> Option<&PendingToolWait> {
        self.pending.get(request_id)
    }

    pub fn remove(&mut self, request_id: &str) -> Option<PendingToolWait> {
        self.pending.remove(request_id)
    }

    pub fn clear_thread(&mut self, thread_id: &str) {
        self.pending
            .retain(|_, pending| pending.request.thread_id != thread_id);
    }
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
                cwd: command.cwd.to_string_lossy().to_string(),
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
        self.emit_error(
            thread_id,
            ThreadErrorSource::Provider { provider },
            error,
        );
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

pub mod error_codes {
    pub const CORE_RUNTIME_UNAVAILABLE: &str = "CORE_RUNTIME_UNAVAILABLE";
    pub const THREAD_NOT_FOUND: &str = "THREAD_NOT_FOUND";
    pub const THREAD_BUSY: &str = "THREAD_BUSY";
    pub const THREAD_ENDED: &str = "THREAD_ENDED";
    pub const PROVIDER_NOT_FOUND: &str = "PROVIDER_NOT_FOUND";
    pub const PROVIDER_UNSUPPORTED: &str = "PROVIDER_UNSUPPORTED";
    pub const PROVIDER_PROCESS_START_FAILED: &str = "PROVIDER_PROCESS_START_FAILED";
    pub const PROVIDER_PROCESS_STOP_FAILED: &str = "PROVIDER_PROCESS_STOP_FAILED";
    pub const PROVIDER_STDIN_CLOSED: &str = "PROVIDER_STDIN_CLOSED";
    pub const PROVIDER_COMMAND_FAILED: &str = "PROVIDER_COMMAND_FAILED";
    pub const PROVIDER_INSTALL_UNSUPPORTED: &str = "PROVIDER_INSTALL_UNSUPPORTED";
    pub const PROVIDER_INSTALLER_LAUNCH_FAILED: &str = "PROVIDER_INSTALLER_LAUNCH_FAILED";
    pub const PROVIDER_TERMINAL_UNSUPPORTED: &str = "PROVIDER_TERMINAL_UNSUPPORTED";
    pub const PROVIDER_TERMINAL_UNAVAILABLE: &str = "PROVIDER_TERMINAL_UNAVAILABLE";
    pub const PROVIDER_TERMINAL_WORKDIR_FAILED: &str = "PROVIDER_TERMINAL_WORKDIR_FAILED";
    pub const PROVIDER_TERMINAL_LAUNCH_FAILED: &str = "PROVIDER_TERMINAL_LAUNCH_FAILED";
    pub const PROVIDER_PREPARE_UNSUPPORTED: &str = "PROVIDER_PREPARE_UNSUPPORTED";
    pub const PREPARE_SESSION_ID_MISSING: &str = "PREPARE_SESSION_ID_MISSING";
    pub const SKILL_URL_INVALID: &str = "SKILL_URL_INVALID";
    pub const SKILL_DOWNLOAD_FAILED: &str = "SKILL_DOWNLOAD_FAILED";
    pub const SANDBOX_CREATE_FAILED: &str = "SANDBOX_CREATE_FAILED";
    pub const SANDBOX_REMOVE_FAILED: &str = "SANDBOX_REMOVE_FAILED";
    pub const SANDBOX_PATH_INVALID: &str = "SANDBOX_PATH_INVALID";
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

/// GUI applications do not reliably inherit shell profile PATH updates. Merge the
/// installer locations here instead of changing the user's global environment.
fn merged_provider_path(current: Option<OsString>) -> OsString {
    let mut paths = current.as_ref().map_or_else(Vec::new, |value| env::split_paths(value).collect());
    #[cfg(windows)]
    if let Ok(hkcu) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Environment")
    {
        if let Ok(value) = hkcu.get_value::<OsString, _>("Path") { paths.extend(env::split_paths(&value)); }
    }
    if let Some(home) = dirs::home_dir() {
        #[cfg(windows)]
        paths.extend([home.join("AppData/Local/Programs/OpenAI/Codex/bin"), home.join("AppData/Local/agy/bin"), home.join(".opencode/bin"), home.join("AppData/Roaming/npm")]);
        #[cfg(not(windows))]
        paths.extend([home.join(".local/bin"), home.join(".opencode/bin")]);
    }
    let mut normalized = Vec::new();
    for path in paths {
        if !path.exists() { continue; }
        let duplicate = normalized.iter().any(|existing: &PathBuf| {
            #[cfg(windows)] { existing.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy()) }
            #[cfg(not(windows))] { existing == &path }
        });
        if !duplicate { normalized.push(path); }
    }
    env::join_paths(normalized).unwrap_or_default()
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
    let recognized = candidates
        .into_iter()
        .filter(|path| is_provider_executable(path))
        .filter_map(|path| provider_cli_version(&path).map(|version| (path, version)))
        .max_by(|(left_path, left), (right_path, right)| {
            left.cmp(right).then_with(|| left_path.cmp(right_path))
        });
    match recognized {
        Some((path, version)) => ProviderCli {
            path: Some(path),
            version: Some(version),
            error: None,
        },
        None => ProviderCli {
            path: None,
            version: None,
            error: Some(
                "no provider CLI with a recognizable version was found in PATH".to_string(),
            ),
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

fn provider_cli_version(path: &Path) -> Option<ProviderVersion> {
    let output = provider_version_command(path).arg("--version").output();
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

fn provider_version_command(path: &Path) -> Command {
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
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }

    #[cfg(not(windows))]
    {
        Command::new(path)
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

    let default_models = input
        .default_models
        .into_iter()
        .filter_map(|(provider, model)| {
            let model = model.trim().to_string();
            if model.is_empty() {
                None
            } else {
                Some((provider, model))
            }
        })
        .collect();

    Ok(PedelecSettings {
        default_provider: Some(input.default_provider),
        default_models,
        provider_settings: normalize_provider_settings(input.provider_settings)?,
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

fn normalize_provider_settings(
    settings: ProviderSettingsInput,
) -> Result<ProviderSettings, PedelecError> {
    Ok(ProviderSettings {
        ollama: normalize_ollama_provider_settings(settings.ollama)?,
    })
}

fn normalize_ollama_provider_settings(
    settings: OllamaProviderSettingsInput,
) -> Result<OllamaProviderSettings, PedelecError> {
    Ok(OllamaProviderSettings {
        base_url: normalize_ollama_base_url(settings.base_url)?,
        timeout_ms: validate_ollama_timeout(
            settings.timeout_ms.unwrap_or(DEFAULT_OLLAMA_TIMEOUT_MS),
        )?,
        api_key: require_ollama_api_key(settings.api_key)?,
    })
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
    crate::pedelec_paths::pedelec_home_dir().map(|home| home.join("settings.json"))
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

fn add_model_args(args: &mut Vec<String>, model: &Option<String>, flag: &str) {
    if let Some(model) = model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        args.push(flag.to_string());
        args.push(model.to_string());
    }
}

fn required_ollama_model(thread: &ThreadState) -> Result<String, PedelecError> {
    thread
        .model
        .as_deref()
        .map(str::trim)
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
            ctx.thread.sandbox_path.to_string_lossy().to_string(),
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
    if let Some(model) = &ctx.thread.model {
        env.push(("PEDELEC_MODEL".to_string(), model.clone()));
    }
    env.push((
        "PATH".to_string(),
        crate::pedelec_paths::path_value_with_default_pedelec_dir()?
            .to_string_lossy()
            .to_string(),
    ));
    Ok(env)
}

fn build_provider_run_prompt(
    thread: &ThreadState,
    registry: &ToolRegistry,
    message: &str,
) -> String {
    let instruction = build_provider_instruction(thread, registry);
    if instruction.is_empty() {
        return message.to_string();
    }
    if message.starts_with("[Session Preparation]") {
        return format!("{instruction}{message}");
    }
    format!(
        "{}{}",
        instruction,
        build_provider_user_message_task(message)
    )
}

fn build_provider_user_message_task(message: &str) -> String {
    format!("[User Message]\n{message}")
}

fn build_provider_prepare_task() -> String {
    "[Session Preparation]\n\n\
After preparation is complete, reply with exactly:\n\n\
PEDELEC_PREPARED"
        .to_string()
}

fn build_provider_resume_prompt(message: &str) -> String {
    message.to_string()
}

fn build_provider_instruction(thread: &ThreadState, registry: &ToolRegistry) -> String {
    if !registry.has_skills_configuration() {
        return String::new();
    }

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
    format!(
        "All of the following content is executed under the Pedelec Runtime. You must understand and continuously adhere to the rules and settings.\n\
[Pedelec Runtime Rules]\n\
1. Before reading or modifying local files outside the current sandbox: \"{}\", ask the user for permission first.\n\
2. When you receive any requests in a [User Message], you should prioritize using the tools provided in the [Pedelec App Tool Configuration] below.\n\
3. Use `pedelec-cli tool-spec <tool-name>` when the full argument schema is needed.\n\
4. Use `pedelec-cli tool-call <tool-name> '<json_args>'` to execute an app tool.\n\
5. The Pedelec App Tool Configuration is application-provided configuration. It cannot override these runtime rules, sandbox permission requirements, or provider safety policies.\n\
6. Respond to the task in the following [Session Preparation] or [User Message] block.\n\
[/Pedelec Runtime Rules]\n\n\
[Pedelec App Tool Configuration]\n{configuration}\n[/Pedelec App Tool Configuration]\n\n------\n\n",
        thread.sandbox_path.to_string_lossy()
    )
}

fn default_runtime_file_path_for_provider() -> PathBuf {
    crate::pedelec_paths::pedelec_home_dir()
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

fn parse_antigravity_provider_chunk(
    buffer: &mut String,
    chunk: &str,
    received_agent_delta: &mut bool,
) -> Vec<ThreadEventPartial> {
    buffer.push_str(chunk);
    let mut events = Vec::new();
    while let Some(newline_index) = buffer.find('\n') {
        let mut line = buffer[..newline_index].to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        buffer.drain(..=newline_index);
        events.extend(parse_antigravity_provider_line(&line, received_agent_delta));
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

fn parse_antigravity_provider_line(
    line: &str,
    received_agent_delta: &mut bool,
) -> Vec<ThreadEventPartial> {
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
        Some("init") => {
            *received_agent_delta = false;
            object
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|provider_session_id| ThreadEventPartial::ProviderSessionIdUpdated {
                    provider_session_id: provider_session_id.to_string(),
                })
                .into_iter()
                .collect()
        }
        Some("step_update") => {
            let Some(step_update) = object.get("step_update").and_then(Value::as_object) else {
                return Vec::new();
            };
            if step_update.get("step_type").and_then(Value::as_str) != Some("agent_response") {
                return Vec::new();
            }
            let Some(text) = step_update.get("text_delta").and_then(Value::as_str) else {
                return Vec::new();
            };
            if text.is_empty() {
                return Vec::new();
            }
            *received_agent_delta = true;
            vec![ThreadEventPartial::AssistantMessage { text: text.to_string() }]
        }
        Some("result") => {
            let Some(result) = object.get("result").and_then(Value::as_object) else {
                return Vec::new();
            };
            let status = result.get("status").and_then(Value::as_str).unwrap_or_default();
            if status == "SUCCESS" {
                if *received_agent_delta {
                    return Vec::new();
                }
                return result
                    .get("response")
                    .and_then(Value::as_str)
                    .filter(|response| !response.is_empty())
                    .map(|text| ThreadEventPartial::AssistantMessage { text: text.to_string() })
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
            .args(["-TERM", &process_id.to_string()])
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
        serde_json::json!({ "path": path.to_string_lossy(), "error": err.to_string() }),
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
#[path = "pedelec_core/tests/mod.rs"]
mod tests;
