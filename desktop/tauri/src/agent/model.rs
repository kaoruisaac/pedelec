use super::config::{AgentConfig, ModelProvider};
use super::error::AgentError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ModelToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelToolCall {
    pub function: ModelToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelToolFunction {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ModelTurnOutput {
    pub text: Option<String>,
    pub tool_calls: Vec<ModelToolCall>,
}

pub trait ModelAdapter {
    fn run_turn(
        &self,
        messages: &[ModelMessage],
        tools: &Value,
    ) -> Result<ModelTurnOutput, AgentError>;
}

pub fn adapter_for(config: &AgentConfig) -> Result<Box<dyn ModelAdapter>, AgentError> {
    match config.provider {
        ModelProvider::Ollama => Ok(Box::new(OllamaAdapter::new(config)?)),
        ModelProvider::OpenAI | ModelProvider::Gemini => Err(AgentError::with_details(
            "CONFIG_ERROR",
            "Provider is recognized but not implemented in MVP",
            serde_json::json!({ "provider": config.provider_name }),
        )),
    }
}

struct OllamaAdapter {
    base_url: String,
    model: String,
    timeout_ms: u64,
    client: reqwest::blocking::Client,
}

impl OllamaAdapter {
    fn new(config: &AgentConfig) -> Result<Self, AgentError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(config.ollama_timeout_ms))
            .build()
            .map_err(|err| {
                AgentError::with_details(
                    "OLLAMA_REQUEST_FAILED",
                    "Failed to create Ollama HTTP client",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
        Ok(Self {
            base_url: config.ollama_base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            timeout_ms: config.ollama_timeout_ms,
            client,
        })
    }
}

impl ModelAdapter for OllamaAdapter {
    fn run_turn(
        &self,
        messages: &[ModelMessage],
        tools: &Value,
    ) -> Result<ModelTurnOutput, AgentError> {
        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "messages": messages,
            "tools": tools,
            "options": {
                "num_ctx": 8192
            }
        });
        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(|err| {
                let code = if err.is_connect() || err.is_timeout() {
                    "OLLAMA_UNAVAILABLE"
                } else {
                    "OLLAMA_REQUEST_FAILED"
                };
                AgentError::with_details(
                    code,
                    "Ollama request failed",
                    serde_json::json!({ "error": err.to_string(), "timeoutMs": self.timeout_ms }),
                )
            })?;
        let status = response.status();
        let text = response.text().map_err(|err| {
            AgentError::with_details(
                "OLLAMA_REQUEST_FAILED",
                "Failed to read Ollama response",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
        if !status.is_success() {
            return Err(AgentError::with_details(
                "OLLAMA_REQUEST_FAILED",
                "Ollama returned an error",
                serde_json::json!({ "status": status.as_u16(), "body": text }),
            ));
        }
        parse_ollama_response(&text)
    }
}

fn parse_ollama_response(text: &str) -> Result<ModelTurnOutput, AgentError> {
    let value = serde_json::from_str::<Value>(text).map_err(|err| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama response was not valid JSON",
            serde_json::json!({ "error": err.to_string(), "body": text }),
        )
    })?;
    let message = value.get("message").ok_or_else(|| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama response did not include message",
            serde_json::json!({ "body": value }),
        )
    })?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_tool_call)
                .collect::<Vec<ModelToolCall>>()
        })
        .unwrap_or_default();
    Ok(ModelTurnOutput { text, tool_calls })
}

fn parse_tool_call(value: &Value) -> Option<ModelToolCall> {
    let function = value.get("function")?;
    let name = function.get("name")?.as_str()?.to_string();
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Some(ModelToolCall {
        function: ModelToolFunction { name, arguments },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ollama_tool_call() {
        let output = parse_ollama_response(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read_text_file","arguments":{"path":"README.md"}}}]}}"#,
        )
        .unwrap();

        assert_eq!(output.tool_calls[0].function.name, "fs.read_text_file");
    }
}
