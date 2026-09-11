use pedelec_core::{error_codes, PedelecError};
use pedelec_ipc::{
    connect_core_ipc, connect_core_ipc_with_runtime_path, default_runtime_file_path,
    read_bounded_json_line, send_core_ipc_request, send_core_ipc_request_with_runtime_path,
    write_json_line, CoreIpcRequest, CoreIpcResponse, MAX_CORE_IPC_MESSAGE_BYTES,
};
use pedelec_shared::paths::{app_launch_config_path, read_app_launch_config};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CORE_READY_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const CORE_READY_MAX_WAIT: Duration = Duration::from_secs(10);

pub fn run() -> io::Result<()> {
    run_chrome_native_host(None)
}

fn run_chrome_native_host(runtime_file_path: Option<PathBuf>) -> io::Result<()> {
    let mut stdin = io::stdin();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let connection = Arc::new(Mutex::new(NativeConnectionState::default()));

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

            let marked = connection
                .lock()
                .unwrap()
                .mark_subscription(&thread_id, request.caller_origin.as_deref());
            if !marked {
                let native_response =
                    duplicate_subscription_response(&request, runtime_file_path.as_deref());
                let mut stdout = stdout.lock().unwrap();
                write_chrome_message(&mut *stdout, &native_response)?;
                continue;
            }

            let result = start_forward_subscription(
                request.clone(),
                Arc::clone(&stdout),
                Arc::clone(&connection),
                runtime_file_path.as_deref(),
            );
            match result {
                Ok(true) => {}
                Ok(false) => connection
                    .lock()
                    .unwrap()
                    .remove_subscription(&thread_id, request.caller_origin.as_deref()),
                Err(err) => {
                    connection
                        .lock()
                        .unwrap()
                        .remove_subscription(&thread_id, request.caller_origin.as_deref());
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

fn duplicate_subscription_response(
    request: &CoreIpcRequest,
    runtime_file_path: Option<&Path>,
) -> NativeProtocolResponse {
    let snapshot_request = CoreIpcRequest {
        r#type: "thread_snapshot".to_string(),
        ..request.clone()
    };
    let snapshot_response = send_core_request(&snapshot_request, runtime_file_path);
    if !snapshot_response.ok {
        return core_response_to_native_response(snapshot_response);
    }

    match snapshot_response.result {
        Some(snapshot) => native_ok_response(
            &request.request_id,
            serde_json::json!({
                "subscribed": true,
                "duplicate": true,
                "snapshot": snapshot,
            }),
        ),
        None => native_error_response(
            &request.request_id,
            PedelecError::new(
                error_codes::IPC_UNAVAILABLE,
                "thread_snapshot response did not include a snapshot",
            ),
        ),
    }
}

#[derive(Debug, Default)]
struct NativeConnectionState {
    subscriptions: HashSet<NativeSubscriptionKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NativeSubscriptionKey {
    thread_id: String,
    caller_origin: Option<String>,
}

impl NativeConnectionState {
    fn mark_subscription(&mut self, thread_id: &str, caller_origin: Option<&str>) -> bool {
        self.subscriptions.insert(NativeSubscriptionKey {
            thread_id: thread_id.to_string(),
            caller_origin: caller_origin.map(ToOwned::to_owned),
        })
    }

    fn remove_subscription(&mut self, thread_id: &str, caller_origin: Option<&str>) {
        self.subscriptions.remove(&NativeSubscriptionKey {
            thread_id: thread_id.to_string(),
            caller_origin: caller_origin.map(ToOwned::to_owned),
        });
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

    let caller_origin = take_string_field(&mut object, "callerOrigin");
    let caller_sdk_version = take_string_field(&mut object, "callerSdkVersion");
    let payload = match request_type.as_str() {
        "create_thread"
        | "send_text"
        | "prepare_thread"
        | "end_thread"
        | "resume_thread"
        | "subscribe_thread"
        | "thread_snapshot"
        | "create_asset_upload"
        | "create_asset_download"
        | "list_assets" => Some(Value::Object(object)),
        "list_providers" | "get_settings" => Some(Value::Object(object)),
        // The directory picker deliberately has no caller-controlled payload.
        "pick_workspace_folder" => Some(serde_json::json!({})),
        // The connectivity probe deliberately has no caller-controlled payload.
        "ping" => Some(serde_json::json!({})),
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
        caller_origin,
        caller_sdk_version,
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
    send_core_request_with_launcher(request, runtime_file_path, ensure_core_runtime_available)
}

fn send_core_request_with_launcher<F>(
    request: &CoreIpcRequest,
    runtime_file_path: Option<&Path>,
    launch_core: F,
) -> CoreIpcResponse
where
    F: FnOnce(Option<&Path>) -> Result<(), PedelecError>,
{
    let first_attempt = match runtime_file_path {
        Some(path) => send_core_ipc_request_with_runtime_path(request, path),
        None => send_core_ipc_request(request),
    };
    match first_attempt {
        Ok(response) => response,
        Err(err) if err.code == error_codes::CORE_RUNTIME_UNAVAILABLE => {
            match launch_core(runtime_file_path) {
                Ok(()) => match runtime_file_path {
                    Some(path) => send_core_ipc_request_with_runtime_path(request, path),
                    None => send_core_ipc_request(request),
                }
                .unwrap_or_else(|err| core_error_response(request, err)),
                Err(err) => core_error_response(request, err),
            }
        }
        Err(err) => core_error_response(request, err),
    }
}

fn core_error_response(request: &CoreIpcRequest, err: PedelecError) -> CoreIpcResponse {
    CoreIpcResponse {
        request_id: request.request_id.clone(),
        ok: false,
        result: None,
        error: Some(err),
    }
}

fn ensure_core_runtime_available(runtime_file_path: Option<&Path>) -> Result<(), PedelecError> {
    let runtime_file_path = match runtime_file_path {
        Some(path) => path.to_path_buf(),
        None => default_runtime_file_path()?,
    };
    if connect_core_ipc_with_runtime_path(&runtime_file_path).is_ok() {
        return Ok(());
    }

    let launch_config_path = app_launch_config_path().map_err(shared_error_to_core)?;
    let launch_config = read_app_launch_config(&launch_config_path)
        .map_err(|mut err| {
            if let Some(Value::Object(details)) = err.details.as_mut() {
                details.insert(
                    "launchConfigPath".to_string(),
                    Value::String(launch_config_path.to_string_lossy().to_string()),
                );
            }
            err
        })
        .map_err(shared_error_to_core)?;
    let mut command = Command::new(&launch_config.executable_path);
    command
        .args(&launch_config.background_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.spawn().map_err(|err| {
        PedelecError::with_details(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
            serde_json::json!({
                "stage": "spawn_desktop_app",
                "launchConfigPath": launch_config_path,
                "executablePath": launch_config.executable_path,
                "reason": err.to_string(),
            }),
        )
    })?;

    let started_at = Instant::now();
    loop {
        thread::sleep(CORE_READY_RETRY_INTERVAL);
        if connect_core_ipc_with_runtime_path(&runtime_file_path).is_ok() {
            return Ok(());
        }
        if started_at.elapsed() >= CORE_READY_MAX_WAIT {
            return Err(PedelecError::with_details(
                error_codes::CORE_RUNTIME_UNAVAILABLE,
                "pedelec-app is not running",
                serde_json::json!({
                    "stage": "wait_for_core_runtime",
                    "timeoutMs": CORE_READY_MAX_WAIT.as_millis(),
                    "retryIntervalMs": CORE_READY_RETRY_INTERVAL.as_millis(),
                }),
            ));
        }
    }
}

fn shared_error_to_core(err: pedelec_shared::error::PedelecError) -> PedelecError {
    PedelecError {
        code: err.code,
        message: err.message,
        details: err.details,
    }
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
    connection: Arc<Mutex<NativeConnectionState>>,
    runtime_file_path: Option<&Path>,
) -> Result<bool, PedelecError> {
    let mut stream = match runtime_file_path {
        Some(path) => connect_core_ipc_with_runtime_path(path),
        None => connect_core_ipc(),
    }
    .or_else(|err| {
        if err.code != error_codes::CORE_RUNTIME_UNAVAILABLE {
            return Err(err);
        }
        ensure_core_runtime_available(runtime_file_path)?;
        match runtime_file_path {
            Some(path) => connect_core_ipc_with_runtime_path(path),
            None => connect_core_ipc(),
        }
    })?;

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

    let thread_id = subscription_thread_id(&request).unwrap_or_default();
    let caller_origin = request.caller_origin.clone();
    thread::spawn(move || loop {
        let line = match read_bounded_json_line(&mut reader) {
            Ok(line) => line,
            Err(err) => {
                connection
                    .lock()
                    .unwrap()
                    .remove_subscription(&thread_id, caller_origin.as_deref());
                let response = subscription_closed_notification(
                    &thread_id,
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
                let response = subscription_closed_notification(
                    &thread_id,
                    PedelecError::with_details(
                        error_codes::IPC_UNAVAILABLE,
                        "Core IPC event was not valid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    ),
                );
                connection
                    .lock()
                    .unwrap()
                    .remove_subscription(&thread_id, caller_origin.as_deref());
                if let Ok(mut stdout) = stdout.lock() {
                    let _ = write_chrome_message(&mut *stdout, &response);
                }
                break;
            }
        };

        if let Ok(mut stdout) = stdout.lock() {
            if write_chrome_message(&mut *stdout, &message).is_err() {
                connection
                    .lock()
                    .unwrap()
                    .remove_subscription(&thread_id, caller_origin.as_deref());
                break;
            }
        } else {
            connection
                .lock()
                .unwrap()
                .remove_subscription(&thread_id, caller_origin.as_deref());
            break;
        }
    });

    Ok(true)
}

fn subscription_closed_notification(thread_id: &str, error: PedelecError) -> Value {
    serde_json::json!({
        "type": "thread_subscription_closed",
        "threadId": thread_id,
        "error": error,
    })
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
    use pedelec_core::{
        error_codes, refresh_shared_providers, CoreRuntime, ProviderCode, SharedCoreRuntime,
    };
    use pedelec_ipc::start_core_ipc_server_with_runtime_path;
    use serde_json::json;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

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

        let resume = native_message_to_core_request(json!({
            "type": "resume_thread",
            "requestId": "req_resume",
            "threadId": "thread_1"
        }))
        .unwrap();
        assert_eq!(resume.r#type, "resume_thread");
        assert_eq!(resume.payload.unwrap(), json!({ "threadId": "thread_1" }));

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
    fn native_settings_and_ping_requests_convert_to_core_payloads() {
        let get = native_message_to_core_request(json!({
            "type": "get_settings",
            "requestId": "req_get_settings"
        }))
        .unwrap();
        assert_eq!(get.r#type, "get_settings");
        assert_eq!(get.payload.unwrap(), json!({}));

        let ping = native_message_to_core_request(json!({
            "type": "ping",
            "requestId": "req_ping"
        }))
        .unwrap();
        assert_eq!(ping.r#type, "ping");
        assert_eq!(ping.payload.unwrap(), json!({}));

        assert!(native_message_to_core_request(json!({
            "type": "update_settings",
            "requestId": "req_update_settings"
        }))
        .is_err());
    }

    #[test]
    fn native_workspace_folder_picker_preserves_metadata_without_session_payload() {
        let request = native_message_to_core_request(json!({
            "type": "pick_workspace_folder",
            "requestId": "req_picker",
            "callerOrigin": "https://approved.example",
            "callerSdkVersion": "mock-sdk-version",
            "path": "C:\\user-controlled-path",
            "isEmptyFolder": true
        }))
        .unwrap();

        assert_eq!(request.r#type, "pick_workspace_folder");
        assert_eq!(
            request.caller_origin.as_deref(),
            Some("https://approved.example")
        );
        assert_eq!(
            request.caller_sdk_version.as_deref(),
            Some("mock-sdk-version")
        );
        assert_eq!(request.payload, Some(json!({})));
    }

    #[test]
    fn native_directory_picker_request_is_not_supported() {
        assert!(native_message_to_core_request(json!({
            "type": "pick_sandbox_folder",
            "requestId": "req_old_picker"
        }))
        .is_err());
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
            caller_origin: None,
            caller_sdk_version: None,
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

        assert!(state.mark_subscription("thread_1", Some("https://app.example.com")));
        assert!(!state.mark_subscription("thread_1", Some("https://app.example.com")));
        assert!(state.mark_subscription("thread_1", Some("https://other.example.com")));
        assert!(state.mark_subscription("thread_2", None));
        assert_eq!(state.subscriptions.len(), 3);
    }

    #[test]
    fn subscription_closed_notification_contains_the_thread_id() {
        let notification = subscription_closed_notification(
            "thread_closed",
            PedelecError::new(error_codes::NATIVE_CONNECTION_CLOSED, "closed"),
        );

        assert_eq!(notification["type"], json!("thread_subscription_closed"));
        assert_eq!(notification["threadId"], json!("thread_closed"));
        assert_eq!(
            notification["error"]["code"],
            json!(error_codes::NATIVE_CONNECTION_CLOSED)
        );
    }

    #[test]
    fn closed_subscription_can_be_readded_without_removing_unrelated_members() {
        let mut state = NativeConnectionState::default();
        assert!(state.mark_subscription("thread_a", Some("https://app.example.com")));
        assert!(state.mark_subscription("thread_b", Some("https://app.example.com")));

        state.remove_subscription("thread_a", Some("https://app.example.com"));
        assert!(!state.subscriptions.contains(&NativeSubscriptionKey {
            thread_id: "thread_a".into(),
            caller_origin: Some("https://app.example.com".into()),
        }));
        assert!(state.subscriptions.contains(&NativeSubscriptionKey {
            thread_id: "thread_b".into(),
            caller_origin: Some("https://app.example.com".into()),
        }));
        assert!(state.mark_subscription("thread_a", Some("https://app.example.com")));
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
        let result = native.result.unwrap();
        assert_eq!(result["subscribed"], json!(true));
        assert_eq!(result["snapshot"]["threadId"], json!("thread_subscribe"));
        assert_eq!(result["snapshot"]["status"], json!("idle"));
        assert!(result["snapshot"]["latestSeq"].is_number());
    }

    #[test]
    fn duplicate_subscribe_returns_fresh_snapshot_and_propagates_snapshot_errors() {
        let temp = tempfile::tempdir().unwrap();
        let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_idle_thread(&runtime, "thread_duplicate_snapshot");

        let request = CoreIpcRequest {
            request_id: "req_duplicate_snapshot".into(),
            r#type: "subscribe_thread".into(),
            caller_origin: None,
            caller_sdk_version: None,
            payload: Some(json!({ "threadId": "thread_duplicate_snapshot" })),
        };
        let first = send_core_request(&request, Some(&runtime_path));
        assert!(first.ok);
        assert_eq!(
            first.result.as_ref().unwrap()["snapshot"]["status"],
            json!("idle")
        );

        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .thread_manager
                .thread_mut("thread_duplicate_snapshot")
                .unwrap()
                .status = pedelec_core::ThreadStatus::Ended;
            runtime.event_bus.emit_status_changed(
                "thread_duplicate_snapshot",
                pedelec_core::ThreadStatus::Ended,
            );
        }

        let mut connection = NativeConnectionState::default();
        assert!(connection.mark_subscription("thread_duplicate_snapshot", None));
        let duplicate = duplicate_subscription_response(&request, Some(&runtime_path));
        assert!(duplicate.ok);
        assert_eq!(duplicate.request_id, "req_duplicate_snapshot");
        assert_eq!(
            duplicate.result.as_ref().unwrap()["subscribed"],
            json!(true)
        );
        assert_eq!(duplicate.result.as_ref().unwrap()["duplicate"], json!(true));
        assert_eq!(
            duplicate.result.as_ref().unwrap()["snapshot"]["status"],
            json!("ended")
        );
        assert!(
            duplicate.result.as_ref().unwrap()["snapshot"]["latestSeq"]
                .as_u64()
                .unwrap()
                > first.result.as_ref().unwrap()["snapshot"]["latestSeq"]
                    .as_u64()
                    .unwrap()
        );

        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .thread_manager
                .thread_mut("thread_duplicate_snapshot")
                .unwrap()
                .status = pedelec_core::ThreadStatus::Idle;
            runtime.event_bus.emit_status_changed(
                "thread_duplicate_snapshot",
                pedelec_core::ThreadStatus::Idle,
            );
        }

        let resumed_duplicate = duplicate_subscription_response(
            &CoreIpcRequest {
                request_id: "req_duplicate_after_resume".into(),
                ..request.clone()
            },
            Some(&runtime_path),
        );
        assert!(resumed_duplicate.ok);
        assert_eq!(
            resumed_duplicate.result.as_ref().unwrap()["snapshot"]["status"],
            json!("idle")
        );
        assert!(
            !connection.mark_subscription("thread_duplicate_snapshot", None),
            "the ended thread's existing forwarding subscription must remain reusable"
        );

        let wrong_origin_request = CoreIpcRequest {
            request_id: "req_duplicate_wrong_origin".into(),
            caller_origin: Some("https://other.example.test".into()),
            ..request.clone()
        };
        assert!(connection.mark_subscription(
            "thread_duplicate_snapshot",
            wrong_origin_request.caller_origin.as_deref()
        ));

        let missing_request = CoreIpcRequest {
            request_id: "req_duplicate_missing".into(),
            payload: Some(json!({ "threadId": "thread_does_not_exist" })),
            ..request
        };
        assert!(connection.mark_subscription("thread_does_not_exist", None));
        let missing = duplicate_subscription_response(&missing_request, Some(&runtime_path));
        assert!(!missing.ok);
        assert_eq!(missing.request_id, "req_duplicate_missing");
        assert_eq!(missing.error.unwrap().code, error_codes::THREAD_NOT_FOUND);
    }

    #[test]
    fn native_auto_launch_keeps_list_providers_pending_until_core_readiness() {
        let temp = tempfile::tempdir().unwrap();
        let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            ..CoreRuntime::new()
        }));
        runtime.lock().unwrap().provider_path_value_override = Some(std::ffi::OsString::new());
        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_initial_scanning_for_test();
        let runtime_path = temp.path().join("runtime.json");
        let request_runtime_path = runtime_path.clone();
        let request = CoreIpcRequest {
            request_id: "req_native_providers".into(),
            r#type: "list_providers".into(),
            caller_origin: None,
            caller_sdk_version: None,
            payload: Some(json!({})),
        };
        let (core_connectable_tx, core_connectable_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let launch_runtime = Arc::clone(&runtime);
        let launch_runtime_path = runtime_path.clone();
        let request_thread = thread::spawn(move || {
            let response =
                send_core_request_with_launcher(&request, Some(&request_runtime_path), move |_| {
                    start_core_ipc_server_with_runtime_path(launch_runtime, launch_runtime_path)
                        .map(|_| ())
                        .map_err(|error| error)
                        .map(|()| {
                            core_connectable_tx.send(()).unwrap();
                        })
                });
            response_tx.send(response).unwrap();
        });

        core_connectable_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("auto-launch did not make Core connectable");
        assert!(response_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());

        refresh_shared_providers(&runtime);

        let response = response_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("list_providers did not resume after readiness");
        request_thread.join().unwrap();
        assert!(response.ok);
        let providers = response.result.unwrap();
        assert!(providers.as_array().is_some_and(|providers| {
            providers.iter().any(|provider| provider["code"] == "codex")
        }));
    }

    fn insert_idle_thread(runtime: &SharedCoreRuntime, thread_id: &str) {
        use pedelec_core::{EffortLevel, ProviderSessionState, ThreadState, ThreadStatus};
        use std::path::PathBuf;

        let now = chrono::Utc::now();
        runtime.lock().unwrap().thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: vec![],
                workspace_path: PathBuf::from("workspace").join(thread_id),
                skills: vec![],
                status: ThreadStatus::Idle,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
    }
}
