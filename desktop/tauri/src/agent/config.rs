use super::cli::CliArgs;
use super::error::AgentError;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

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
    pub home: PathBuf,
    pub sandbox: PathBuf,
    pub web_search_provider: Option<String>,
    pub web_search_timeout_ms: u64,
    pub web_search_max_results: usize,
    pub brave_search_api_key: Option<String>,
    pub pedelec_cli_path: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
    pub max_transcript_bytes: u64,
    pub max_tool_rounds: usize,
    pub max_list_files: usize,
    pub max_file_bytes: u64,
    pub pedelec_cli_timeout_ms: u64,
}

pub fn resolve_config(cli: &CliArgs) -> Result<AgentConfig, AgentError> {
    let env_file = cli
        .env_file
        .clone()
        .unwrap_or_else(|| PathBuf::from(".env.local"));
    let file_env = read_env_file(&env_file)?;

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

    Ok(AgentConfig {
        provider,
        provider_name,
        model,
        ollama_base_url: get_value(&file_env, "OLLAMA_BASE_URL")
            .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.into()),
        ollama_timeout_ms: get_u64(&file_env, "OLLAMA_TIMEOUT_MS", 120_000)?,
        home: env_path("PEDELEC_AGENT_HOME")
            .or_else(|| env_file_path(&file_env, "PEDELEC_AGENT_HOME"))
            .unwrap_or_else(|| PathBuf::from(".pedelec-agent")),
        sandbox,
        web_search_provider: get_optional_value(&file_env, "PEDELEC_AGENT_WEB_SEARCH_PROVIDER"),
        web_search_timeout_ms: get_u64(&file_env, "PEDELEC_AGENT_WEB_SEARCH_TIMEOUT_MS", 30_000)?,
        web_search_max_results: get_usize(&file_env, "PEDELEC_AGENT_WEB_SEARCH_MAX_RESULTS", 5)?,
        brave_search_api_key: get_optional_value(&file_env, "BRAVE_SEARCH_API_KEY"),
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
        max_tool_rounds: get_usize(&file_env, "PEDELEC_AGENT_MAX_TOOL_ROUNDS", 8)?,
        max_list_files: get_usize(&file_env, "PEDELEC_AGENT_MAX_LIST_FILES", 200)?,
        max_file_bytes: get_u64(&file_env, "PEDELEC_AGENT_MAX_FILE_BYTES", 262_144)?,
        pedelec_cli_timeout_ms: get_u64(&file_env, "PEDELEC_AGENT_PEDELEC_CLI_TIMEOUT_MS", 60_000)?,
    })
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

fn get_optional_value(file_env: &HashMap<String, String>, key: &str) -> Option<String> {
    get_value(file_env, key).filter(|value| !value.trim().is_empty())
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
            session_id: "s".into(),
            prompt: "p".into(),
            model: Some("cli-model".into()),
            env_file: Some(env_file),
            ..CliArgs::default()
        };

        let config = resolve_config(&cli).unwrap();

        assert_eq!(config.model, "cli-model");
    }
}
