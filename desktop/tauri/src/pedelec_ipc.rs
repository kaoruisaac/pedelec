use crate::pedelec_core::{
    error_codes, CreateAssetUploadInput, CreateThreadInput, EndThreadInput, ListAssetsInput,
    PedelecError, PrepareThreadInput, PrepareThreadOutput, RunningProviderProcessPurpose,
    SendTextInput, SharedCoreRuntime, SubmitToolResultInput, SubscribeThreadInput, ThreadEvent,
    ToolCallInput, ToolSpecInput, UpdateSettingsInput,
};
use encoding_rs::Encoding;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const CORE_IPC_PROTOCOL: &str = "pedelec-core-ipc-v1";
pub const CORE_IPC_HOST: &str = "127.0.0.1";
pub const MAX_CORE_IPC_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeFile {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub endpoint: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcRequest {
    pub request_id: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcResponse {
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoreIpcEventMessage {
    pub r#type: String,
    pub event: ThreadEvent,
}

pub struct CoreIpcServerHandle {
    pub runtime_file: RuntimeFile,
    pub runtime_file_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCoreIpcRequest {
    request_id: Option<String>,
    r#type: Option<String>,
    payload: Option<Value>,
}

pub fn start_core_ipc_server(
    runtime: SharedCoreRuntime,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    start_core_ipc_server_with_runtime_path(runtime, runtime_file_path)
}

pub fn start_core_ipc_server_with_runtime_path(
    runtime: SharedCoreRuntime,
    runtime_file_path: impl Into<PathBuf>,
) -> Result<CoreIpcServerHandle, PedelecError> {
    let listener = TcpListener::bind((CORE_IPC_HOST, 0)).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot bind Core IPC server",
            serde_json::json!({ "host": CORE_IPC_HOST, "error": err.to_string() }),
        )
    })?;
    let local_addr = listener.local_addr().map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot resolve Core IPC server address",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    let runtime_file = RuntimeFile {
        protocol: CORE_IPC_PROTOCOL.to_string(),
        host: CORE_IPC_HOST.to_string(),
        port: local_addr.port(),
        endpoint: local_addr.to_string(),
        pid: std::process::id(),
    };
    let runtime_file_path = runtime_file_path.into();
    write_runtime_file(&runtime_file_path, &runtime_file)?;
    runtime
        .lock()
        .unwrap()
        .set_core_ipc_runtime(runtime_file.endpoint.clone(), runtime_file_path.clone());

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else {
                continue;
            };
            let runtime = Arc::clone(&runtime);
            thread::spawn(move || {
                let _ = handle_core_ipc_connection(stream, runtime);
            });
        }
    });

    Ok(CoreIpcServerHandle {
        runtime_file,
        runtime_file_path,
    })
}

pub fn default_runtime_file_path() -> Result<PathBuf, PedelecError> {
    dirs::home_dir()
        .map(|home| home.join(".pedelec").join("runtime.json"))
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::IPC_UNAVAILABLE,
                "cannot resolve user home directory for runtime.json",
            )
        })
}

pub fn send_core_ipc_request(request: &CoreIpcRequest) -> Result<CoreIpcResponse, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    send_core_ipc_request_with_runtime_path(request, runtime_file_path)
}

pub fn send_core_ipc_request_with_runtime_path(
    request: &CoreIpcRequest,
    runtime_file_path: impl AsRef<Path>,
) -> Result<CoreIpcResponse, PedelecError> {
    let mut stream = connect_core_ipc_with_runtime_path(runtime_file_path)?;
    write_json_line(&mut stream, request).map_err(core_unavailable_error)?;

    let mut reader = BufReader::new(stream);
    let line = read_bounded_json_line(&mut reader).map_err(core_unavailable_error)?;
    let value: Value = serde_json::from_slice(&line).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC response was not valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    serde_json::from_value(value).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC response had invalid shape",
            serde_json::json!({ "error": err.to_string() }),
        )
    })
}

pub fn connect_core_ipc() -> Result<TcpStream, PedelecError> {
    let runtime_file_path = default_runtime_file_path()?;
    connect_core_ipc_with_runtime_path(runtime_file_path)
}

pub fn connect_core_ipc_with_runtime_path(
    runtime_file_path: impl AsRef<Path>,
) -> Result<TcpStream, PedelecError> {
    let runtime_file = read_runtime_file(runtime_file_path)?;
    if runtime_file.protocol != CORE_IPC_PROTOCOL || runtime_file.host != CORE_IPC_HOST {
        return Err(PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        ));
    }

    TcpStream::connect(&runtime_file.endpoint).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })
}

pub fn write_json_line<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    writer.write_all(&payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub fn read_bounded_json_line<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();

    loop {
        let (consumed, found_newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if output.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Core IPC connection closed",
                    ));
                }
                return Ok(output);
            }

            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    output.extend_from_slice(&available[..index]);
                    (index + 1, true)
                }
                None => {
                    output.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        reader.consume(consumed);

        if output.len() > MAX_CORE_IPC_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Core IPC message exceeds size limit",
            ));
        }

        if found_newline {
            break;
        }
    }

    Ok(output)
}

fn handle_core_ipc_connection(stream: TcpStream, runtime: SharedCoreRuntime) -> io::Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let writer = Arc::new(Mutex::new(stream));

    loop {
        let line = match read_bounded_json_line(&mut reader) {
            Ok(line) => line,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                let response = error_response(
                    "",
                    PedelecError::new(
                        error_codes::MESSAGE_TOO_LARGE,
                        "Core IPC message exceeds size limit",
                    ),
                );
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        let value = match serde_json::from_slice::<Value>(&line) {
            Ok(value) => value,
            Err(err) => {
                let response = error_response(
                    "",
                    PedelecError::with_details(
                        error_codes::IPC_UNAVAILABLE,
                        "Core IPC request was not valid JSON",
                        serde_json::json!({ "error": err.to_string() }),
                    ),
                );
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                continue;
            }
        };

        let request = match parse_core_ipc_request(value) {
            Ok(request) => request,
            Err(response) => {
                let mut writer = writer.lock().unwrap();
                write_json_line(&mut *writer, &response)?;
                continue;
            }
        };

        if request.r#type == "subscribe_thread" {
            let response = handle_subscribe_thread(&request, &runtime, Arc::clone(&writer));
            let mut writer = writer.lock().unwrap();
            write_json_line(&mut *writer, &response)?;
            continue;
        }

        let response = handle_core_ipc_request(request, Arc::clone(&runtime));
        let mut writer = writer.lock().unwrap();
        write_json_line(&mut *writer, &response)?;
    }
}

fn parse_core_ipc_request(value: Value) -> Result<CoreIpcRequest, CoreIpcResponse> {
    let raw: RawCoreIpcRequest = serde_json::from_value(value).map_err(|err| {
        error_response(
            "",
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "Core IPC request had invalid shape",
                serde_json::json!({ "error": err.to_string() }),
            ),
        )
    })?;

    let request_id = raw.request_id.unwrap_or_default();
    if request_id.trim().is_empty() {
        return Err(error_response(
            "",
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "requestId is required"),
        ));
    }

    let request_type = raw.r#type.unwrap_or_default();
    if request_type.trim().is_empty() {
        return Err(error_response(
            &request_id,
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "type is required"),
        ));
    }

    Ok(CoreIpcRequest {
        request_id,
        r#type: request_type,
        payload: raw.payload,
    })
}

fn handle_core_ipc_request(request: CoreIpcRequest, runtime: SharedCoreRuntime) -> CoreIpcResponse {
    match request.r#type.as_str() {
        "create_thread" => match decode_payload::<CreateThreadInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().create_thread(input) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "list_providers" => ok_response(
            &request.request_id,
            serde_json::json!(runtime.lock().unwrap().list_providers()),
        ),
        "get_settings" => match runtime.lock().unwrap().get_settings() {
            Ok(settings) => ok_response(&request.request_id, serde_json::json!(settings)),
            Err(err) => error_response(&request.request_id, err),
        },
        "update_settings" => match decode_payload::<UpdateSettingsInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().update_settings(input) {
                Ok(settings) => ok_response(&request.request_id, serde_json::json!(settings)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "send_text" => match decode_payload::<SendTextInput>(&request) {
            Ok(input) => match start_provider_process(runtime, input) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "prepare_thread" => match decode_payload::<PrepareThreadInput>(&request) {
            Ok(input) => match prepare_provider_process(runtime, input) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "create_asset_upload" => match decode_payload::<CreateAssetUploadInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().create_asset_upload(input) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "list_assets" => match decode_payload::<ListAssetsInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().list_assets(input) {
                Ok(output) => ok_response(&request.request_id, serde_json::json!(output)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "end_thread" => match decode_payload::<EndThreadInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().end_thread(input) {
                Ok(()) => ok_response(&request.request_id, serde_json::json!({})),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "submit_tool_result" => match decode_payload::<SubmitToolResultInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().submit_tool_result(input) {
                Ok(()) => ok_response(&request.request_id, serde_json::json!({})),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        "tool_call" => handle_tool_call_request(&request, runtime),
        "tool_spec" => match decode_payload::<ToolSpecInput>(&request) {
            Ok(input) => match runtime.lock().unwrap().tool_spec(input) {
                Ok(spec) => ok_response(&request.request_id, serde_json::json!(spec)),
                Err(err) => error_response(&request.request_id, err),
            },
            Err(err) => error_response(&request.request_id, err),
        },
        _ => error_response(
            &request.request_id,
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "unknown Core IPC request type",
                serde_json::json!({ "type": request.r#type }),
            ),
        ),
    }
}

fn handle_subscribe_thread(
    request: &CoreIpcRequest,
    runtime: &SharedCoreRuntime,
    writer: Arc<Mutex<TcpStream>>,
) -> CoreIpcResponse {
    let input = match decode_payload::<SubscribeThreadInput>(request) {
        Ok(input) => input,
        Err(err) => return error_response(&request.request_id, err),
    };

    let event_rx = match runtime.lock().unwrap().subscribe_thread(input) {
        Ok(event_rx) => event_rx,
        Err(err) => return error_response(&request.request_id, err),
    };

    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            let message = CoreIpcEventMessage {
                r#type: "thread_event".to_string(),
                event,
            };
            let Ok(mut writer) = writer.lock() else {
                break;
            };
            if write_json_line(&mut *writer, &message).is_err() {
                break;
            }
        }
    });

    ok_response(
        &request.request_id,
        serde_json::json!({ "subscribed": true }),
    )
}

fn handle_tool_call_request(
    request: &CoreIpcRequest,
    runtime: SharedCoreRuntime,
) -> CoreIpcResponse {
    let input = match decode_payload::<ToolCallInput>(request) {
        Ok(input) => input,
        Err(err) => return error_response(&request.request_id, err),
    };

    let (request_id, timeout_ms, result_rx) = match runtime.lock().unwrap().begin_tool_call(input) {
        Ok(wait) => wait,
        Err(err) => return error_response(&request.request_id, err),
    };

    match result_rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(result) => ok_response(&request.request_id, result),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            runtime.lock().unwrap().timeout_tool_call(&request_id);
            error_response(
                &request.request_id,
                PedelecError::new(error_codes::TOOL_TIMEOUT, "tool timeout"),
            )
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            runtime.lock().unwrap().timeout_tool_call(&request_id);
            error_response(
                &request.request_id,
                PedelecError::new(error_codes::TOOL_TIMEOUT, "tool result channel closed"),
            )
        }
    }
}

fn decode_payload<T>(request: &CoreIpcRequest) -> Result<T, PedelecError>
where
    T: for<'de> Deserialize<'de>,
{
    let payload = request.payload.clone().unwrap_or(Value::Null);
    serde_json::from_value(payload).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Core IPC request payload had invalid shape",
            serde_json::json!({
                "type": request.r#type,
                "error": err.to_string()
            }),
        )
    })
}

pub fn start_provider_process(
    runtime: SharedCoreRuntime,
    input: SendTextInput,
) -> Result<crate::pedelec_core::SendTextOutput, PedelecError> {
    let thread_id = input.thread_id.clone();
    let start = runtime.lock().unwrap().begin_send_text(input)?;
    start_provider_process_with_command(
        runtime,
        thread_id,
        start.command,
        RunningProviderProcessPurpose::UserMessage,
    )?;
    Ok(start.output)
}

pub fn prepare_provider_process(
    runtime: SharedCoreRuntime,
    input: PrepareThreadInput,
) -> Result<PrepareThreadOutput, PedelecError> {
    let thread_id = input.thread_id.clone();
    let start = runtime.lock().unwrap().begin_prepare_thread(input)?;
    let Some(command) = start.command else {
        return Ok(start.output);
    };
    start_provider_process_with_command(
        runtime,
        thread_id,
        command,
        RunningProviderProcessPurpose::Prepare,
    )?;
    Ok(start.output)
}

fn start_provider_process_with_command(
    runtime: SharedCoreRuntime,
    thread_id: String,
    command_spec: crate::pedelec_core::CommandSpec,
    purpose: RunningProviderProcessPurpose,
) -> Result<(), PedelecError> {
    let resolved_program = match resolve_provider_program(&command_spec.program, &command_spec.env)
    {
        Ok(resolved_program) => resolved_program,
        Err(err) => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROCESS_START_FAILED,
                "provider program could not be found",
                provider_start_error_details(&thread_id, &command_spec, None, Some(err)),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    };

    let mut command = build_provider_process_command(&command_spec, &resolved_program);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let error = PedelecError::with_details(
                error_codes::PROVIDER_PROCESS_START_FAILED,
                "provider process could not be started",
                provider_start_error_details(
                    &thread_id,
                    &command_spec,
                    Some(&resolved_program),
                    Some(ProviderProgramResolveError {
                        candidates: Vec::new(),
                        error: err.to_string(),
                    }),
                ),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    };

    let process_id = child.id();
    runtime
        .lock()
        .unwrap()
        .emit_provider_command_started(&thread_id, process_id, &command_spec);

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(command_spec.stdin.as_bytes()) {
            let _ = child.kill();
            let error = PedelecError::with_details(
                error_codes::PROVIDER_STDIN_CLOSED,
                "provider stdin closed before prompt was written",
                serde_json::json!({
                    "threadId": thread_id,
                    "processId": process_id,
                    "error": err.to_string()
                }),
            );
            runtime
                .lock()
                .unwrap()
                .fail_provider_process_start(&thread_id, error.clone(), purpose);
            return Err(error);
        }
    } else {
        let _ = child.kill();
        let error = PedelecError::with_details(
            error_codes::PROVIDER_STDIN_CLOSED,
            "provider stdin was not available",
            serde_json::json!({
                "threadId": thread_id,
                "processId": process_id
            }),
        );
        runtime
            .lock()
            .unwrap()
            .fail_provider_process_start(&thread_id, error.clone(), purpose);
        return Err(error);
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(Some(child)));
    runtime.lock().unwrap().register_provider_process(
        &thread_id,
        process_id,
        Arc::clone(&child),
        purpose,
    );

    if let Some(stdout) = stdout {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stdout,
            ProviderStream::Stdout,
        );
    }
    if let Some(stderr) = stderr {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stderr,
            ProviderStream::Stderr,
        );
    }
    spawn_provider_waiter(runtime, thread_id, process_id, child);

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedProviderProgram {
    Direct(PathBuf),
    #[cfg(windows)]
    CmdScript(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderProgramResolveError {
    candidates: Vec<PathBuf>,
    error: String,
}

fn resolve_provider_program(
    program: &str,
    command_env: &[(String, String)],
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    let program_path = Path::new(program);
    if has_path_separator(program) {
        return resolve_provider_program_path(program_path);
    }

    let Some(path_value) = command_env_path(command_env).or_else(|| env::var_os("PATH")) else {
        return Err(ProviderProgramResolveError {
            candidates: Vec::new(),
            error: "PATH was not available".to_string(),
        });
    };

    let path_dirs = env::split_paths(&path_value).collect::<Vec<_>>();
    let candidates = provider_program_lookup_candidates(program, &path_dirs);
    for candidate in &candidates {
        if candidate.is_file() {
            return resolved_provider_program_for_existing_path(candidate);
        }
    }

    Err(ProviderProgramResolveError {
        candidates,
        error: "program was not found in PATH".to_string(),
    })
}

fn resolve_provider_program_path(
    program_path: &Path,
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    let candidates = provider_program_path_candidates(program_path);
    for candidate in &candidates {
        if candidate.is_file() {
            return resolved_provider_program_for_existing_path(candidate);
        }
    }

    Err(ProviderProgramResolveError {
        candidates,
        error: "program path was not found".to_string(),
    })
}

fn resolved_provider_program_for_existing_path(
    path: &Path,
) -> Result<ResolvedProviderProgram, ProviderProgramResolveError> {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "cmd" | "bat") {
            return Ok(ResolvedProviderProgram::CmdScript(path.to_path_buf()));
        }
    }

    Ok(ResolvedProviderProgram::Direct(path.to_path_buf()))
}

fn provider_program_lookup_candidates(program: &str, path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let program_path = Path::new(program);
        if program_path.extension().is_some() {
            return path_dirs.iter().map(|dir| dir.join(program)).collect();
        }

        return ["exe", "cmd", "bat"]
            .iter()
            .flat_map(|extension| {
                path_dirs
                    .iter()
                    .map(move |dir| dir.join(format!("{program}.{extension}")))
            })
            .collect();
    }

    #[cfg(not(windows))]
    {
        path_dirs.iter().map(|dir| dir.join(program)).collect()
    }
}

fn provider_program_path_candidates(program_path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if program_path.extension().is_some() {
            return vec![program_path.to_path_buf()];
        }

        return ["exe", "cmd", "bat"]
            .iter()
            .map(|extension| program_path.with_extension(extension))
            .collect();
    }

    #[cfg(not(windows))]
    {
        vec![program_path.to_path_buf()]
    }
}

fn build_provider_process_command(
    spec: &crate::pedelec_core::CommandSpec,
    resolved_program: &ResolvedProviderProgram,
) -> Command {
    let mut command = match resolved_program {
        ResolvedProviderProgram::Direct(program) => {
            let mut command = Command::new(program);
            command.args(&spec.args);
            command
        }
        #[cfg(windows)]
        ResolvedProviderProgram::CmdScript(program) => {
            let mut command = Command::new("cmd.exe");
            command.arg("/d").arg("/c").arg("call").arg(program);
            command.args(&spec.args);
            command
        }
    };

    command
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn command_env_path(command_env: &[(String, String)]) -> Option<OsString> {
    command_env
        .iter()
        .rev()
        .find(|(key, _)| env_key_is_path(key))
        .map(|(_, value)| OsString::from(value))
}

#[cfg(windows)]
fn env_key_is_path(key: &str) -> bool {
    key.eq_ignore_ascii_case("PATH")
}

#[cfg(not(windows))]
fn env_key_is_path(key: &str) -> bool {
    key == "PATH"
}

fn has_path_separator(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

fn provider_start_error_details(
    thread_id: &str,
    spec: &crate::pedelec_core::CommandSpec,
    resolved_program: Option<&ResolvedProviderProgram>,
    resolve_error: Option<ProviderProgramResolveError>,
) -> Value {
    let mut details = serde_json::json!({
        "threadId": thread_id,
        "program": spec.program,
        "args": spec.args,
        "cwd": spec.cwd.to_string_lossy(),
        "path": command_env_path(&spec.env)
            .or_else(|| env::var_os("PATH"))
            .map(|path| path.to_string_lossy().to_string())
    });
    if let Some(resolved_program) = resolved_program {
        details["resolvedProgram"] = match resolved_program {
            ResolvedProviderProgram::Direct(program) => serde_json::json!({
                "type": "direct",
                "path": program.to_string_lossy()
            }),
            #[cfg(windows)]
            ResolvedProviderProgram::CmdScript(program) => serde_json::json!({
                "type": "cmdScript",
                "path": program.to_string_lossy()
            }),
        };
    }
    if let Some(resolve_error) = resolve_error {
        details["error"] = serde_json::json!(resolve_error.error);
        if !resolve_error.candidates.is_empty() {
            details["programLookupCandidates"] = serde_json::json!(resolve_error
                .candidates
                .iter()
                .map(|candidate| candidate.to_string_lossy().to_string())
                .collect::<Vec<_>>());
        }
    }
    details
}

#[derive(Debug, Clone, Copy)]
enum ProviderStream {
    Stdout,
    Stderr,
}

fn spawn_provider_reader<R>(
    runtime: SharedCoreRuntime,
    thread_id: String,
    mut reader: R,
    stream: ProviderStream,
) where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        let mut decoder = ProviderOutputDecoder::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes_read) => {
                    let Some(text) = decoder.decode_chunk(&buffer[..bytes_read]) else {
                        continue;
                    };
                    if let Ok(mut runtime) = runtime.lock() {
                        match stream {
                            ProviderStream::Stdout => {
                                runtime.emit_provider_stdout(&thread_id, text)
                            }
                            ProviderStream::Stderr => {
                                runtime.emit_provider_stderr(&thread_id, text)
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(text) = decoder.flush() {
            if let Ok(mut runtime) = runtime.lock() {
                match stream {
                    ProviderStream::Stdout => runtime.emit_provider_stdout(&thread_id, text),
                    ProviderStream::Stderr => runtime.emit_provider_stderr(&thread_id, text),
                }
            }
        }
    });
}

struct ProviderOutputDecoder {
    pending: Vec<u8>,
    fallback_encoding: Option<&'static Encoding>,
}

impl ProviderOutputDecoder {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            fallback_encoding: provider_output_fallback_encoding(),
        }
    }

    fn decode_chunk(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        self.decode_pending(false)
    }

    fn flush(&mut self) -> Option<String> {
        self.decode_pending(true)
    }

    fn decode_pending(&mut self, flush: bool) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }

        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                Some(text)
            }
            Err(err) if err.error_len().is_none() && !flush => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to == 0 {
                    return None;
                }

                let suffix = self.pending.split_off(valid_up_to);
                let text = String::from_utf8(self.pending.split_off(0)).ok();
                self.pending = suffix;
                text
            }
            Err(_) => {
                let text = self.decode_with_fallback();
                self.pending.clear();
                Some(text)
            }
        }
    }

    fn decode_with_fallback(&self) -> String {
        if let Some(encoding) = self.fallback_encoding {
            let (text, _, _) = encoding.decode(&self.pending);
            return text.into_owned();
        }

        String::from_utf8_lossy(&self.pending).to_string()
    }
}

#[cfg(windows)]
fn provider_output_fallback_encoding() -> Option<&'static Encoding> {
    let code_page = unsafe { windows_sys::Win32::Globalization::GetACP() };
    provider_output_encoding_for_windows_code_page(code_page)
}

#[cfg(windows)]
fn provider_output_encoding_for_windows_code_page(code_page: u32) -> Option<&'static Encoding> {
    let label = match code_page {
        65001 => "utf-8",
        950 => "big5",
        936 => "gbk",
        932 => "shift_jis",
        949 => "euc-kr",
        874 => "windows-874",
        866 => "ibm866",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1252 => "windows-1252",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        _ => return None,
    };
    Encoding::for_label(label.as_bytes())
}

#[cfg(not(windows))]
fn provider_output_fallback_encoding() -> Option<&'static Encoding> {
    None
}

fn spawn_provider_waiter(
    runtime: SharedCoreRuntime,
    thread_id: String,
    process_id: u32,
    child: Arc<Mutex<Option<std::process::Child>>>,
) {
    thread::spawn(move || {
        let child = {
            let Ok(mut child) = child.lock() else {
                return;
            };
            child.take()
        };

        let Some(mut child) = child else {
            return;
        };

        match child.wait() {
            Ok(status) => {
                if let Ok(mut runtime) = runtime.lock() {
                    runtime.complete_provider_process(&thread_id, process_id, status);
                }
            }
            Err(err) => {
                if let Ok(mut runtime) = runtime.lock() {
                    runtime.fail_provider_process_wait(&thread_id, process_id, err.to_string());
                }
            }
        }
    });
}

fn ok_response(request_id: &str, result: Value) -> CoreIpcResponse {
    CoreIpcResponse {
        request_id: request_id.to_string(),
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn error_response(request_id: &str, error: PedelecError) -> CoreIpcResponse {
    CoreIpcResponse {
        request_id: request_id.to_string(),
        ok: false,
        result: None,
        error: Some(error),
    }
}

fn write_runtime_file(path: &Path, runtime_file: &RuntimeFile) -> Result<(), PedelecError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot create runtime.json directory",
                serde_json::json!({ "path": parent.to_string_lossy(), "error": err.to_string() }),
            )
        })?;
    }

    let payload = serde_json::to_string_pretty(runtime_file).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot serialize runtime.json",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(path, payload).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot write runtime.json",
            serde_json::json!({ "path": path.to_string_lossy(), "error": err.to_string() }),
        )
    })
}

fn read_runtime_file(path: impl AsRef<Path>) -> Result<RuntimeFile, PedelecError> {
    let path = path.as_ref();
    let payload = fs::read_to_string(path).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })?;
    serde_json::from_str(&payload).map_err(|_| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        )
    })
}

fn core_unavailable_error(_err: io::Error) -> PedelecError {
    PedelecError::new(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        "pedelec-app is not running",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pedelec_cli::{run_tool_cli_with_runtime_file_path, ThreadIdEnvGuard};
    use crate::pedelec_core::{
        CommandSpec, CoreRuntime, CreateThreadOutput, CreateThreadSkillsInput,
        CreateThreadToolInput, OllamaProviderSettings, PedelecSettings, ProviderAdapterState,
        ProviderCode, ProviderSettings, SandboxManager, ThreadState, ThreadStatus, ToolRegistry,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::env;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_prefers_exe_over_cmd() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("codex.exe");
        let cmd = bin_dir.join("codex.cmd");
        std::fs::write(&exe, b"fake-exe").unwrap();
        std::fs::write(&cmd, b"fake-cmd").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::Direct(exe));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_uses_cmd_shim_when_exe_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let cmd = bin_dir.join("codex.cmd");
        std::fs::write(&cmd, b"fake-cmd").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("Path".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::CmdScript(cmd));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_reports_lookup_candidates_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let err = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap_err();

        assert_eq!(err.error, "program was not found in PATH");
        assert_eq!(
            err.candidates,
            vec![
                bin_dir.join("codex.exe"),
                bin_dir.join("codex.cmd"),
                bin_dir.join("codex.bat")
            ]
        );
    }

    #[test]
    fn provider_output_decoder_preserves_split_utf8_sequence() {
        let mut decoder = ProviderOutputDecoder {
            pending: Vec::new(),
            fallback_encoding: None,
        };

        assert_eq!(decoder.decode_chunk("中".as_bytes().split_at(1).0), None);
        assert_eq!(
            decoder.decode_chunk("中".as_bytes().split_at(1).1),
            Some("中".into())
        );
    }

    #[test]
    fn provider_output_decoder_uses_fallback_for_non_utf8_bytes() {
        let mut decoder = ProviderOutputDecoder {
            pending: Vec::new(),
            fallback_encoding: Encoding::for_label(b"big5"),
        };

        assert_eq!(
            decoder.decode_chunk(&[0xA4, 0xA4, 0xA4, 0xE5]),
            Some("中文".into())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_provider_program_resolution_uses_bare_program_from_path() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let program = bin_dir.join("codex");
        std::fs::write(&program, b"fake-bin").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::Direct(program));
    }

    #[test]
    fn provider_process_command_uses_direct_program_with_original_args() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp
            .path()
            .join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let spec = test_command_spec("codex", temp.path(), vec!["exec".into(), "-".into()]);

        let command = build_provider_process_command(
            &spec,
            &ResolvedProviderProgram::Direct(program.clone()),
        );

        assert_eq!(command.get_program(), program.as_os_str());
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["exec", "-"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn provider_process_command_wraps_cmd_script_and_preserves_args() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex.cmd");
        let spec = test_command_spec(
            "codex",
            temp.path(),
            vec!["exec".into(), "--json".into(), "-".into()],
        );

        let command = build_provider_process_command(
            &spec,
            &ResolvedProviderProgram::CmdScript(script.clone()),
        );

        assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![
                "/d".to_string(),
                "/c".to_string(),
                "call".to_string(),
                script.to_string_lossy().to_string(),
                "exec".to_string(),
                "--json".to_string(),
                "-".to_string()
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn provider_process_command_runs_cmd_script_via_call() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("echo_args.cmd");
        std::fs::write(&script, b"@echo off\r\necho %1 %2\r\n").unwrap();
        let spec = test_command_spec(
            "echo_args",
            temp.path(),
            vec!["hello".into(), "world".into()],
        );

        let output =
            build_provider_process_command(&spec, &ResolvedProviderProgram::CmdScript(script))
                .output()
                .unwrap();

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello world"
        );
    }

    #[test]
    fn runtime_file_is_written_with_loopback_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let handle =
            start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json"))
                .unwrap();

        assert_eq!(handle.runtime_file.protocol, CORE_IPC_PROTOCOL);
        assert_eq!(handle.runtime_file.host, CORE_IPC_HOST);
        assert!(handle.runtime_file.endpoint.starts_with("127.0.0.1:"));
        assert!(handle.runtime_file_path.exists());
    }

    #[test]
    fn missing_runtime_file_maps_to_core_runtime_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let err = connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap_err();

        assert_eq!(err.code, error_codes::CORE_RUNTIME_UNAVAILABLE);
    }

    #[test]
    fn list_providers_core_ipc_returns_opencode_entry() {
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));

        let response = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "providers".into(),
                r#type: "list_providers".into(),
                payload: Some(json!({})),
            },
            runtime,
        );

        assert!(response.ok);
        let providers = response.result.unwrap().as_array().unwrap().clone();
        assert!(providers.iter().any(|provider| {
            provider.get("code") == Some(&json!("opencode"))
                && provider.get("name") == Some(&json!("OpenCode"))
                && provider.get("available").is_some()
        }));
    }

    #[test]
    fn settings_core_ipc_gets_and_updates_persisted_settings() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(test_provider_path(temp.path(), "codex")),
            ..CoreRuntime::default()
        }));

        let initial = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "settings_get_initial".into(),
                r#type: "get_settings".into(),
                payload: Some(json!({})),
            },
            Arc::clone(&runtime),
        );
        assert!(initial.ok);
        assert_eq!(
            initial.result.unwrap(),
            json!({
                "defaultProvider": null,
                "defaultModels": {},
                "providerSettings": {
                    "ollama": {
                        "baseUrl": "http://127.0.0.1:11434",
                        "timeoutMs": 120000,
                        "apiKey": ""
                    }
                }
            })
        );

        let updated = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "settings_update".into(),
                r#type: "update_settings".into(),
                payload: Some(json!({
                    "defaultProvider": "codex",
                    "defaultModels": {
                        "codex": " gpt-5 ",
                        "gemini": "gemini-2.5-pro"
                    },
                    "providerSettings": {
                        "ollama": {
                            "baseUrl": " http://127.0.0.1:11434/ ",
                            "timeoutMs": 120000,
                            "apiKey": "ollama"
                        }
                    }
                })),
            },
            Arc::clone(&runtime),
        );
        assert!(updated.ok);
        assert_eq!(
            updated.result.unwrap(),
            json!({
                "defaultProvider": "codex",
                "defaultModels": {
                    "codex": "gpt-5",
                    "gemini": "gemini-2.5-pro"
                },
                "providerSettings": {
                    "ollama": {
                        "baseUrl": "http://127.0.0.1:11434",
                        "timeoutMs": 120000,
                        "apiKey": "ollama"
                    }
                }
            })
        );

        assert_eq!(
            runtime.lock().unwrap().get_settings().unwrap(),
            PedelecSettings {
                default_provider: Some(ProviderCode::Codex),
                default_models: HashMap::from([
                    (ProviderCode::Codex, "gpt-5".into()),
                    (ProviderCode::Gemini, "gemini-2.5-pro".into()),
                ]),
                provider_settings: ProviderSettings {
                    ollama: OllamaProviderSettings {
                        api_key: "ollama".into(),
                        ..OllamaProviderSettings::default()
                    },
                },
            }
        );
    }

    #[test]
    fn request_id_is_echoed_for_unknown_request() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json")).unwrap();

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "req_1".into(),
                r#type: "missing".into(),
                payload: None,
            },
            temp.path().join("runtime.json"),
        )
        .unwrap();

        assert_eq!(response.request_id, "req_1");
        assert!(!response.ok);
    }

    #[test]
    fn oversized_message_returns_message_too_large() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json")).unwrap();
        let mut stream =
            connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap();
        let oversized = "x".repeat(MAX_CORE_IPC_MESSAGE_BYTES + 1);
        stream.write_all(oversized.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();

        let mut reader = BufReader::new(stream);
        let line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&line).unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::MESSAGE_TOO_LARGE);
    }

    #[test]
    fn subscribe_receives_later_thread_event() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(
            Arc::clone(&runtime),
            temp.path().join("runtime.json"),
        )
        .unwrap();

        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_sub",
            ThreadStatus::Idle,
            1000,
        );

        let mut stream =
            connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap();
        write_json_line(
            &mut stream,
            &CoreIpcRequest {
                request_id: "sub_1".into(),
                r#type: "subscribe_thread".into(),
                payload: Some(json!({ "threadId": "thread_sub" })),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let response_line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&response_line).unwrap();
        assert!(response.ok);

        runtime
            .lock()
            .unwrap()
            .event_bus
            .emit_raw_stdout("thread_sub", "hello".into());
        let event_line = read_bounded_json_line(&mut reader).unwrap();
        let event: CoreIpcEventMessage = serde_json::from_slice(&event_line).unwrap();

        assert_eq!(event.r#type, "thread_event");
    }

    #[test]
    fn send_text_rejects_busy_thread() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(
            Arc::clone(&runtime),
            temp.path().join("runtime.json"),
        )
        .unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_busy",
            ThreadStatus::Running,
            1000,
        );

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "send_1".into(),
                r#type: "send_text".into(),
                payload: Some(json!({ "threadId": "thread_busy", "message": "hello" })),
            },
            temp.path().join("runtime.json"),
        )
        .unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::THREAD_BUSY);
    }

    #[test]
    fn phase09_mock_app_path_create_send_tool_end_e2e() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_path = temp.path().join("runtime.json");
        let sandbox_root = temp.path().join("sandbox");
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        }));
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        let create = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "phase09_create".into(),
                r#type: "create_thread".into(),
                payload: Some(json!({
                    "provider": "codex",
                    "skills": phase09_skills_manifest()
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(create.ok);
        let output: CreateThreadOutput = serde_json::from_value(create.result.unwrap()).unwrap();
        assert_eq!(create.request_id, "phase09_create");
        assert_eq!(runtime.lock().unwrap().running_process_count(), 0);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&output.thread_id),
            Some(ThreadStatus::Idle)
        );

        let mut subscription = subscribe_to_thread(&runtime_path, &output.thread_id);
        install_test_provider_command(&runtime, &output.thread_id, true, false);
        let _thread_env = ThreadIdEnvGuard::set(Some(&output.thread_id));

        let send = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "phase09_send".into(),
                r#type: "send_text".into(),
                payload: Some(json!({
                    "threadId": output.thread_id,
                    "message": "call update_counter with delta 2"
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(send.ok);
        assert_eq!(
            send.result.unwrap(),
            json!({ "threadId": output.thread_id })
        );

        let first_tool_runtime_path = runtime_path.clone();
        let first_tool_handle = thread::spawn(move || {
            run_tool_cli_with_runtime_file_path(
                vec![
                    "pedelec-cli".into(),
                    "tool-call".into(),
                    "update_counter".into(),
                    r#"{"delta":2}"#.into(),
                ],
                Some(&first_tool_runtime_path),
            )
        });

        let mut events = Vec::new();
        let first_request_id = loop {
            let event = read_thread_event(&mut subscription).event;
            if let ThreadEvent::ToolCall { request_id, .. } = &event {
                let request_id = request_id.clone();
                events.push(event);
                break request_id;
            }
            events.push(event);
        };

        assert!(events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Running,
                    ..
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::WaitingToolResult,
                    ..
                }
            )
        }));
        assert_thread_event_seq_is_strictly_increasing(&events);

        let duplicate = run_tool_cli_with_runtime_file_path(
            vec![
                "pedelec-cli".into(),
                "tool-call".into(),
                "get_app_state".into(),
                "{}".into(),
            ],
            Some(&runtime_path),
        );
        assert!(!duplicate.ok);
        assert_eq!(
            duplicate.error.unwrap().code,
            error_codes::PENDING_TOOL_REQUEST_EXISTS
        );

        let submit = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "phase09_submit".into(),
                r#type: "submit_tool_result".into(),
                payload: Some(json!({
                    "threadId": output.thread_id,
                    "requestId": first_request_id,
                    "result": { "counter": 2 }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(submit.ok);
        let first_tool_response = first_tool_handle.join().unwrap();
        assert!(first_tool_response.ok);
        assert_eq!(first_tool_response.result.unwrap(), json!({ "counter": 2 }));

        let after_submit_events = collect_ipc_events_until(&mut subscription, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Idle,
                        ..
                    }
                )
            })
        });
        assert!(after_submit_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Running,
                    ..
                }
            )
        }));
        assert!(after_submit_events
            .iter()
            .any(|event| matches!(event, ThreadEvent::ToolResult { .. })));
        assert!(events
            .iter()
            .chain(after_submit_events.iter())
            .any(|event| matches!(event, ThreadEvent::RawStdout { .. })));
        assert!(events
            .iter()
            .chain(after_submit_events.iter())
            .any(|event| matches!(event, ThreadEvent::RawStderr { .. })));
        assert_eq!(
            runtime.lock().unwrap().thread_status(&output.thread_id),
            Some(ThreadStatus::Idle)
        );

        let sandbox_path = runtime
            .lock()
            .unwrap()
            .thread_sandbox_path(&output.thread_id)
            .unwrap();
        assert!(sandbox_path.exists());
        let end = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "phase09_end".into(),
                r#type: "end_thread".into(),
                payload: Some(json!({ "threadId": output.thread_id })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(end.ok);

        let end_events = collect_ipc_events_until(&mut subscription, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::Ended { .. }))
        });
        assert!(end_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Stopping,
                    ..
                }
            )
        }));
        assert!(!sandbox_path.exists());
    }

    #[test]
    fn phase09_pedelec_tool_timeout_returns_fixed_json_shape_with_runtime_override() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_tool_timeout_cli",
            ThreadStatus::Running,
            20,
        );

        let _env = ThreadIdEnvGuard::set(Some("thread_tool_timeout_cli"));
        let response = run_tool_cli_with_runtime_file_path(
            vec![
                "pedelec-cli".into(),
                "tool-call".into(),
                "get_app_state".into(),
                "{}".into(),
            ],
            Some(&runtime_path),
        );

        assert!(!response.ok);
        assert!(response.result.is_none());
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_TIMEOUT);
    }

    #[test]
    fn phase09_demo_tools_fixture_matches_registry_contract() {
        let skills: CreateThreadSkillsInput =
            serde_json::from_value(phase09_skills_manifest()).unwrap();
        let registry = ToolRegistry::from_skills_input(Some(&skills)).unwrap();

        assert!(registry
            .validate_tool_call("get_app_state", &json!({}))
            .is_ok());
        assert!(registry
            .validate_tool_call("update_counter", &json!({ "delta": 1 }))
            .is_ok());
        assert_eq!(
            registry
                .validate_tool_call("update_counter", &json!({}))
                .unwrap_err()
                .code,
            error_codes::TOOL_ARGS_INVALID
        );
    }

    #[test]
    fn tool_call_success_resolves_after_submit_tool_result() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_tool",
            ThreadStatus::Running,
            1000,
        );
        let mut subscription = subscribe_to_thread(&runtime_path, "thread_tool");

        let tool_path = runtime_path.clone();
        let tool_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_1".into(),
                    r#type: "tool_call".into(),
                    payload: Some(json!({
                        "threadId": "thread_tool",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                tool_path,
            )
            .unwrap()
        });

        let event = read_thread_event(&mut subscription);
        let request_id = match event.event {
            ThreadEvent::StatusChanged { .. } => match read_thread_event(&mut subscription).event {
                ThreadEvent::ToolCall { request_id, .. } => request_id,
                other => panic!("expected tool_call event, got {other:?}"),
            },
            ThreadEvent::ToolCall { request_id, .. } => request_id,
            other => panic!("expected status_changed/tool_call event, got {other:?}"),
        };

        let submit = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "submit_1".into(),
                r#type: "submit_tool_result".into(),
                payload: Some(json!({
                    "threadId": "thread_tool",
                    "requestId": request_id,
                    "result": { "value": 123 }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(submit.ok);

        let tool_response = tool_handle.join().unwrap();
        assert!(tool_response.ok);
        assert_eq!(tool_response.result.unwrap(), json!({ "value": 123 }));
    }

    #[test]
    fn tool_call_times_out_without_submit() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_timeout",
            ThreadStatus::Running,
            20,
        );

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "tool_timeout".into(),
                r#type: "tool_call".into(),
                payload: Some(json!({
                    "threadId": "thread_timeout",
                    "toolName": "get_app_state",
                    "args": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::TOOL_TIMEOUT);
    }

    #[test]
    fn second_tool_call_is_rejected_while_pending_exists() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_pending",
            ThreadStatus::Running,
            1000,
        );
        let mut subscription = subscribe_to_thread(&runtime_path, "thread_pending");

        let first_path = runtime_path.clone();
        let first_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_first".into(),
                    r#type: "tool_call".into(),
                    payload: Some(json!({
                        "threadId": "thread_pending",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                first_path,
            )
            .unwrap()
        });

        let event = read_thread_event(&mut subscription);
        let request_id = match event.event {
            ThreadEvent::StatusChanged { .. } => match read_thread_event(&mut subscription).event {
                ThreadEvent::ToolCall { request_id, .. } => request_id,
                other => panic!("expected tool_call event, got {other:?}"),
            },
            ThreadEvent::ToolCall { request_id, .. } => request_id,
            other => panic!("expected status_changed/tool_call event, got {other:?}"),
        };

        let second = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "tool_second".into(),
                r#type: "tool_call".into(),
                payload: Some(json!({
                    "threadId": "thread_pending",
                    "toolName": "get_app_state",
                    "args": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(
            second.error.unwrap().code,
            error_codes::PENDING_TOOL_REQUEST_EXISTS
        );

        send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "submit_pending".into(),
                r#type: "submit_tool_result".into(),
                payload: Some(json!({
                    "threadId": "thread_pending",
                    "requestId": request_id,
                    "result": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(first_handle.join().unwrap().ok);
    }

    #[test]
    fn tool_call_rejects_missing_tool_and_schema_invalid_args() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_schema",
            ThreadStatus::Running,
            1000,
        );

        let missing = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "missing_tool".into(),
                r#type: "tool_call".into(),
                payload: Some(json!({
                    "threadId": "thread_schema",
                    "toolName": "missing",
                    "args": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(missing.error.unwrap().code, error_codes::TOOL_NOT_FOUND);

        let invalid = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "invalid_args".into(),
                r#type: "tool_call".into(),
                payload: Some(json!({
                    "threadId": "thread_schema",
                    "toolName": "update_counter",
                    "args": { "delta": "1" }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(invalid.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    #[test]
    fn create_thread_rolls_back_sandbox_when_skill_load_fails() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let result = runtime.create_thread(CreateThreadInput {
            provider: ProviderCode::Codex,
            model: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: "bad".into(),
                tools: vec![CreateThreadToolInput {
                    name: "bad/name".into(),
                    description: "Bad.".into(),
                    args_schema: json!({ "type": "object" }),
                    timeout_ms: None,
                }],
            }),
        });

        assert_eq!(
            result.unwrap_err().code,
            error_codes::TOOLS_MANIFEST_INVALID
        );
        let entries = std::fs::read_dir(&sandbox_root)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(entries, 0);
    }

    #[test]
    fn send_text_starts_mock_process_and_emits_command_and_raw_stdout_stderr() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_mock",
            ThreadStatus::Idle,
            1000,
        );
        let event_rx = runtime.lock().unwrap().event_bus.subscribe("thread_mock");

        install_test_provider_command(&runtime, "thread_mock", true, false);

        let output = start_provider_process(
            Arc::clone(&runtime),
            SendTextInput {
                thread_id: "thread_mock".into(),
                message: "hello".into(),
            },
        )
        .unwrap();

        assert_eq!(output.thread_id, "thread_mock");
        assert_eq!(
            runtime.lock().unwrap().thread_status("thread_mock"),
            Some(ThreadStatus::Running)
        );
        assert!(runtime
            .lock()
            .unwrap()
            .active_process_id("thread_mock")
            .is_some());

        let events = collect_events_until(&event_rx, |events| {
            has_provider_command_started(events)
                && has_raw_stdout(events)
                && has_raw_stderr(events)
                && has_idle(events)
        });
        let command_event = find_provider_command_started(&events).unwrap();
        match command_event {
            ThreadEvent::ProviderCommandStarted {
                thread_id,
                process_id,
                program,
                args,
                cwd,
                prompt,
                ..
            } => {
                assert_eq!(thread_id, "thread_mock");
                assert!(*process_id > 0);
                assert!(!program.is_empty());
                assert!(!args.is_empty());
                assert_eq!(prompt, "test provider stdin");
                assert_eq!(
                    cwd,
                    &runtime
                        .lock()
                        .unwrap()
                        .thread_sandbox_path("thread_mock")
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                );
            }
            _ => unreachable!(),
        }
        let command_value = serde_json::to_value(command_event).unwrap();
        assert!(command_value.get("stdin").is_none());
        assert!(command_value.get("env").is_none());
        assert!(has_raw_stdout(&events));
        assert!(has_raw_stderr(&events));
        assert!(has_idle(&events));
        assert_eq!(
            runtime.lock().unwrap().thread_status("thread_mock"),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            runtime.lock().unwrap().active_process_id("thread_mock"),
            None
        );
    }

    #[test]
    fn mock_process_failure_sets_error_and_rejects_later_send() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_fail",
            ThreadStatus::Idle,
            1000,
        );
        let event_rx = runtime.lock().unwrap().event_bus.subscribe("thread_fail");

        install_test_provider_command(&runtime, "thread_fail", false, true);

        let output = start_provider_process(
            Arc::clone(&runtime),
            SendTextInput {
                thread_id: "thread_fail".into(),
                message: "fail".into(),
            },
        )
        .unwrap();
        assert_eq!(output.thread_id, "thread_fail");

        let events = collect_events_until(&event_rx, has_provider_command_failed);
        assert!(has_provider_command_failed(&events));
        assert_eq!(
            runtime.lock().unwrap().thread_status("thread_fail"),
            Some(ThreadStatus::Error)
        );

        let err = start_provider_process(
            Arc::clone(&runtime),
            SendTextInput {
                thread_id: "thread_fail".into(),
                message: "after error".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, error_codes::PROVIDER_COMMAND_FAILED);
    }

    #[test]
    fn end_thread_stops_running_process_emits_ended_and_removes_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_end",
            ThreadStatus::Idle,
            1000,
        );
        let event_rx = runtime.lock().unwrap().event_bus.subscribe("thread_end");

        install_test_provider_command(&runtime, "thread_end", true, false);

        start_provider_process(
            Arc::clone(&runtime),
            SendTextInput {
                thread_id: "thread_end".into(),
                message: "sleep".into(),
            },
        )
        .unwrap();
        let sandbox_path = runtime
            .lock()
            .unwrap()
            .thread_sandbox_path("thread_end")
            .unwrap();
        assert!(sandbox_path.exists());

        runtime
            .lock()
            .unwrap()
            .end_thread(EndThreadInput {
                thread_id: "thread_end".into(),
            })
            .unwrap();

        let events = collect_events_until(&event_rx, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::Ended { .. }))
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Ended { .. })));
        assert_eq!(
            runtime.lock().unwrap().thread_status("thread_end"),
            Some(ThreadStatus::Ended)
        );
        assert_eq!(
            runtime.lock().unwrap().active_process_id("thread_end"),
            None
        );
        assert_eq!(runtime.lock().unwrap().running_process_count(), 0);
        assert!(!sandbox_path.exists());
    }

    fn insert_thread_with_registry(
        runtime: &Arc<Mutex<CoreRuntime>>,
        temp: &Path,
        thread_id: &str,
        status: ThreadStatus,
        timeout_ms: u64,
    ) {
        let mut runtime = runtime.lock().unwrap();
        let now = chrono::Utc::now();
        let sandbox_root = temp.join("sandbox");
        runtime.sandbox_manager = SandboxManager::with_sandbox_root(&sandbox_root);
        let sandbox_path = sandbox_root.join(thread_id);
        std::fs::create_dir_all(sandbox_path.join("logs")).unwrap();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                model: None,
                sandbox_path,
                skills: vec![],
                status,
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
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_tools_json_str(&format!(
                r#"{{
                    "tools": [
                        {{
                            "name": "get_app_state",
                            "description": "Read state.",
                            "argsSchema": {{
                                "type": "object",
                                "properties": {{}},
                                "additionalProperties": false
                            }},
                            "timeoutMs": {timeout_ms}
                        }},
                        {{
                            "name": "update_counter",
                            "description": "Update counter.",
                            "argsSchema": {{
                                "type": "object",
                                "properties": {{ "delta": {{ "type": "integer" }} }},
                                "required": ["delta"],
                                "additionalProperties": false
                            }},
                            "timeoutMs": {timeout_ms}
                        }}
                    ]
                }}"#
            ))
            .unwrap(),
        );
    }

    fn install_test_provider_command(
        runtime: &Arc<Mutex<CoreRuntime>>,
        thread_id: &str,
        sleep: bool,
        fail: bool,
    ) {
        let cwd = runtime
            .lock()
            .unwrap()
            .thread_sandbox_path(thread_id)
            .unwrap();
        runtime.lock().unwrap().test_provider_command =
            Some(test_provider_command(cwd, sleep, fail));
    }

    fn test_provider_command(cwd: PathBuf, sleep: bool, fail: bool) -> CommandSpec {
        #[cfg(windows)]
        let (program, args) = {
            let script = format!(
                r#"
$inputText = [Console]::In.ReadToEnd()
[Console]::Out.Write("mock provider received: ")
[Console]::Out.WriteLine($inputText)
[Console]::Error.Write("mock provider stderr: codex")
if ({sleep}) {{ Start-Sleep -Seconds 2 }}
if ({fail}) {{ exit 7 }}
exit 0
"#,
                sleep = if sleep { "$true" } else { "$false" },
                fail = if fail { "$true" } else { "$false" }
            );
            (
                "powershell.exe".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-Command".to_string(),
                    script,
                ],
            )
        };

        #[cfg(not(windows))]
        let (program, args) = {
            let script = format!(
                r#"
input="$(cat)"
printf 'mock provider received: %s\n' "$input"
printf 'mock provider stderr: codex\n' >&2
{sleep}
{fail}
exit 0
"#,
                sleep = if sleep { "sleep 2" } else { ":" },
                fail = if fail { "exit 7" } else { ":" }
            );
            ("sh".to_string(), vec!["-c".to_string(), script])
        };

        CommandSpec {
            program,
            args,
            cwd,
            env: Vec::new(),
            prompt: "test provider stdin".to_string(),
            stdin: "test provider stdin".to_string(),
        }
    }

    fn collect_events_until(
        event_rx: &std::sync::mpsc::Receiver<ThreadEvent>,
        done: impl Fn(&[ThreadEvent]) -> bool,
    ) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        for _ in 0..20 {
            let event = event_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("timed out waiting for thread event");
            events.push(event);
            if done(&events) {
                return events;
            }
        }
        panic!("condition was not met by collected events: {events:?}");
    }

    fn has_provider_command_started(events: &[ThreadEvent]) -> bool {
        find_provider_command_started(events).is_some()
    }

    fn find_provider_command_started(events: &[ThreadEvent]) -> Option<&ThreadEvent> {
        events
            .iter()
            .find(|event| matches!(event, ThreadEvent::ProviderCommandStarted { .. }))
    }

    fn has_raw_stdout(events: &[ThreadEvent]) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::RawStdout { text, .. } if text.contains("mock provider received")
            )
        })
    }

    fn has_raw_stderr(events: &[ThreadEvent]) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::RawStderr { text, .. } if text.contains("mock provider stderr")
            )
        })
    }

    fn has_idle(events: &[ThreadEvent]) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Idle,
                    ..
                }
            )
        })
    }

    fn has_provider_command_failed(events: &[ThreadEvent]) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::Error {
                    error,
                    ..
                } if error.code == error_codes::PROVIDER_COMMAND_FAILED
            )
        })
    }

    fn subscribe_to_thread(runtime_path: &Path, thread_id: &str) -> BufReader<TcpStream> {
        let mut stream = connect_core_ipc_with_runtime_path(runtime_path).unwrap();
        write_json_line(
            &mut stream,
            &CoreIpcRequest {
                request_id: "sub".into(),
                r#type: "subscribe_thread".into(),
                payload: Some(json!({ "threadId": thread_id })),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let response_line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&response_line).unwrap();
        assert!(response.ok);
        reader
    }

    fn test_command_spec(program: &str, cwd: &Path, args: Vec<String>) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args,
            cwd: cwd.to_path_buf(),
            env: vec![("PATH".into(), cwd.to_string_lossy().into())],
            prompt: String::new(),
            stdin: String::new(),
        }
    }

    fn test_provider_path(root: &Path, program: &str) -> std::ffi::OsString {
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let program_name = format!("{program}.exe");
        #[cfg(not(windows))]
        let program_name = program.to_string();
        std::fs::write(bin_dir.join(program_name), b"fake provider").unwrap();
        env::join_paths([bin_dir]).unwrap()
    }

    fn read_thread_event(reader: &mut BufReader<TcpStream>) -> CoreIpcEventMessage {
        let event_line = read_bounded_json_line(reader).unwrap();
        serde_json::from_slice(&event_line).unwrap()
    }

    fn collect_ipc_events_until(
        reader: &mut BufReader<TcpStream>,
        done: impl Fn(&[ThreadEvent]) -> bool,
    ) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        for _ in 0..20 {
            let event = read_thread_event(reader).event;
            events.push(event);
            if done(&events) {
                return events;
            }
        }
        panic!("condition was not met by collected IPC events: {events:?}");
    }

    fn assert_thread_event_seq_is_strictly_increasing(events: &[ThreadEvent]) {
        let mut previous = 0;
        for event in events {
            let seq = event.seq();
            assert!(seq > previous, "event seq did not increase: {events:?}");
            previous = seq;
        }
    }

    fn phase09_tools_json() -> &'static str {
        r#"{
            "tools": [
                {
                    "name": "get_app_state",
                    "description": "Read current app state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    },
                    "timeoutMs": 60000
                },
                {
                    "name": "update_counter",
                    "description": "Update counter by delta.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {
                            "delta": { "type": "number" }
                        },
                        "required": ["delta"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 60000
                }
            ]
        }"#
    }

    fn phase09_skills_manifest() -> Value {
        let registry: Value = serde_json::from_str(phase09_tools_json()).unwrap();
        json!({
            "guidance": "Use get_app_state to read app state. Use update_counter to update the counter.",
            "tools": registry["tools"].clone()
        })
    }
}
