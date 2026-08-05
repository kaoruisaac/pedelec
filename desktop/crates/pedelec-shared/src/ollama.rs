use crate::error::{error_codes, PedelecError};
use url::Url;

pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_TIMEOUT_MS: u64 = 120_000;

pub fn normalize_ollama_base_url(value: Option<String>) -> Result<String, PedelecError> {
    validate_ollama_base_url(
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OLLAMA_BASE_URL),
    )
}

pub fn validate_ollama_base_url(value: &str) -> Result<String, PedelecError> {
    let url = Url::parse(value.trim()).map_err(|err| {
        PedelecError::with_details(
            error_codes::OLLAMA_BASE_URL_INVALID,
            "Ollama base URL is invalid",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(PedelecError::new(
            error_codes::OLLAMA_BASE_URL_INVALID,
            "Ollama base URL is invalid",
        ));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn validate_ollama_timeout(value: u64) -> Result<u64, PedelecError> {
    if value == 0 {
        Err(PedelecError::new(
            error_codes::OLLAMA_REQUEST_FAILED,
            "Ollama timeout must be greater than zero",
        ))
    } else {
        Ok(value)
    }
}
