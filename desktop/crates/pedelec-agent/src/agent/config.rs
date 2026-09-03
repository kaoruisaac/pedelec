use super::error::AgentError;
use pedelec_shared::ollama::{
    normalize_ollama_base_url, validate_ollama_base_url, validate_ollama_timeout,
    DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS,
};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Ollama,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PedelecAgentServerConfig {
    pub provider: BackendKind,
    pub base_url: String,
    pub timeout_ms: u64,
    pub api_key: String,
    pub tavily_api_key: Option<String>,
    pub pedelec_cli_path: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
    pub session_root: Option<PathBuf>,
    pub max_transcript_bytes: u64,
    pub max_tool_rounds: usize,
    pub max_list_files: usize,
    pub max_file_bytes: u64,
    pub max_image_bytes: u64,
    pub pedelec_cli_timeout_ms: u64,
}

impl PedelecAgentServerConfig {
    pub fn tool_host_config(&self) -> ToolHostConfig {
        ToolHostConfig {
            pedelec_cli_path: self.pedelec_cli_path.clone(),
            core_runtime_file: self.core_runtime_file.clone(),
            pedelec_cli_timeout_ms: self.pedelec_cli_timeout_ms,
        }
    }

    pub fn web_search_enabled(&self) -> bool {
        self.tavily_api_key.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct ToolHostConfig {
    pub pedelec_cli_path: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
    pub pedelec_cli_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AgentSessionConfig {
    pub requested_session_id: Option<String>,
    pub model: String,
    pub workspace_path: PathBuf,
    pub host_instructions: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerConfigResolveInputs {
    pub env_file: Option<PathBuf>,
    pub settings_path: Option<PathBuf>,
    pub pedelec_cli_path: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
    pub session_root: Option<PathBuf>,
}

pub fn resolve_server_config() -> Result<PedelecAgentServerConfig, AgentError> {
    resolve_server_config_from(ServerConfigResolveInputs::default())
}

pub fn resolve_server_config_from(
    inputs: ServerConfigResolveInputs,
) -> Result<PedelecAgentServerConfig, AgentError> {
    let env_file = inputs
        .env_file
        .unwrap_or_else(|| PathBuf::from(".env.local"));
    let file_env = read_env_file(&env_file)?;
    let settings_path = match inputs.settings_path {
        Some(path) => path,
        None => default_settings_file_path()?,
    };
    let ollama_settings = read_ollama_settings(&settings_path)?;

    let provider_name = env::var("PEDELEC_AGENT_PROVIDER")
        .ok()
        .or_else(|| file_env.get("PEDELEC_AGENT_PROVIDER").cloned())
        .unwrap_or_else(|| "ollama".into());
    let provider = parse_provider(&provider_name)?;
    let api_key = normalize_ollama_api_key(env::var("OLLAMA_API_KEY").ok())?;
    let tavily_api_key = normalize_tavily_api_key(env::var("TAVILY_API_KEY").ok());

    Ok(PedelecAgentServerConfig {
        provider,
        base_url: ollama_settings.base_url,
        timeout_ms: ollama_settings.timeout_ms,
        api_key,
        tavily_api_key,
        pedelec_cli_path: inputs
            .pedelec_cli_path
            .or_else(|| env_path("PEDELEC_CLI_PATH"))
            .or_else(|| env_file_path(&file_env, "PEDELEC_CLI_PATH")),
        core_runtime_file: inputs
            .core_runtime_file
            .or_else(|| env_path("PEDELEC_CORE_RUNTIME_FILE"))
            .or_else(|| env_path("PEDELEC_CORE_IPC_RUNTIME_FILE"))
            .or_else(|| env_file_path(&file_env, "PEDELEC_CORE_RUNTIME_FILE")),
        session_root: inputs.session_root,
        max_transcript_bytes: get_u64(&file_env, "PEDELEC_AGENT_MAX_TRANSCRIPT_BYTES", 1_048_576)?,
        max_tool_rounds: get_usize(&file_env, "PEDELEC_AGENT_MAX_TOOL_ROUNDS", 100)?,
        max_list_files: get_usize(&file_env, "PEDELEC_AGENT_MAX_LIST_FILES", 200)?,
        max_file_bytes: get_u64(&file_env, "PEDELEC_AGENT_MAX_FILE_BYTES", 262_144)?,
        max_image_bytes: positive_u64(
            &file_env,
            "PEDELEC_AGENT_MAX_IMAGE_BYTES",
            20 * 1024 * 1024,
        )?,
        pedelec_cli_timeout_ms: get_u64(&file_env, "PEDELEC_AGENT_PEDELEC_CLI_TIMEOUT_MS", 60_000)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedOllamaSettings {
    base_url: String,
    timeout_ms: u64,
}

fn default_settings_file_path() -> Result<PathBuf, AgentError> {
    pedelec_shared::paths::pedelec_home_dir()
        .map(|home| home.join("settings.json"))
        .map_err(|err| AgentError {
            code: err.code,
            message: err.message,
            details: err.details,
        })
}

fn read_ollama_settings(path: &Path) -> Result<ResolvedOllamaSettings, AgentError> {
    if !path.exists() {
        return Ok(default_ollama_settings());
    }

    let content = fs::read_to_string(path).map_err(|err| {
        AgentError::with_details(
            "CONFIG_ERROR",
            "Failed to read Pedelec settings",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })?;
    let value = serde_json::from_str::<serde_json::Value>(&content).map_err(|err| {
        AgentError::with_details(
            "CONFIG_ERROR",
            "Pedelec settings file was not valid JSON",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })?;
    let Some(ollama) = value
        .get("providerSettings")
        .and_then(|settings| settings.get("ollama"))
    else {
        return Ok(default_ollama_settings());
    };

    let base_url = match ollama.get("baseUrl") {
        None | Some(serde_json::Value::Null) => DEFAULT_OLLAMA_BASE_URL.to_string(),
        Some(serde_json::Value::String(value)) => {
            if value.trim().is_empty() {
                normalize_ollama_base_url(None).map_err(agent_config_error_from_pedelec)?
            } else {
                validate_ollama_base_url(value).map_err(agent_config_error_from_pedelec)?
            }
        }
        Some(value) => {
            return Err(AgentError::with_details(
                "CONFIG_ERROR",
                "Ollama Base URL in Pedelec settings must be a string.",
                serde_json::json!({ "field": "providerSettings.ollama.baseUrl", "value": value }),
            ));
        }
    };

    let timeout_ms = match ollama.get("timeoutMs") {
        None | Some(serde_json::Value::Null) => DEFAULT_OLLAMA_TIMEOUT_MS,
        Some(serde_json::Value::Number(number)) => {
            let value = number.as_u64().ok_or_else(|| {
                AgentError::with_details(
                    "CONFIG_ERROR",
                    "Ollama timeout in Pedelec settings must be a positive integer.",
                    serde_json::json!({ "field": "providerSettings.ollama.timeoutMs", "value": number }),
                )
            })?;
            validate_ollama_timeout(value).map_err(agent_config_error_from_pedelec)?
        }
        Some(value) => {
            return Err(AgentError::with_details(
                "CONFIG_ERROR",
                "Ollama timeout in Pedelec settings must be a positive integer.",
                serde_json::json!({ "field": "providerSettings.ollama.timeoutMs", "value": value }),
            ));
        }
    };

    Ok(ResolvedOllamaSettings {
        base_url,
        timeout_ms,
    })
}

fn default_ollama_settings() -> ResolvedOllamaSettings {
    ResolvedOllamaSettings {
        base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
        timeout_ms: DEFAULT_OLLAMA_TIMEOUT_MS,
    }
}

fn agent_config_error_from_pedelec(err: pedelec_shared::error::PedelecError) -> AgentError {
    AgentError {
        code: err.code,
        message: err.message,
        details: err.details,
    }
}

fn normalize_ollama_api_key(value: Option<String>) -> Result<String, AgentError> {
    let trimmed = value.as_deref().map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return Err(AgentError::new(
            "OLLAMA_API_KEY_REQUIRED",
            "Ollama API key is required. For local models, enter 'ollama'.",
        ));
    }
    Ok(trimmed.to_string())
}

fn normalize_tavily_api_key(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn parse_provider(value: &str) -> Result<BackendKind, AgentError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ollama" => Ok(BackendKind::Ollama),
        other => Err(AgentError::with_details(
            "CONFIG_ERROR",
            "Unsupported model provider",
            serde_json::json!({ "provider": other }),
        )),
    }
}

fn read_env_file(path: &Path) -> Result<HashMap<String, String>, AgentError> {
    let mut values = HashMap::new();
    if !path.exists() {
        return Ok(values);
    }
    let content = fs::read_to_string(path).map_err(|err| {
        AgentError::with_details(
            "CONFIG_ERROR",
            "Failed to read env file",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            values.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            );
        }
    }
    Ok(values)
}

fn get_value(file_env: &HashMap<String, String>, key: &str) -> Option<String> {
    env::var(key).ok().or_else(|| file_env.get(key).cloned())
}

fn get_u64(file_env: &HashMap<String, String>, key: &str, default: u64) -> Result<u64, AgentError> {
    match get_value(file_env, key) {
        Some(value) => value.parse::<u64>().map_err(|_| {
            AgentError::with_details(
                "CONFIG_ERROR",
                "Invalid integer config value",
                serde_json::json!({ "key": key, "value": value }),
            )
        }),
        None => Ok(default),
    }
}

fn get_usize(
    file_env: &HashMap<String, String>,
    key: &str,
    default: usize,
) -> Result<usize, AgentError> {
    Ok(get_u64(file_env, key, default as u64)? as usize)
}

fn positive_u64(
    file_env: &HashMap<String, String>,
    key: &str,
    default: u64,
) -> Result<u64, AgentError> {
    let value = get_u64(file_env, key, default)?;
    if value == 0 {
        return Err(AgentError::with_details(
            "CONFIG_ERROR",
            "Config value must be a positive integer",
            serde_json::json!({ "key": key }),
        ));
    }
    Ok(value)
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_file_path(file_env: &HashMap<String, String>, key: &str) -> Option<PathBuf> {
    file_env
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_test_ollama_api_key() {
        env::set_var("OLLAMA_API_KEY", "ollama");
    }

    fn resolve_with(temp: &tempfile::TempDir, settings: Option<&str>) -> PedelecAgentServerConfig {
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "PEDELEC_AGENT_MAX_TOOL_ROUNDS=8\n").unwrap();
        let settings_file = temp.path().join("settings.json");
        if let Some(content) = settings {
            fs::write(&settings_file, content).unwrap();
        }
        set_test_ollama_api_key();
        resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(settings_file),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap()
    }

    #[test]
    fn server_config_does_not_require_a_session_model() {
        let temp = tempfile::tempdir().unwrap();
        let config = resolve_with(&temp, None);
        assert_eq!(config.provider, BackendKind::Ollama);
        assert!(!config.api_key.is_empty());
        assert_eq!(config.max_tool_rounds, 8);
    }

    #[test]
    fn ollama_settings_are_read_from_pedelec_settings_file() {
        let temp = tempfile::tempdir().unwrap();
        let config = resolve_with(
            &temp,
            Some(
                r#"{
                    "providerSettings": {
                        "ollama": {
                            "baseUrl": "http://127.0.0.1:4567/",
                            "timeoutMs": 3456
                        }
                    }
                }"#,
            ),
        );

        assert_eq!(config.base_url, "http://127.0.0.1:4567");
        assert_eq!(config.timeout_ms, 3456);
        assert!(!config.api_key.is_empty());
    }

    #[test]
    fn ollama_settings_default_when_file_or_fields_are_missing() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "").unwrap();
        let cli_inputs = ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(temp.path().join("missing.json")),
            ..ServerConfigResolveInputs::default()
        };

        set_test_ollama_api_key();
        let missing_file = resolve_server_config_from(cli_inputs.clone()).unwrap();
        assert_eq!(missing_file.base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(missing_file.timeout_ms, DEFAULT_OLLAMA_TIMEOUT_MS);

        set_test_ollama_api_key();
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"  "}}}"#,
        )
        .unwrap();
        let missing_fields = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(temp.path().join(".env.local")),
            settings_path: Some(settings_file),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap();
        assert_eq!(missing_fields.base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(missing_fields.timeout_ms, DEFAULT_OLLAMA_TIMEOUT_MS);
    }

    #[test]
    fn ollama_settings_reject_invalid_values() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "").unwrap();
        set_test_ollama_api_key();
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"ftp://127.0.0.1","timeoutMs":120000}}}"#,
        )
        .unwrap();
        let url_err = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file.clone()),
            settings_path: Some(settings_file.clone()),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap_err();
        assert_eq!(url_err.code, "OLLAMA_BASE_URL_INVALID");

        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"http://127.0.0.1:11434","timeoutMs":0}}}"#,
        )
        .unwrap();
        let timeout_err = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(settings_file),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap_err();
        assert_eq!(timeout_err.code, "OLLAMA_REQUEST_FAILED");
    }

    #[test]
    fn ollama_base_url_timeout_env_and_env_file_values_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &env_file,
            "OLLAMA_BASE_URL=http://127.0.0.1:9999\nOLLAMA_TIMEOUT_MS=999\n",
        )
        .unwrap();
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"http://127.0.0.1:4567","timeoutMs":3456}}}"#,
        )
        .unwrap();
        env::set_var("OLLAMA_BASE_URL", "http://127.0.0.1:8888");
        env::set_var("OLLAMA_TIMEOUT_MS", "888");
        set_test_ollama_api_key();

        let config = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(settings_file),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap();

        env::remove_var("OLLAMA_BASE_URL");
        env::remove_var("OLLAMA_TIMEOUT_MS");
        assert_eq!(config.base_url, "http://127.0.0.1:4567");
        assert_eq!(config.timeout_ms, 3456);
    }

    #[test]
    fn ollama_api_key_normalizes_required_process_env_value() {
        let missing = normalize_ollama_api_key(None).unwrap_err();
        assert_eq!(missing.code, "OLLAMA_API_KEY_REQUIRED");
        let blank = normalize_ollama_api_key(Some("  ".into())).unwrap_err();
        assert_eq!(blank.code, "OLLAMA_API_KEY_REQUIRED");
        assert_eq!(
            normalize_ollama_api_key(Some("  ollama  ".into())).unwrap(),
            "ollama"
        );
    }

    #[test]
    fn tavily_api_key_is_optional_and_trimmed() {
        assert_eq!(normalize_tavily_api_key(None), None);
        assert_eq!(normalize_tavily_api_key(Some("  \t".into())), None);
        assert_eq!(
            normalize_tavily_api_key(Some(" key ".into())),
            Some("key".into())
        );
    }

    #[test]
    fn ollama_api_key_ignores_env_file_value_when_process_env_is_set() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        let settings_file = temp.path().join("settings.json");
        fs::write(&env_file, "OLLAMA_API_KEY=env-file-key\n").unwrap();
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"http://127.0.0.1:4567","timeoutMs":3456}}}"#,
        )
        .unwrap();
        env::set_var("OLLAMA_API_KEY", "process-key");

        let config = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(settings_file),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap();
        assert_ne!(config.api_key, "env-file-key");
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "PEDELEC_AGENT_PROVIDER=openai\n").unwrap();
        set_test_ollama_api_key();
        let err = resolve_server_config_from(ServerConfigResolveInputs {
            env_file: Some(env_file),
            settings_path: Some(temp.path().join("missing.json")),
            ..ServerConfigResolveInputs::default()
        })
        .unwrap_err();
        assert_eq!(err.code, "CONFIG_ERROR");
    }
}
