use super::cli::CliArgs;
use super::error::AgentError;
use crate::pedelec_core::{
    normalize_ollama_base_url, validate_ollama_base_url, validate_ollama_timeout,
    DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS,
};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelProvider {
    Ollama,
    OpenAI,
    Gemini,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub provider: ModelProvider,
    pub provider_name: String,
    pub model: String,
    pub ollama_base_url: String,
    pub ollama_timeout_ms: u64,
    pub ollama_api_key: String,
    pub sandbox: PathBuf,
    pub pedelec_cli_path: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
    pub max_transcript_bytes: u64,
    pub max_tool_rounds: usize,
    pub max_list_files: usize,
    pub max_file_bytes: u64,
    pub pedelec_cli_timeout_ms: u64,
}

pub fn resolve_config(cli: &CliArgs) -> Result<AgentConfig, AgentError> {
    resolve_config_with_settings_path(cli, default_settings_file_path()?)
}

pub(crate) fn resolve_config_with_settings_path(
    cli: &CliArgs,
    settings_path: PathBuf,
) -> Result<AgentConfig, AgentError> {
    let env_file = cli
        .env_file
        .clone()
        .unwrap_or_else(|| PathBuf::from(".env.local"));
    let file_env = read_env_file(&env_file)?;
    let ollama_settings = read_ollama_settings(&settings_path)?;

    let provider_name = cli
        .provider
        .clone()
        .or_else(|| env::var("PEDELEC_AGENT_PROVIDER").ok())
        .or_else(|| file_env.get("PEDELEC_AGENT_PROVIDER").cloned())
        .unwrap_or_else(|| "ollama".into());
    let provider = parse_provider(&provider_name)?;

    let model = cli
        .model
        .clone()
        .or_else(|| env::var("PEDELEC_AGENT_MODEL").ok())
        .or_else(|| env::var("PEDELEC_MODEL").ok())
        .or_else(|| file_env.get("PEDELEC_AGENT_MODEL").cloned())
        .ok_or_else(|| AgentError::new("CONFIG_ERROR", "Model is required"))?;

    let sandbox = cli
        .sandbox
        .clone()
        .or_else(|| env_path("PEDELEC_AGENT_SANDBOX"))
        .or_else(|| env_file_path(&file_env, "PEDELEC_AGENT_SANDBOX"))
        .unwrap_or_else(|| PathBuf::from("."));
    let ollama_api_key = normalize_ollama_api_key(env::var("OLLAMA_API_KEY").ok())?;

    Ok(AgentConfig {
        provider,
        provider_name,
        model,
        ollama_base_url: ollama_settings.base_url,
        ollama_timeout_ms: ollama_settings.timeout_ms,
        ollama_api_key,
        sandbox,
        pedelec_cli_path: cli
            .pedelec_cli
            .clone()
            .or_else(|| env_path("PEDELEC_CLI_PATH"))
            .or_else(|| env_file_path(&file_env, "PEDELEC_CLI_PATH")),
        core_runtime_file: cli
            .core_runtime_file
            .clone()
            .or_else(|| env_path("PEDELEC_CORE_RUNTIME_FILE"))
            .or_else(|| env_path("PEDELEC_CORE_IPC_RUNTIME_FILE"))
            .or_else(|| env_file_path(&file_env, "PEDELEC_CORE_RUNTIME_FILE")),
        max_transcript_bytes: get_u64(&file_env, "PEDELEC_AGENT_MAX_TRANSCRIPT_BYTES", 1_048_576)?,
        max_tool_rounds: get_usize(&file_env, "PEDELEC_AGENT_MAX_TOOL_ROUNDS", 100)?,
        max_list_files: get_usize(&file_env, "PEDELEC_AGENT_MAX_LIST_FILES", 200)?,
        max_file_bytes: get_u64(&file_env, "PEDELEC_AGENT_MAX_FILE_BYTES", 262_144)?,
        pedelec_cli_timeout_ms: get_u64(&file_env, "PEDELEC_AGENT_PEDELEC_CLI_TIMEOUT_MS", 60_000)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedOllamaSettings {
    base_url: String,
    timeout_ms: u64,
}

fn default_settings_file_path() -> Result<PathBuf, AgentError> {
    crate::pedelec_paths::pedelec_home_dir()
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

fn agent_config_error_from_pedelec(err: crate::pedelec_core::PedelecError) -> AgentError {
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

fn parse_provider(value: &str) -> Result<ModelProvider, AgentError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ollama" => Ok(ModelProvider::Ollama),
        "openai" => Ok(ModelProvider::OpenAI),
        "gemini" => Ok(ModelProvider::Gemini),
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

    #[test]
    fn cli_values_win_over_env_file() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(
            &env_file,
            "PEDELEC_AGENT_PROVIDER=ollama\nPEDELEC_AGENT_MODEL=file-model\n",
        )
        .unwrap();
        let cli = CliArgs {
            session_id: None,
            model: Some("cli-model".into()),
            env_file: Some(env_file),
            ..CliArgs::default()
        };

        set_test_ollama_api_key();
        let config = resolve_config(&cli).unwrap();

        assert_eq!(config.model, "cli-model");
        assert!(!config.ollama_api_key.is_empty());
    }

    #[test]
    fn ollama_settings_are_read_from_pedelec_settings_file() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        let settings_file = temp.path().join("settings.json");
        fs::write(&env_file, "PEDELEC_AGENT_MODEL=file-model\n").unwrap();
        fs::write(
            &settings_file,
            r#"{
                "providerSettings": {
                    "ollama": {
                        "baseUrl": "http://127.0.0.1:4567/",
                        "timeoutMs": 3456
                    }
                }
            }"#,
        )
        .unwrap();
        let cli = CliArgs {
            env_file: Some(env_file),
            ..CliArgs::default()
        };

        set_test_ollama_api_key();
        let config = resolve_config_with_settings_path(&cli, settings_file).unwrap();

        assert_eq!(config.ollama_base_url, "http://127.0.0.1:4567");
        assert_eq!(config.ollama_timeout_ms, 3456);
        assert!(!config.ollama_api_key.is_empty());
    }

    #[test]
    fn ollama_settings_default_when_file_or_fields_are_missing() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "PEDELEC_AGENT_MODEL=file-model\n").unwrap();
        let cli = CliArgs {
            env_file: Some(env_file.clone()),
            ..CliArgs::default()
        };

        set_test_ollama_api_key();
        let missing_file =
            resolve_config_with_settings_path(&cli, temp.path().join("missing.json")).unwrap();
        assert_eq!(missing_file.ollama_base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(missing_file.ollama_timeout_ms, DEFAULT_OLLAMA_TIMEOUT_MS);

        set_test_ollama_api_key();
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"  "}}}"#,
        )
        .unwrap();
        let missing_fields = resolve_config_with_settings_path(&cli, settings_file).unwrap();
        assert_eq!(missing_fields.ollama_base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(missing_fields.ollama_timeout_ms, DEFAULT_OLLAMA_TIMEOUT_MS);
    }

    #[test]
    fn ollama_settings_reject_invalid_values() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        fs::write(&env_file, "PEDELEC_AGENT_MODEL=file-model\n").unwrap();
        let cli = CliArgs {
            env_file: Some(env_file),
            ..CliArgs::default()
        };

        set_test_ollama_api_key();
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"ftp://127.0.0.1","timeoutMs":120000}}}"#,
        )
        .unwrap();
        let url_err = resolve_config_with_settings_path(&cli, settings_file.clone()).unwrap_err();
        assert_eq!(url_err.code, "OLLAMA_BASE_URL_INVALID");

        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"http://127.0.0.1:11434","timeoutMs":0}}}"#,
        )
        .unwrap();
        let timeout_err = resolve_config_with_settings_path(&cli, settings_file).unwrap_err();
        assert_eq!(timeout_err.code, "OLLAMA_REQUEST_FAILED");
    }

    #[test]
    fn ollama_base_url_timeout_env_and_env_file_values_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &env_file,
            "PEDELEC_AGENT_MODEL=file-model\nOLLAMA_BASE_URL=http://127.0.0.1:9999\nOLLAMA_TIMEOUT_MS=999\n",
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
        let cli = CliArgs {
            env_file: Some(env_file),
            ..CliArgs::default()
        };

        let config = resolve_config_with_settings_path(&cli, settings_file).unwrap();

        env::remove_var("OLLAMA_BASE_URL");
        env::remove_var("OLLAMA_TIMEOUT_MS");
        assert_eq!(config.ollama_base_url, "http://127.0.0.1:4567");
        assert_eq!(config.ollama_timeout_ms, 3456);
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
    fn ollama_api_key_ignores_env_file_value_when_process_env_is_set() {
        let temp = tempfile::tempdir().unwrap();
        let env_file = temp.path().join(".env.local");
        let settings_file = temp.path().join("settings.json");
        fs::write(
            &env_file,
            "PEDELEC_AGENT_MODEL=file-model\nOLLAMA_API_KEY=env-file-key\n",
        )
        .unwrap();
        fs::write(
            &settings_file,
            r#"{"providerSettings":{"ollama":{"baseUrl":"http://127.0.0.1:4567","timeoutMs":3456}}}"#,
        )
        .unwrap();
        env::set_var("OLLAMA_API_KEY", "process-key");
        let cli = CliArgs {
            env_file: Some(env_file.clone()),
            ..CliArgs::default()
        };

        let config = resolve_config_with_settings_path(&cli, settings_file).unwrap();
        assert_ne!(config.ollama_api_key, "env-file-key");
    }
}
