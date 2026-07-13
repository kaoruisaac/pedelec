use super::config::{AgentConfig, ModelProvider};
use super::error::AgentError;
use base64::Engine;
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
    #[serde(skip)]
    pub attachments: Vec<ModelAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelAttachment {
    Image { media_type: String, bytes: Vec<u8> },
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
    api_key: String,
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
            api_key: config.ollama_api_key.clone(),
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
        let serialized = messages.iter().map(ollama_message).collect::<Vec<_>>();
        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "messages": serialized,
            "tools": tools,
            "options": {
                "num_ctx": 8192
            }
        });
        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .bearer_auth(&self.api_key)
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
                    if code == "OLLAMA_UNAVAILABLE" {
                        "Ollama is unavailable. Check the Base URL, network connection, and timeout setting."
                    } else {
                        "Ollama request failed."
                    },
                    serde_json::json!({ "error": err.to_string(), "timeoutMs": self.timeout_ms }),
                )
            })?;
        let status = response.status();
        let text = response.text().map_err(|err| {
            AgentError::with_details(
                "OLLAMA_REQUEST_FAILED",
                "Ollama request failed.",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
        if !status.is_success() {
            return Err(ollama_http_status_error(status.as_u16(), &text));
        }
        parse_ollama_response(&text)
    }
}

fn ollama_message(message: &ModelMessage) -> Value {
    // Ollama Cloud validates message content as a string. Send an empty string
    // for messages such as assistant tool calls instead of serializing `null`.
    let mut value = serde_json::json!({
        "role": message.role,
        "content": message.content.clone().unwrap_or_default(),
    });
    if let Some(tool_calls) = &message.tool_calls {
        value["tool_calls"] = serde_json::json!(tool_calls);
    }
    if let Some(name) = &message.tool_name {
        value["tool_name"] = Value::String(name.clone());
    }
    let images = message
        .attachments
        .iter()
        .map(|attachment| match attachment {
            ModelAttachment::Image { bytes, .. } => {
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }
        })
        .collect::<Vec<_>>();
    if !images.is_empty() {
        value["images"] = serde_json::json!(images);
    }
    value
}

fn ollama_http_status_error(status: u16, body: &str) -> AgentError {
    let lower_body = body.to_ascii_lowercase();
    let (code, message) = match status {
        401 | 403 => (
            "OLLAMA_AUTH_FAILED",
            "Ollama authentication failed. Check your API key.",
        ),
        429 => (
            "OLLAMA_CLOUD_LIMIT_EXCEEDED",
            "Ollama Cloud limit was exceeded. Try again later or check your Ollama account usage.",
        ),
        404 if lower_body.contains("model") && lower_body.contains("not found") => (
            "OLLAMA_MODEL_NOT_FOUND",
            "Ollama model was not found. Refresh the model list and choose an available model.",
        ),
        _ => ("OLLAMA_REQUEST_FAILED", "Ollama request failed."),
    };
    AgentError::with_details(
        code,
        message,
        serde_json::json!({ "status": status, "body": body }),
    )
}

fn parse_ollama_response(text: &str) -> Result<ModelTurnOutput, AgentError> {
    let value = serde_json::from_str::<Value>(text).map_err(|err| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama response was invalid.",
            serde_json::json!({ "error": err.to_string(), "body": text }),
        )
    })?;
    let message = value.get("message").ok_or_else(|| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama response was invalid.",
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
    use crate::agent::config::{AgentConfig, ModelProvider};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    #[test]
    fn parses_ollama_tool_call() {
        let output = parse_ollama_response(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read_text_file","arguments":{"path":"README.md"}}}]}}"#,
        )
        .unwrap();

        assert_eq!(output.tool_calls[0].function.name, "fs.read_text_file");
    }

    #[test]
    fn serializes_image_only_on_the_message_that_carries_the_attachment() {
        let tool_message = ModelMessage {
            role: "tool".into(),
            content: Some(r#"{"path":"image.png"}"#.into()),
            tool_calls: None,
            attachments: vec![],
            tool_name: Some("fs.read_image".into()),
        };
        let image_message = ModelMessage {
            role: "user".into(),
            content: Some("The image returned by fs.read_image is attached.".into()),
            tool_calls: None,
            attachments: vec![ModelAttachment::Image {
                media_type: "image/png".into(),
                bytes: vec![0, 1, 2],
            }],
            tool_name: None,
        };

        let serialized_tool = ollama_message(&tool_message);
        let serialized_image = ollama_message(&image_message);

        assert_eq!(serialized_tool["content"], r#"{"path":"image.png"}"#);
        assert!(serialized_tool.get("images").is_none());
        assert!(serialized_tool.get("tool_calls").is_none());
        assert_eq!(serialized_image["role"], "user");
        assert_eq!(serialized_image["images"], serde_json::json!(["AAEC"]));
    }

    #[test]
    fn serializes_missing_content_as_an_empty_string() {
        let serialized = ollama_message(&ModelMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            attachments: vec![],
            tool_name: None,
        });

        assert_eq!(serialized["content"], "");
        assert!(serialized.get("tool_calls").is_none());
    }

    #[test]
    fn ollama_adapter_posts_native_chat_with_authorization_header() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let body = r#"{"message":{"role":"assistant","content":"hello"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });
        let config = test_config(format!("http://{addr}"), "ollama_chat_key");
        let adapter = OllamaAdapter::new(&config).unwrap();

        let output = adapter
            .run_turn(
                &[ModelMessage {
                    role: "user".into(),
                    content: Some("hi".into()),
                    tool_calls: None,
                    attachments: vec![],
                    tool_name: None,
                }],
                &serde_json::json!([]),
            )
            .unwrap();
        let request = handle.join().unwrap();

        assert_eq!(output.text.as_deref(), Some("hello"));
        assert!(request.starts_with("POST /api/chat "));
        assert!(request.contains("authorization: Bearer ollama_chat_key"));
        assert!(request.contains("content-type: application/json"));
    }

    #[test]
    fn ollama_adapter_maps_status_without_leaking_api_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).unwrap();
            let body = r#"{"error":"unauthorized"}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let config = test_config(format!("http://{addr}"), "secret_api_key");
        let adapter = OllamaAdapter::new(&config).unwrap();

        let err = adapter.run_turn(&[], &serde_json::json!([])).unwrap_err();
        handle.join().unwrap();

        assert_eq!(err.code, "OLLAMA_AUTH_FAILED");
        assert!(!serde_json::to_string(&err)
            .unwrap()
            .contains("secret_api_key"));
    }

    fn test_config(base_url: String, api_key: &str) -> AgentConfig {
        AgentConfig {
            provider: ModelProvider::Ollama,
            provider_name: "ollama".into(),
            model: "fake".into(),
            ollama_base_url: base_url,
            ollama_timeout_ms: 120_000,
            ollama_api_key: api_key.into(),
            sandbox: PathBuf::from("."),
            pedelec_cli_path: None,
            core_runtime_file: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            max_image_bytes: 20 * 1024 * 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }
}
