use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PedelecError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
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

pub mod error_codes {
    pub const IPC_UNAVAILABLE: &str = "IPC_UNAVAILABLE";
    pub const CORE_RUNTIME_UNAVAILABLE: &str = "CORE_RUNTIME_UNAVAILABLE";
    pub const OLLAMA_BASE_URL_INVALID: &str = "OLLAMA_BASE_URL_INVALID";
    pub const OLLAMA_REQUEST_FAILED: &str = "OLLAMA_REQUEST_FAILED";
}
