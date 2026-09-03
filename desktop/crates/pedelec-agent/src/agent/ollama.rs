use super::backend::{
    InferenceBackend, InferenceEvent, InferenceEventSink, InferenceMessage, InferenceRequest,
    InferenceResult, InferenceUsage, ModelCapabilities,
};
use super::config::PedelecAgentServerConfig;
use super::conversation::NormalizedToolCall;
use super::error::AgentError;
use super::tools::AgentToolDefinition;
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::time::Duration;

pub struct OllamaBackend {
    base_url: String,
    api_key: String,
    timeout_ms: u64,
    client: reqwest::blocking::Client,
    capability_cache: Mutex<HashMap<String, ModelCapabilities>>,
}

impl OllamaBackend {
    pub fn new(config: &PedelecAgentServerConfig) -> Result<Self, AgentError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|err| {
                AgentError::with_details(
                    "OLLAMA_REQUEST_FAILED",
                    "Failed to create Ollama HTTP client",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })?;
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            timeout_ms: config.timeout_ms,
            client,
            capability_cache: Mutex::new(HashMap::new()),
        })
    }

    fn probe_model(&self, model: &str) -> Result<ModelCapabilities, AgentError> {
        let response = self
            .client
            .post(format!("{}/api/show", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "model": model }).to_string())
            .send()
            .map_err(|err| map_transport_error(err, self.timeout_ms))?;
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
        parse_show_capabilities(&text)
    }
}

impl InferenceBackend for OllamaBackend {
    fn inspect_model(&self, model: &str) -> Result<ModelCapabilities, AgentError> {
        if let Some(cached) = lock_cache(&self.capability_cache).get(model).cloned() {
            return Ok(cached);
        }
        let capabilities = self.probe_model(model)?;
        lock_cache(&self.capability_cache).insert(model.to_string(), capabilities);
        Ok(capabilities)
    }

    fn infer(
        &self,
        request: InferenceRequest,
        sink: &mut dyn InferenceEventSink,
    ) -> Result<InferenceResult, AgentError> {
        let serialized = flatten_ollama_messages(&request.messages);
        let body = serde_json::json!({
            "model": request.model,
            "stream": true,
            "messages": serialized,
            "tools": ollama_tools(&request.tools),
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
            .map_err(|err| map_transport_error(err, self.timeout_ms))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            return Err(ollama_http_status_error(status.as_u16(), &text));
        }
        parse_ollama_stream(response, sink)
    }
}

fn lock_cache(
    cache: &Mutex<HashMap<String, ModelCapabilities>>,
) -> std::sync::MutexGuard<'_, HashMap<String, ModelCapabilities>> {
    cache.lock().unwrap_or_else(|err| err.into_inner())
}

fn ollama_tools(tools: &[AgentToolDefinition]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    }
                })
            })
            .collect(),
    )
}

fn flatten_ollama_messages(messages: &[InferenceMessage]) -> Vec<Value> {
    let mut serialized = Vec::new();
    for message in messages {
        serialized.push(ollama_message(message));
        if !message.attachments.is_empty() {
            serialized.push(ollama_image_follow_up(message));
        }
    }
    serialized
}

fn ollama_message(message: &InferenceMessage) -> Value {
    let mut value = serde_json::json!({
        "role": message.role,
        "content": message.content.clone().unwrap_or_default(),
    });
    if !message.tool_calls.is_empty() {
        value["tool_calls"] =
            Value::Array(message.tool_calls.iter().map(ollama_tool_call).collect());
    }
    if let Some(name) = &message.tool_name {
        value["tool_name"] = Value::String(name.clone());
    }
    value
}

fn ollama_tool_call(call: &NormalizedToolCall) -> Value {
    let mut value = serde_json::json!({
        "function": {
            "name": call.name,
            "arguments": call.arguments
        }
    });
    if !call.id.is_empty() {
        value["id"] = Value::String(call.id.clone());
    }
    value
}

fn ollama_image_follow_up(message: &InferenceMessage) -> Value {
    let images = message
        .attachments
        .iter()
        .map(|attachment| base64::engine::general_purpose::STANDARD.encode(&attachment.bytes))
        .collect::<Vec<_>>();
    serde_json::json!({
        "role": "user",
        "content": format!(
            "The image returned by fs.read_image is attached. Use it to answer the user's request. Metadata: {}",
            message.content.clone().unwrap_or_default()
        ),
        "images": images
    })
}

fn map_transport_error(err: reqwest::Error, timeout_ms: u64) -> AgentError {
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
        serde_json::json!({ "error": err.to_string(), "timeoutMs": timeout_ms }),
    )
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

fn parse_show_capabilities(text: &str) -> Result<ModelCapabilities, AgentError> {
    let value = serde_json::from_str::<Value>(text).map_err(|err| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama model inspect response was invalid.",
            serde_json::json!({ "error": err.to_string(), "body": text }),
        )
    })?;
    let caps = value.get("capabilities").and_then(Value::as_array);
    let has =
        |name: &str| caps.is_some_and(|items| items.iter().any(|item| item.as_str() == Some(name)));
    Ok(ModelCapabilities {
        tools: has("tools"),
        vision: has("vision"),
    })
}

fn parse_ollama_stream(
    response: reqwest::blocking::Response,
    sink: &mut dyn InferenceEventSink,
) -> Result<InferenceResult, AgentError> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    let mut saw_done = false;
    let mut saw_message = false;

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).map_err(|err| {
            AgentError::with_details(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama stream was invalid.",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let chunk = serde_json::from_str::<Value>(trimmed).map_err(|err| {
            AgentError::with_details(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama stream was invalid.",
                serde_json::json!({ "error": err.to_string(), "body": trimmed }),
            )
        })?;
        apply_stream_chunk(
            &chunk,
            &mut text,
            &mut tool_calls,
            &mut usage,
            &mut saw_done,
            &mut saw_message,
            sink,
        )?;
    }

    if !saw_done && !saw_message {
        return Err(AgentError::new(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama stream ended without a chat message.",
        ));
    }
    if !saw_done && saw_message {
        return Err(AgentError::new(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama stream ended before the terminal chunk.",
        ));
    }

    Ok(InferenceResult {
        text: if text.is_empty() { None } else { Some(text) },
        tool_calls,
        usage,
        finish_reason: if saw_done { Some("stop".into()) } else { None },
    })
}

fn apply_stream_chunk(
    chunk: &Value,
    text: &mut String,
    tool_calls: &mut Vec<NormalizedToolCall>,
    usage: &mut Option<InferenceUsage>,
    saw_done: &mut bool,
    saw_message: &mut bool,
    sink: &mut dyn InferenceEventSink,
) -> Result<(), AgentError> {
    if let Some(message) = chunk.get("message") {
        *saw_message = true;
        if let Some(delta) = message.get("content").and_then(Value::as_str) {
            if !delta.is_empty() {
                text.push_str(delta);
                sink.on_event(InferenceEvent::TextDelta(delta.to_string()));
            }
        }
        if let Some(items) = message.get("tool_calls").and_then(Value::as_array) {
            for item in items {
                tool_calls.push(parse_tool_call(item)?);
            }
        }
    }
    if chunk.get("done").and_then(Value::as_bool) == Some(true) {
        *saw_done = true;
        *usage = extract_usage(chunk);
    }
    Ok(())
}

fn extract_usage(chunk: &Value) -> Option<InferenceUsage> {
    let input_tokens = chunk.get("prompt_eval_count").and_then(Value::as_u64);
    let output_tokens = chunk.get("eval_count").and_then(Value::as_u64);
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(InferenceUsage::from_token_counts(
        input_tokens,
        output_tokens,
    ))
}

fn parse_tool_call(value: &Value) -> Result<NormalizedToolCall, AgentError> {
    let function = value.get("function").ok_or_else(|| {
        AgentError::with_details(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama tool call was missing function data.",
            serde_json::json!({ "toolCall": value }),
        )
    })?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            AgentError::with_details(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama tool call was missing a function name.",
                serde_json::json!({ "toolCall": value }),
            )
        })?;
    let arguments = match function.get("arguments") {
        None | Some(Value::Null) => serde_json::json!({}),
        Some(Value::String(raw)) => serde_json::from_str(raw).map_err(|err| {
            AgentError::with_details(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama tool call arguments were not valid JSON.",
                serde_json::json!({ "error": err.to_string(), "arguments": raw }),
            )
        })?,
        Some(other) => other.clone(),
    };
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(NormalizedToolCall {
        id,
        name: name.to_string(),
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{BackendKind, PedelecAgentServerConfig};
    use crate::agent::conversation::InferenceAttachment;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    struct CollectingSink {
        deltas: Vec<String>,
    }

    impl InferenceEventSink for CollectingSink {
        fn on_event(&mut self, event: InferenceEvent) {
            match event {
                InferenceEvent::TextDelta(text) => self.deltas.push(text),
            }
        }
    }

    fn test_config(base_url: String, api_key: &str) -> PedelecAgentServerConfig {
        PedelecAgentServerConfig {
            provider: BackendKind::Ollama,
            base_url,
            timeout_ms: 120_000,
            api_key: api_key.into(),
            tavily_api_key: None,
            pedelec_cli_path: None,
            core_runtime_file: None,
            session_root: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            max_image_bytes: 20 * 1024 * 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    #[test]
    fn parses_tool_call_arguments_as_json_value_and_optional_native_id() {
        let with_id = parse_tool_call(&serde_json::json!({
            "id": "call_1",
            "function": {"name": "fs.read_text_file", "arguments": {"path": "README.md"}}
        }))
        .unwrap();
        assert_eq!(with_id.id, "call_1");
        assert_eq!(with_id.arguments["path"], "README.md");

        let from_string = parse_tool_call(&serde_json::json!({
            "function": {"name": "fs.read_text_file", "arguments": "{\"path\":\"README.md\"}"}
        }))
        .unwrap();
        assert_eq!(from_string.id, "");
        assert_eq!(from_string.arguments["path"], "README.md");
        assert!(from_string.arguments.is_object());
    }

    #[test]
    fn serializes_image_follow_up_only_on_ollama_wire() {
        let tool_message = InferenceMessage {
            role: "tool".into(),
            content: Some(r#"{"path":"image.png"}"#.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some("t1".into()),
            tool_name: Some("fs.read_image".into()),
            attachments: vec![],
        };
        let image_message = InferenceMessage {
            role: "tool".into(),
            content: Some(r#"{"path":"image.png"}"#.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some("t1".into()),
            tool_name: Some("fs.read_image".into()),
            attachments: vec![InferenceAttachment {
                media_type: "image/png".into(),
                bytes: vec![0, 1, 2],
            }],
        };

        let serialized = flatten_ollama_messages(&[tool_message, image_message]);
        assert_eq!(serialized.len(), 3);
        assert_eq!(serialized[0]["role"], "tool");
        assert!(serialized[0].get("images").is_none());
        assert_eq!(serialized[1]["role"], "tool");
        assert_eq!(serialized[2]["role"], "user");
        assert_eq!(serialized[2]["images"], serde_json::json!(["AAEC"]));
        assert!(serialized[2]["content"]
            .as_str()
            .unwrap()
            .contains("fs.read_image"));
    }

    #[test]
    fn ollama_backend_streams_text_deltas_and_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let body = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":3,\"eval_count\":2}\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });
        let backend =
            OllamaBackend::new(&test_config(format!("http://{addr}"), "ollama_chat_key")).unwrap();
        let mut sink = CollectingSink { deltas: Vec::new() };
        let output = backend
            .infer(
                InferenceRequest {
                    model: "fake".into(),
                    messages: vec![InferenceMessage {
                        role: "user".into(),
                        content: Some("hi".into()),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        tool_name: None,
                        attachments: Vec::new(),
                    }],
                    tools: vec![],
                },
                &mut sink,
            )
            .unwrap();
        let request = handle.join().unwrap();

        assert_eq!(sink.deltas, vec!["Hel".to_string(), "lo".to_string()]);
        assert_eq!(output.text.as_deref(), Some("Hello"));
        assert_eq!(output.usage.as_ref().unwrap().input_tokens, Some(3));
        assert_eq!(output.usage.as_ref().unwrap().output_tokens, Some(2));
        assert_eq!(output.usage.as_ref().unwrap().total_tokens, Some(5));
        assert!(request.starts_with("POST /api/chat "));
        assert!(request.contains("authorization: Bearer ollama_chat_key"));
        assert!(request.contains("\"stream\":true"));
        assert!(!request.contains("secret_api_key"));
    }

    #[test]
    fn inspect_model_uses_generation_cache_and_splits_tools_vision() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).unwrap();
            let body = r#"{"capabilities":["tools","vision"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let backend = OllamaBackend::new(&test_config(format!("http://{addr}"), "ollama")).unwrap();
        let first = backend.inspect_model("fake").unwrap();
        let second = backend.inspect_model("fake").unwrap();
        handle.join().unwrap();
        assert_eq!(
            first,
            ModelCapabilities {
                tools: true,
                vision: true
            }
        );
        assert_eq!(first, second);
    }

    #[test]
    fn inspect_tools_false_vision_false_from_capabilities_list() {
        let caps = parse_show_capabilities(r#"{"capabilities":[]}"#).unwrap();
        assert!(!caps.tools);
        assert!(!caps.vision);
        let tools_only = parse_show_capabilities(r#"{"capabilities":["tools"]}"#).unwrap();
        assert!(tools_only.tools);
        assert!(!tools_only.vision);
    }

    #[test]
    fn ollama_backend_maps_status_without_leaking_api_key() {
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
        let backend =
            OllamaBackend::new(&test_config(format!("http://{addr}"), "secret_api_key")).unwrap();
        let err = backend
            .infer(
                InferenceRequest {
                    model: "fake".into(),
                    messages: vec![],
                    tools: vec![],
                },
                &mut CollectingSink { deltas: Vec::new() },
            )
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(err.code, "OLLAMA_AUTH_FAILED");
        assert!(!serde_json::to_string(&err)
            .unwrap()
            .contains("secret_api_key"));
    }

    #[test]
    fn malformed_stream_is_protocol_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).unwrap();
            let body = "not-json\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let backend = OllamaBackend::new(&test_config(format!("http://{addr}"), "ollama")).unwrap();
        let err = backend
            .infer(
                InferenceRequest {
                    model: "fake".into(),
                    messages: vec![],
                    tools: vec![],
                },
                &mut CollectingSink { deltas: Vec::new() },
            )
            .unwrap_err();
        assert_eq!(err.code, "OLLAMA_RESPONSE_INVALID");
    }
}
