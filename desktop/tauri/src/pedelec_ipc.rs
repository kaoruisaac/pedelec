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

    let stdout_reader = stdout.map(|stdout| {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stdout,
            ProviderStream::Stdout,
        )
    });
    let stderr_reader = stderr.map(|stderr| {
        spawn_provider_reader(
            Arc::clone(&runtime),
            thread_id.clone(),
            stderr,
            ProviderStream::Stderr,
        )
    });
    spawn_provider_waiter(
        runtime,
        thread_id,
        process_id,
        child,
        stdout_reader,
        stderr_reader,
    );

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
) -> thread::JoinHandle<()>
where
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
    })
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
    stdout_reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
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
                if let Some(reader) = stdout_reader {
                    let _ = reader.join();
                }
                if let Some(reader) = stderr_reader {
                    let _ = reader.join();
                }
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
#[path = "pedelec_ipc/tests/mod.rs"]
mod tests;
