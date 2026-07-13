use crate::pedelec_core::{error_codes, PedelecError};
use crate::pedelec_ipc::{
    connect_core_ipc, connect_core_ipc_with_runtime_path, read_bounded_json_line,
    send_core_ipc_request, send_core_ipc_request_with_runtime_path, write_json_line,
    CoreIpcRequest, CoreIpcResponse, MAX_CORE_IPC_MESSAGE_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

pub fn run() -> io::Result<()> {
    run_chrome_native_host(None)
}

fn run_chrome_native_host(runtime_file_path: Option<PathBuf>) -> io::Result<()> {
    let mut stdin = io::stdin();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let mut connection = NativeConnectionState::default();

    #[cfg(debug_assertions)]
    eprintln!("pedelec-native-host started");

    while let Some(message_result) = read_chrome_message(&mut stdin)? {
        let message = match message_result {
            Ok(message) => message,
            Err(err) => {
                let should_close = err.code == error_codes::MESSAGE_TOO_LARGE;
                let mut stdout = stdout.lock().unwrap();
                write_chrome_message(&mut *stdout, &native_error_response("", err))?;
                if should_close {
                    break;
                }
                continue;
            }
        };

        let request = match native_message_to_core_request(message) {
            Ok(request) => request,
            Err(response) => {
                let mut stdout = stdout.lock().unwrap();
                write_chrome_message(&mut *stdout, &response)?;
                continue;
            }
        };

        if request.r#type == "subscribe_thread" {
            let Some(thread_id) = subscription_thread_id(&request) else {
                let mut stdout = stdout.lock().unwrap();
                write_chrome_message(
                    &mut *stdout,
                    &native_error_response(
                        &request.request_id,
                        PedelecError::new(
                            error_codes::IPC_UNAVAILABLE,
                            "subscribe_thread requires threadId",
                        ),
                    ),
                )?;
                continue;
            };

            if !connection.mark_subscription(&thread_id) {
                let mut stdout = stdout.lock().unwrap();
                write_chrome_message(
                    &mut *stdout,
                    &native_ok_response(
                        &request.request_id,
                        serde_json::json!({ "subscribed": true, "duplicate": true }),
                    ),
                )?;
                continue;
            }

            let result = start_forward_subscription(
                request.clone(),
                Arc::clone(&stdout),
                runtime_file_path.as_deref(),
            );
            match result {
                Ok(true) => {}
                Ok(false) => connection.remove_subscription(&thread_id),
                Err(err) => {
                    connection.remove_subscription(&thread_id);
                    let mut stdout = stdout.lock().unwrap();
                    write_chrome_message(
                        &mut *stdout,
                        &native_error_response(&request.request_id, err),
                    )?;
                }
            }
        } else {
            let response = send_core_request(&request, runtime_file_path.as_deref());
            let native_response = core_response_to_native_response(response);
            let mut stdout = stdout.lock().unwrap();
            write_chrome_message(&mut *stdout, &native_response)?;
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct NativeConnectionState {
    subscriptions: HashSet<String>,
}

impl NativeConnectionState {
    fn mark_subscription(&mut self, thread_id: &str) -> bool {
        self.subscriptions.insert(thread_id.to_string())
    }

    fn remove_subscription(&mut self, thread_id: &str) {
        self.subscriptions.remove(thread_id);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct NativeProtocolResponse {
    #[serde(rename = "type")]
    r#type: String,
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PedelecError>,
}

fn native_message_to_core_request(
    message: Value,
) -> Result<CoreIpcRequest, NativeProtocolResponse> {
    let mut object = match message {
        Value::Object(object) => object,
        _ => {
            return Err(native_error_response(
                "",
                PedelecError::new(
                    error_codes::IPC_UNAVAILABLE,
                    "native message must be an object",
                ),
            ))
        }
    };

    let request_id = take_string_field(&mut object, "requestId").unwrap_or_default();
    if request_id.trim().is_empty() {
        return Err(native_error_response(
            "",
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "requestId is required"),
        ));
    }

    let request_type = take_string_field(&mut object, "type").unwrap_or_default();
    if request_type.trim().is_empty() {
        return Err(native_error_response(
            &request_id,
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "type is required"),
        ));
    }

    let payload = match request_type.as_str() {
        "create_thread"
        | "send_text"
        | "prepare_thread"
        | "end_thread"
        | "subscribe_thread"
        | "create_asset_upload"
        | "list_assets" => Some(Value::Object(object)),
        "list_providers" | "get_settings" | "update_settings" => Some(Value::Object(object)),
        "submit_tool_result" => {
            if let Some(value) = object.remove("toolRequestId") {
                object.insert("requestId".to_string(), value);
            }
            Some(Value::Object(object))
        }
        _ => {
            return Err(native_error_response(
                &request_id,
                PedelecError::with_details(
                    error_codes::IPC_UNAVAILABLE,
                    "unknown native request type",
                    serde_json::json!({ "type": request_type }),
                ),
            ))
        }
    };

    Ok(CoreIpcRequest {
        request_id,
        r#type: request_type,
        payload,
    })
}

fn take_string_field(object: &mut Map<String, Value>, field: &str) -> Option<String> {
    object
        .remove(field)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn subscription_thread_id(request: &CoreIpcRequest) -> Option<String> {
    request
        .payload
        .as_ref()
        .and_then(|payload| payload.get("threadId"))
        .and_then(Value::as_str)
        .filter(|thread_id| !thread_id.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn send_core_request(
    request: &CoreIpcRequest,
    runtime_file_path: Option<&Path>,
) -> CoreIpcResponse {
    match runtime_file_path {
        Some(path) => send_core_ipc_request_with_runtime_path(request, path),
        None => send_core_ipc_request(request),
    }
    .unwrap_or_else(|err| CoreIpcResponse {
        request_id: request.request_id.clone(),
        ok: false,
        result: None,
        error: Some(err),
    })
}

fn core_response_to_native_response(response: CoreIpcResponse) -> NativeProtocolResponse {
    NativeProtocolResponse {
        r#type: "response".to_string(),
        request_id: response.request_id,
        ok: response.ok,
        result: response.result,
        error: response.error,
    }
}

fn native_ok_response(request_id: &str, result: Value) -> NativeProtocolResponse {
    NativeProtocolResponse {
        r#type: "response".to_string(),
        request_id: request_id.to_string(),
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn native_error_response(request_id: &str, error: PedelecError) -> NativeProtocolResponse {
    NativeProtocolResponse {
        r#type: "response".to_string(),
        request_id: request_id.to_string(),
        ok: false,
        result: None,
        error: Some(error),
    }
}

fn start_forward_subscription(
    request: CoreIpcRequest,
    stdout: Arc<Mutex<io::Stdout>>,
    runtime_file_path: Option<&Path>,
) -> Result<bool, PedelecError> {
    let mut stream = match runtime_file_path {
        Some(path) => connect_core_ipc_with_runtime_path(path),
        None => connect_core_ipc(),
    }?;

    write_json_line(&mut stream, &request).map_err(core_subscription_error)?;

    let mut reader = BufReader::new(stream);
    let response_line = read_bounded_json_line(&mut reader).map_err(core_subscription_error)?;
    let response: CoreIpcResponse = serde_json::from_slice(&response_line).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC subscribe response was not valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let ok = response.ok;
    {
        let mut stdout = stdout.lock().unwrap();
        write_chrome_message(&mut *stdout, &core_response_to_native_response(response))
            .map_err(core_subscription_error)?;
    }

    if !ok {
        return Ok(false);
    }

    thread::spawn(move || loop {
        let line = match read_bounded_json_line(&mut reader) {
            Ok(line) => line,
            Err(err) => {
                let response = native_error_response(
                    "",
                    PedelecError::with_details(
                        error_codes::NATIVE_CONNECTION_CLOSED,
                        "Core IPC subscription closed",
                        serde_json::json!({ "error": err.to_string() }),
                    ),
                );
                if let Ok(mut stdout) = stdout.lock() {
                    let _ = write_chrome_message(&mut *stdout, &response);
                }
                break;
            }
        };

        let message: Value = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(err) => {
                let response = native_error_response(
                    "",
                    PedelecError::with_details(
                        error_codes::IPC_UNAVAILABLE,
                        "Core IPC event was not valid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    ),
                );
                if let Ok(mut stdout) = stdout.lock() {
                    let _ = write_chrome_message(&mut *stdout, &response);
                }
                continue;
            }
        };

        if let Ok(mut stdout) = stdout.lock() {
            if write_chrome_message(&mut *stdout, &message).is_err() {
                break;
            }
        } else {
            break;
        }
    });

    Ok(true)
}

fn core_subscription_error(err: io::Error) -> PedelecError {
    PedelecError::with_details(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        "pedelec-app is not running",
        serde_json::json!({ "error": err.to_string() }),
    )
}

fn read_chrome_message<R: Read>(reader: &mut R) -> io::Result<Option<Result<Value, PedelecError>>> {
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_CORE_IPC_MESSAGE_BYTES {
        return Ok(Some(Err(PedelecError::new(
            error_codes::MESSAGE_TOO_LARGE,
            "native message exceeds size limit",
        ))));
    }

    let mut msg_buf = vec![0_u8; len];
    reader.read_exact(&mut msg_buf)?;
    Ok(Some(serde_json::from_slice(&msg_buf).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "native message was not valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })))
}

fn write_chrome_message<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pedelec_core::{error_codes, CoreRuntime, ProviderCode, SharedCoreRuntime};
    use crate::pedelec_ipc::start_core_ipc_server_with_runtime_path;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn native_create_thread_converts_to_core_payload() {
        let request = native_message_to_core_request(json!({
            "type": "create_thread",
            "requestId": "req_create",
            "provider": "codex",
            "model": "gpt-5",
            "skills": {
                "guidance": "Use tools.",
                "tools": []
            }
        }))
        .unwrap();

        assert_eq!(request.request_id, "req_create");
        assert_eq!(request.r#type, "create_thread");
        assert_eq!(
            request.payload.unwrap(),
            json!({
                "provider": "codex",
                "model": "gpt-5",
                "skills": {
                    "guidance": "Use tools.",
                    "tools": []
                }
            })
        );
    }

    #[test]
    fn native_list_assets_preserves_request_id_and_payload() {
        let request = native_message_to_core_request(json!({
            "type": "list_assets",
            "requestId": "req_assets",
            "threadId": "thread_assets"
        }))
        .unwrap();
        assert_eq!(request.request_id, "req_assets");
        assert_eq!(request.r#type, "list_assets");
        assert_eq!(
            request.payload,
            Some(json!({ "threadId": "thread_assets" }))
        );
    }

    #[test]
    fn native_send_end_and_submit_convert_to_core_payloads() {
        let send = native_message_to_core_request(json!({
            "type": "send_text",
            "requestId": "req_send",
            "threadId": "thread_1",
            "message": "hello"
        }))
        .unwrap();
        assert_eq!(send.r#type, "send_text");
        assert_eq!(
            send.payload.unwrap(),
            json!({ "threadId": "thread_1", "message": "hello" })
        );

        let end = native_message_to_core_request(json!({
            "type": "end_thread",
            "requestId": "req_end",
            "threadId": "thread_1"
        }))
        .unwrap();
        assert_eq!(end.r#type, "end_thread");
        assert_eq!(end.payload.unwrap(), json!({ "threadId": "thread_1" }));

        let submit = native_message_to_core_request(json!({
            "type": "submit_tool_result",
            "requestId": "req_submit",
            "threadId": "thread_1",
            "toolRequestId": "tool_1",
            "result": { "ok": true }
        }))
        .unwrap();
        assert_eq!(submit.r#type, "submit_tool_result");
        assert_eq!(
            submit.payload.unwrap(),
            json!({
                "threadId": "thread_1",
                "requestId": "tool_1",
                "result": { "ok": true }
            })
        );
    }

    #[test]
    fn native_list_providers_converts_to_core_payload() {
        let request = native_message_to_core_request(json!({
            "type": "list_providers",
            "requestId": "req_providers"
        }))
        .unwrap();

        assert_eq!(request.request_id, "req_providers");
        assert_eq!(request.r#type, "list_providers");
        assert_eq!(request.payload.unwrap(), json!({}));
    }

    #[test]
    fn native_settings_requests_convert_to_core_payloads() {
        let get = native_message_to_core_request(json!({
            "type": "get_settings",
            "requestId": "req_get_settings"
        }))
        .unwrap();
        assert_eq!(get.r#type, "get_settings");
        assert_eq!(get.payload.unwrap(), json!({}));

        let update = native_message_to_core_request(json!({
            "type": "update_settings",
            "requestId": "req_update_settings",
            "defaultProvider": "codex",
            "defaultModels": {
                "codex": "gpt-5"
            },
            "providerSettings": {
                "ollama": {
                    "baseUrl": "http://127.0.0.1:11434",
                    "timeoutMs": 120000
                }
            }
        }))
        .unwrap();
        assert_eq!(update.r#type, "update_settings");
        assert_eq!(
            update.payload.unwrap(),
            json!({
                "defaultProvider": "codex",
                "defaultModels": { "codex": "gpt-5" },
                "providerSettings": {
                    "ollama": {
                        "baseUrl": "http://127.0.0.1:11434",
                        "timeoutMs": 120000
                    }
                }
            })
        );
    }

    #[test]
    fn phase09_native_protocol_converts_full_mock_lifecycle_without_http_transport() {
        let create = native_message_to_core_request(json!({
            "type": "create_thread",
            "requestId": "phase09_create",
            "provider": "codex",
            "skills": {
                "guidance": "Use tools.",
                "tools": []
            }
        }))
        .unwrap();
        assert_eq!(create.r#type, "create_thread");
        assert_eq!(
            create.payload.unwrap(),
            json!({
                "provider": "codex",
                "skills": {
                    "guidance": "Use tools.",
                    "tools": []
                }
            })
        );

        let subscribe = native_message_to_core_request(json!({
            "type": "subscribe_thread",
            "requestId": "phase09_subscribe",
            "threadId": "thread_phase09"
        }))
        .unwrap();
        assert_eq!(subscribe.r#type, "subscribe_thread");
        assert_eq!(
            subscribe.payload.unwrap(),
            json!({ "threadId": "thread_phase09" })
        );

        let send = native_message_to_core_request(json!({
            "type": "send_text",
            "requestId": "phase09_send",
            "threadId": "thread_phase09",
            "message": "call update_counter"
        }))
        .unwrap();
        assert_eq!(send.r#type, "send_text");
        assert_eq!(
            send.payload.unwrap(),
            json!({
                "threadId": "thread_phase09",
                "message": "call update_counter"
            })
        );

        let submit = native_message_to_core_request(json!({
            "type": "submit_tool_result",
            "requestId": "phase09_submit",
            "threadId": "thread_phase09",
            "toolRequestId": "tool_phase09",
            "result": {
                "counter": 2,
                "toolName": "update_counter"
            }
        }))
        .unwrap();
        assert_eq!(submit.r#type, "submit_tool_result");
        assert_eq!(
            submit.payload.unwrap(),
            json!({
                "threadId": "thread_phase09",
                "requestId": "tool_phase09",
                "result": {
                    "counter": 2,
                    "toolName": "update_counter"
                }
            })
        );

        let end = native_message_to_core_request(json!({
            "type": "end_thread",
            "requestId": "phase09_end",
            "threadId": "thread_phase09"
        }))
        .unwrap();
        assert_eq!(end.r#type, "end_thread");
        assert_eq!(
            end.payload.unwrap(),
            json!({ "threadId": "thread_phase09" })
        );
    }

    #[test]
    fn core_success_and_error_responses_are_wrapped_for_native_protocol() {
        let success = core_response_to_native_response(CoreIpcResponse {
            request_id: "req_1".into(),
            ok: true,
            result: Some(json!({ "threadId": "thread_1" })),
            error: None,
        });
        assert_eq!(
            serde_json::to_value(success).unwrap(),
            json!({
                "type": "response",
                "requestId": "req_1",
                "ok": true,
                "result": { "threadId": "thread_1" }
            })
        );

        let error = core_response_to_native_response(CoreIpcResponse {
            request_id: "req_2".into(),
            ok: false,
            result: None,
            error: Some(PedelecError::new(error_codes::THREAD_NOT_FOUND, "missing")),
        });
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({
                "type": "response",
                "requestId": "req_2",
                "ok": false,
                "error": { "code": "THREAD_NOT_FOUND", "message": "missing" }
            })
        );
    }

    #[test]
    fn native_request_requires_request_id_and_rejects_unknown_type() {
        let missing_request_id = native_message_to_core_request(json!({
            "type": "send_text",
            "threadId": "thread_1",
            "message": "hello"
        }))
        .unwrap_err();
        assert_eq!(missing_request_id.r#type, "response");
        assert_eq!(missing_request_id.request_id, "");
        assert_eq!(
            missing_request_id.error.unwrap().code,
            error_codes::IPC_UNAVAILABLE
        );

        let unknown = native_message_to_core_request(json!({
            "type": "unknown",
            "requestId": "req_unknown"
        }))
        .unwrap_err();
        assert_eq!(unknown.request_id, "req_unknown");
        assert_eq!(unknown.error.unwrap().code, error_codes::IPC_UNAVAILABLE);
    }

    #[test]
    fn oversized_native_message_returns_message_too_large() {
        let mut input = Vec::new();
        input.extend_from_slice(&((MAX_CORE_IPC_MESSAGE_BYTES as u32) + 1).to_le_bytes());

        let err = read_chrome_message(&mut input.as_slice())
            .unwrap()
            .unwrap()
            .unwrap_err();

        assert_eq!(err.code, error_codes::MESSAGE_TOO_LARGE);
    }

    #[test]
    fn missing_runtime_file_maps_to_core_runtime_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let missing_runtime_path = temp.path().join("missing-runtime.json");
        let request = CoreIpcRequest {
            request_id: "req_missing_runtime".into(),
            r#type: "send_text".into(),
            payload: Some(json!({
                "threadId": "thread_1",
                "message": "hello"
            })),
        };

        let response = send_core_request(&request, Some(&missing_runtime_path));

        assert!(!response.ok);
        assert_eq!(response.request_id, "req_missing_runtime");
        assert_eq!(
            response.error.unwrap().code,
            error_codes::CORE_RUNTIME_UNAVAILABLE
        );
    }

    #[test]
    fn duplicate_subscribe_thread_is_idempotent_in_connection_state() {
        let mut state = NativeConnectionState::default();

        assert!(state.mark_subscription("thread_1"));
        assert!(!state.mark_subscription("thread_1"));
        assert!(state.mark_subscription("thread_2"));
    }

    #[test]
    fn subscribe_thread_bridges_core_success_response() {
        let temp = tempfile::tempdir().unwrap();
        let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_idle_thread(&runtime, "thread_subscribe");

        let core_request = native_message_to_core_request(json!({
            "type": "subscribe_thread",
            "requestId": "req_subscribe",
            "threadId": "thread_subscribe"
        }))
        .unwrap();
        let response = send_core_request(&core_request, Some(&runtime_path));
        let native = core_response_to_native_response(response);

        assert!(native.ok);
        assert_eq!(native.r#type, "response");
        assert_eq!(native.request_id, "req_subscribe");
        assert_eq!(native.result.unwrap(), json!({ "subscribed": true }));
    }

    fn insert_idle_thread(runtime: &SharedCoreRuntime, thread_id: &str) {
        use crate::pedelec_core::{ProviderAdapterState, ThreadState, ThreadStatus};
        use std::path::PathBuf;

        let now = chrono::Utc::now();
        runtime.lock().unwrap().thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                model: None,
                sandbox_path: PathBuf::from("sandbox").join(thread_id),
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
            },
            ProviderAdapterState {
                provider_session_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
    }
}
