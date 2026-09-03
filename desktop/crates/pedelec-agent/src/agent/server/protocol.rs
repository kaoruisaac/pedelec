use super::super::backend::InferenceUsage;
use super::super::conversation::NormalizedToolCall;
use super::super::error::AgentError;
use super::super::events::TurnToolResult;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

pub const PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC_VERSION: &str = "2.0";
pub const SERVER_NAME: &str = "pedelec-agent";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const APPLICATION_ERROR: i64 = -32000;

#[derive(Debug, Clone)]
pub struct JsonRpcRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    #[serde(default)]
    pub client_info: Option<ClientInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenParams {
    pub thread_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub model: String,
    pub workspace_path: String,
    #[serde(default)]
    pub host_instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseParams {
    pub thread_id: String,
    pub session_id: String,
    #[serde(default)]
    pub active_turn_id: Option<String>,
}

pub struct ProtocolWriter {
    out: Mutex<Box<dyn Write + Send>>,
    on_fatal: Arc<dyn Fn() + Send + Sync>,
}

impl ProtocolWriter {
    pub fn new(out: Box<dyn Write + Send>, on_fatal: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            out: Mutex::new(out),
            on_fatal,
        }
    }

    pub fn write_value(&self, value: &Value) {
        if let Err(err) = self.try_write(value) {
            eprintln!("pedelec-agent stdout writer failed: {err}");
            (self.on_fatal)();
        }
    }

    fn try_write(&self, value: &Value) -> Result<(), AgentError> {
        let mut out = lock_mutex(&self.out);
        serde_json::to_writer(&mut *out, value).map_err(|err| {
            AgentError::with_details(
                "PROTOCOL_WRITE_FAILED",
                "Failed to write JSON-RPC frame",
                json!({ "error": err.to_string() }),
            )
        })?;
        out.write_all(b"\n").map_err(AgentError::from)?;
        out.flush().map_err(AgentError::from)?;
        Ok(())
    }
}

pub fn parse_request_line(line: &str) -> Result<JsonRpcRequest, Value> {
    let value: Value = serde_json::from_str(line).map_err(|err| {
        rpc_error_frame(
            Value::Null,
            PARSE_ERROR,
            "Parse error",
            AgentError::with_details(
                "PARSE_ERROR",
                "Request was not valid JSON",
                json!({ "error": err.to_string() }),
            ),
        )
    })?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(rpc_error(
            value.get("id").cloned().unwrap_or(Value::Null),
            AgentError::new("INVALID_REQUEST", "JSON-RPC request must use jsonrpc 2.0."),
        ));
    }
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| {
            rpc_error(
                value.get("id").cloned().unwrap_or(Value::Null),
                AgentError::new("INVALID_REQUEST", "JSON-RPC method is required."),
            )
        })?
        .to_string();
    let Some(id) = value.get("id").cloned() else {
        return Err(rpc_error(
            Value::Null,
            AgentError::new(
                "INVALID_REQUEST",
                "JSON-RPC notifications are not supported.",
            ),
        ));
    };
    if id.is_null() || (!id.is_string() && !id.is_number()) {
        return Err(rpc_error(
            Value::Null,
            AgentError::new("INVALID_REQUEST", "JSON-RPC request id is invalid."),
        ));
    }
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Ok(JsonRpcRequest { id, method, params })
}

pub fn rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result
    })
}

pub fn rpc_error(id: Value, error: AgentError) -> Value {
    let code = jsonrpc_code(&error.code);
    let message = error.message.clone();
    rpc_error_frame(id, code, &message, error)
}

pub fn method_not_found(id: Value, method: &str) -> Value {
    rpc_error(
        id,
        AgentError::with_details(
            "METHOD_NOT_FOUND",
            "Unknown JSON-RPC method",
            json!({ "method": method }),
        ),
    )
}

pub fn invalid_params(id: Value, message: &str) -> Value {
    rpc_error(id, AgentError::new("INVALID_PARAMS", message))
}

pub fn decode_params<T: for<'de> Deserialize<'de>>(id: &Value, params: Value) -> Result<T, Value> {
    serde_json::from_value(params).map_err(|err| {
        rpc_error(
            id.clone(),
            AgentError::with_details(
                "INVALID_PARAMS",
                "Request params were invalid.",
                json!({ "error": err.to_string() }),
            ),
        )
    })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params
    })
}

pub fn turn_identity(thread_id: &str, session_id: &str, turn_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "sessionId": session_id,
        "turnId": turn_id
    })
}

pub fn usage_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    usage: &InferenceUsage,
) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["usage"] = json!(usage);
    params
}

pub fn assistant_delta_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["text"] = Value::String(text.to_string());
    params
}

pub fn assistant_message_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> Value {
    assistant_delta_params(thread_id, session_id, turn_id, text)
}

pub fn completed_success_params(thread_id: &str, session_id: &str, turn_id: &str) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["status"] = Value::String("completed".into());
    params
}

pub fn completed_failed_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    error: &AgentError,
) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["status"] = Value::String("failed".into());
    params["error"] = agent_error_data(error);
    params
}

pub fn tool_call_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    call: &NormalizedToolCall,
) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["toolCallId"] = Value::String(call.id.clone());
    params["name"] = Value::String(call.name.clone());
    params["arguments"] = sanitize_value(&call.arguments);
    params
}

pub fn tool_result_params(
    thread_id: &str,
    session_id: &str,
    turn_id: &str,
    result: &TurnToolResult,
) -> Value {
    let mut params = turn_identity(thread_id, session_id, turn_id);
    params["toolCallId"] = Value::String(result.tool_call_id.clone());
    params["name"] = Value::String(result.name.clone());
    params["ok"] = Value::Bool(result.ok);
    if let Some(content) = &result.content {
        params["content"] = sanitize_value(content);
    }
    if let Some(error) = &result.error {
        params["error"] = agent_error_data(error);
    }
    params
}

pub fn agent_error_data(error: &AgentError) -> Value {
    json!({
        "code": error.code,
        "message": error.message,
        "details": error.details.clone().unwrap_or_else(|| json!({}))
    })
}

fn rpc_error_frame(id: Value, code: i64, message: &str, error: AgentError) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": agent_error_data(&error)
        }
    })
}

fn jsonrpc_code(code: &str) -> i64 {
    match code {
        "PARSE_ERROR" => PARSE_ERROR,
        "INVALID_REQUEST" | "PROTOCOL_ERROR" | "PROTOCOL_VERSION_UNSUPPORTED" => INVALID_REQUEST,
        "METHOD_NOT_FOUND" => METHOD_NOT_FOUND,
        "INVALID_PARAMS" | "INVALID_ARGUMENT" => INVALID_PARAMS,
        _ => APPLICATION_ERROR,
    }
}

fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                if is_sensitive_key(key) {
                    continue;
                }
                out.insert(key.clone(), sanitize_value(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::String(text) if text.len() > 16_384 => {
            Value::String(format!("{}…", &text[..16_384]))
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "apikey" | "api_key" | "authorization" | "bytes" | "image" | "images"
    )
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}
