//! Loopback-only binary asset data plane.  The control plane only creates tickets.
use pedelec_core::{
    error_codes, file_mime_type, require_authoritative_directory,
    revalidate_workspace_regular_file, workspace_tmp_root, DenoModuleUploadState,
    FileDownloadState, FileUploadState, PedelecError, SharedCoreRuntime, WorkspaceFileFault,
    MAX_ASSET_UPLOAD_BYTES,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;

fn asset_upload_temp_path(workspace_path: &Path, upload_id: &str) -> std::path::PathBuf {
    workspace_tmp_root(workspace_path).join(format!("{upload_id}.upload"))
}

/// Open `<workspace>/.pedelec-runtime/tmp/<uploadId>.upload`.
///
/// The Workspace root and tmp root must already exist as real directories.
/// This helper never creates either of them.
fn open_workspace_upload_staging_file(
    workspace_path: &Path,
    upload_id: &str,
) -> std::io::Result<(std::path::PathBuf, File)> {
    let canonical_workspace = require_authoritative_directory(workspace_path)?;
    let tmp_root = workspace_tmp_root(workspace_path);
    let canonical_tmp = require_authoritative_directory(&tmp_root)?;
    if !canonical_tmp.starts_with(&canonical_workspace) {
        return Err(std::io::Error::other(
            "workspace upload staging root escapes workspace",
        ));
    }
    let path = tmp_root.join(format!("{upload_id}.upload"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((path, file))
}

pub fn start_asset_upload_server(runtime: SharedCoreRuntime) -> Result<u16, PedelecError> {
    // Binding port 0 asks the OS for a new loopback port on every attempt.
    let mut last_error = None;
    for _ in 0..3 {
        match TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => {
                let port = listener
                    .local_addr()
                    .map_err(|e| {
                        PedelecError::new(
                            error_codes::ASSET_UPLOAD_SERVER_UNAVAILABLE,
                            e.to_string(),
                        )
                    })?
                    .port();
                runtime.lock().unwrap().set_asset_upload_port(port);
                thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let runtime = Arc::clone(&runtime);
                        thread::spawn(move || {
                            let _ = handle(stream, runtime);
                        });
                    }
                });
                return Ok(port);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(PedelecError::new(
        error_codes::ASSET_UPLOAD_SERVER_UNAVAILABLE,
        format!(
            "cannot start asset upload server: {}",
            last_error.map(|e| e.to_string()).unwrap_or_default()
        ),
    ))
}

fn handle(mut stream: TcpStream, runtime: SharedCoreRuntime) -> std::io::Result<()> {
    let clone = stream.try_clone()?;
    let mut reader = BufReader::new(clone);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut headers = std::collections::HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if first.starts_with("OPTIONS ") {
        return respond(&mut stream, 204, None);
    }
    if first.starts_with("GET ") {
        return handle_download(&mut stream, runtime, &first, &headers);
    }
    let deno_module_upload_id = first
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.strip_prefix("/deno-modules/"))
        .unwrap_or("");
    if first.starts_with("PUT ") && !deno_module_upload_id.is_empty() {
        return handle_deno_module_upload(
            &mut stream,
            runtime,
            deno_module_upload_id,
            &headers,
            &mut reader,
        );
    }
    let upload_id = first
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.strip_prefix("/uploads/"))
        .unwrap_or("");
    if !first.starts_with("PUT ") || upload_id.is_empty() {
        return respond_error(
            &mut stream,
            400,
            error_codes::INVALID_INPUT,
            "expected PUT /uploads/<uploadId>",
        );
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let length = headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    let (staging_root, commit_root, relative_path, expected, response_path, audience) = {
        let mut core = runtime.lock().unwrap();
        core.expire_file_uploads();
        let ticket = match core.file_upload_tickets.get_mut(upload_id) {
            Some(ticket) => ticket,
            None => {
                return respond_error(
                    &mut stream,
                    401,
                    error_codes::ASSET_UPLOAD_UNAUTHORIZED,
                    "upload ticket is invalid",
                )
            }
        };
        let audience = ticket.audience;
        if ticket.state == FileUploadState::Expired {
            return respond_error(
                &mut stream,
                410,
                audience.upload_expired(),
                "upload ticket has expired",
            );
        }
        if ticket.state != FileUploadState::Pending
            || format!("{:x}", Sha256::digest(token.as_bytes())) != ticket.token_hash
        {
            ticket.state = FileUploadState::Failed;
            return respond_error(
                &mut stream,
                401,
                audience.upload_unauthorized(),
                "upload token is invalid",
            );
        }
        if length.is_some_and(|n| n > ticket.expected_size_bytes || n > MAX_ASSET_UPLOAD_BYTES) {
            ticket.state = FileUploadState::Failed;
            return respond_error(
                &mut stream,
                413,
                audience.upload_size_mismatch(),
                "upload size does not match ticket",
            );
        }
        ticket.state = FileUploadState::Uploading;
        (
            ticket.workspace_path.clone(),
            ticket.commit_root.clone(),
            ticket.relative_path.clone(),
            ticket.expected_size_bytes,
            ticket.response_path.clone(),
            audience,
        )
    };
    let mut tmp = asset_upload_temp_path(&staging_root, upload_id);
    let root_is_authoritative = require_authoritative_directory(&staging_root).is_ok()
        && require_authoritative_directory(&commit_root).is_ok();
    let result = if root_is_authoritative {
        (|| -> std::io::Result<u64> {
            let (path, mut file) = open_workspace_upload_staging_file(&staging_root, upload_id)?;
            tmp = path;
            let mut total = 0u64;
            let mut buf = [0u8; 64 * 1024];
            while total < expected {
                let want = ((expected - total) as usize).min(buf.len());
                let n = reader.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                total += n as u64;
            }
            file.flush()?;
            Ok(total)
        })()
    } else {
        Err(std::io::Error::other("workspace file root is unsafe"))
    };
    let ok = matches!(result, Ok(n) if n == expected);
    if ok {
        let committed = {
            let mut core = runtime.lock().unwrap();
            core.commit_admitted_file_upload(
                upload_id,
                &tmp,
                &commit_root,
                &relative_path,
                audience,
            )
        };
        if committed.is_ok() {
            let body = serde_json::json!({ "path": response_path }).to_string();
            return respond(&mut stream, 201, Some(&body));
        }
    }
    let _ = fs::remove_file(&tmp);
    {
        let mut core = runtime.lock().unwrap();
        if let Some(ticket) = core.file_upload_tickets.get_mut(upload_id) {
            if ticket.state == FileUploadState::Uploading {
                ticket.state = FileUploadState::Failed;
            }
        }
    }
    let message = match audience {
        pedelec_core::FileTransferAudience::Asset => "asset upload failed",
        pedelec_core::FileTransferAudience::Workspace => "workspace file upload failed",
    };
    respond_error(&mut stream, 400, audience.upload_failed(), message)
}

fn handle_deno_module_upload(
    stream: &mut TcpStream,
    runtime: SharedCoreRuntime,
    upload_id: &str,
    headers: &std::collections::HashMap<String, String>,
    reader: &mut BufReader<TcpStream>,
) -> std::io::Result<()> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok());
    let (temporary_path, expected_size) = {
        let mut core = runtime.lock().unwrap();
        core.expire_deno_module_uploads();
        let ticket = match core.deno_module_upload_tickets.get_mut(upload_id) {
            Some(ticket) => ticket,
            None => {
                return respond_error(
                    stream,
                    401,
                    error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                    "Deno Module upload ticket is invalid",
                )
            }
        };
        if ticket.state == DenoModuleUploadState::Expired {
            return respond_error(
                stream,
                410,
                error_codes::DENO_MODULE_UPLOAD_TICKET_EXPIRED,
                "Deno Module upload ticket has expired",
            );
        }
        if ticket.state != DenoModuleUploadState::Pending
            || format!("{:x}", Sha256::digest(token.as_bytes())) != ticket.token_hash
        {
            ticket.state = DenoModuleUploadState::Failed;
            return respond_error(
                stream,
                401,
                error_codes::DENO_MODULE_UPLOAD_UNAUTHORIZED,
                "Deno Module upload token is invalid",
            );
        }
        if length != Some(ticket.expected_size_bytes)
            || ticket.expected_size_bytes > MAX_ASSET_UPLOAD_BYTES
        {
            ticket.state = DenoModuleUploadState::Failed;
            return respond_error(
                stream,
                413,
                error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
                "Deno Module upload size does not match its ticket",
            );
        }
        ticket.state = DenoModuleUploadState::Uploading;
        (
            workspace_tmp_root(&ticket.workspace_path)
                .join(format!("{upload_id}.deno-module.upload")),
            ticket.expected_size_bytes,
        )
    };

    let result = (|| -> std::io::Result<u64> {
        fs::create_dir_all(temporary_path.parent().unwrap())?;
        let mut file = File::create(&temporary_path)?;
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        while total < expected_size {
            let want = ((expected_size - total) as usize).min(buffer.len());
            let count = reader.read(&mut buffer[..want])?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])?;
            total += count as u64;
        }
        file.flush()?;
        Ok(total)
    })();

    if !matches!(result, Ok(size) if size == expected_size) {
        let _ = fs::remove_file(&temporary_path);
        runtime
            .lock()
            .unwrap()
            .mark_deno_module_upload_failed(upload_id);
        return respond_error(
            stream,
            400,
            error_codes::DENO_MODULE_UPLOAD_SIZE_MISMATCH,
            "Deno Module upload body was truncated",
        );
    }

    let completion = runtime
        .lock()
        .unwrap()
        .complete_deno_module_upload(upload_id, &temporary_path);
    let _ = fs::remove_file(&temporary_path);
    match completion {
        Ok(completion) => respond(
            stream,
            201,
            Some(&serde_json::to_string(&completion).unwrap_or_else(|_| "{}".to_string())),
        ),
        Err(error) => {
            let status = if error.code == error_codes::DENO_MODULE_ARTIFACT_TOO_LARGE {
                413
            } else {
                422
            };
            let body = serde_json::json!({ "error": error });
            respond(stream, status, Some(&body.to_string()))
        }
    }
}

fn handle_download(
    stream: &mut TcpStream,
    runtime: SharedCoreRuntime,
    first: &str,
    headers: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let download_id = first
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.strip_prefix("/downloads/"))
        .unwrap_or("");
    let token = headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let (target, expected_length, mime_type, audience) = {
        let mut core = runtime.lock().unwrap();
        core.expire_file_downloads();
        let ticket = match core.file_download_tickets.get_mut(download_id) {
            Some(ticket) => ticket,
            None => {
                return respond_error(
                    stream,
                    401,
                    error_codes::ASSET_DOWNLOAD_UNAUTHORIZED,
                    "download ticket is invalid",
                )
            }
        };
        let audience = ticket.audience;
        if ticket.state == FileDownloadState::Expired {
            return respond_error(
                stream,
                410,
                audience.download_expired(),
                "download ticket has expired",
            );
        }
        if ticket.state != FileDownloadState::Pending
            || format!("{:x}", Sha256::digest(token.as_bytes())) != ticket.token_hash
        {
            ticket.state = FileDownloadState::Failed;
            return respond_error(
                stream,
                401,
                audience.download_unauthorized(),
                "download token is invalid",
            );
        }
        let resolved =
            revalidate_workspace_regular_file(&ticket.commit_root, &ticket.relative_path);
        let file = match resolved {
            Ok(file) if file.size_bytes == ticket.expected_size_bytes => file,
            Err(WorkspaceFileFault::Invalid) => {
                ticket.state = FileDownloadState::Failed;
                return respond_error(stream, 400, audience.path_invalid(), "file path is invalid");
            }
            _ => {
                ticket.state = FileDownloadState::Failed;
                return respond_error(stream, 404, audience.read_failed(), "file is unavailable");
            }
        };
        ticket.state = FileDownloadState::Downloading;
        let mime = file_mime_type(&file.canonical_target);
        (file.canonical_target, file.size_bytes, mime, audience)
    };
    let body = match File::open(&target)
        .and_then(|mut file| read_exact_body(&mut file, expected_length))
    {
        Ok(body) => body,
        Err(_) => {
            runtime
                .lock()
                .unwrap()
                .file_download_tickets
                .get_mut(download_id)
                .map(|ticket| {
                    ticket.state = FileDownloadState::Failed;
                });
            return respond_error(
                stream,
                409,
                audience.read_failed(),
                "file changed during read",
            );
        }
    };
    let write_result = (|| -> std::io::Result<()> {
        write!(stream, "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, PUT, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nContent-Type: {mime_type}\r\nContent-Length: {expected_length}\r\n\r\n")?;
        stream.write_all(&body)
    })();
    runtime
        .lock()
        .unwrap()
        .file_download_tickets
        .get_mut(download_id)
        .map(|ticket| {
            ticket.state = if write_result.is_ok() {
                FileDownloadState::Completed
            } else {
                FileDownloadState::Failed
            }
        });
    write_result
}

fn read_exact_body(reader: &mut impl Read, expected: u64) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut remaining = expected;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let limit = remaining.min(buf.len() as u64) as usize;
        let count = reader.read(&mut buf[..limit])?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file changed during read",
            ));
        }
        body.extend_from_slice(&buf[..count]);
        remaining -= count as u64;
    }
    Ok(body)
}

fn respond(stream: &mut TcpStream, status: u16, body: Option<&str>) -> std::io::Result<()> {
    let body = body.unwrap_or("");
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        410 => "Gone",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        _ => "OK",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, PUT, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
}
fn respond_error(
    stream: &mut TcpStream,
    status: u16,
    code: &str,
    message: &str,
) -> std::io::Result<()> {
    respond(
        stream,
        status,
        Some(&format!(
            r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_core::finalize_asset_write;

    #[test]
    fn browser_asset_replacement_survives_shared_finalizer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("assets");
        fs::create_dir_all(&root).unwrap();
        let relative = Path::new("nested/image.png");
        let first = root.join("first.tmp");
        fs::write(&first, b"first").unwrap();
        finalize_asset_write(&first, &root, relative, true, "first").unwrap();
        let second = root.join("second.tmp");
        fs::write(&second, b"second").unwrap();
        finalize_asset_write(&second, &root, relative, true, "second").unwrap();
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"second");
        let third = root.join("third.tmp");
        fs::write(&third, b"third").unwrap();
        assert!(finalize_asset_write(&third, &root, relative, false, "third").is_err());
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"second");
    }
    use chrono::Utc;
    use pedelec_core::{
        workspace_assets_root, workspace_deno_modules_root, workspace_tmp_root, AssetUploadState,
        CoreRuntime, CreateAssetUploadInput, CreateDenoModuleUploadInput,
        CreateWorkspaceFileDownloadInput, CreateWorkspaceFileUploadInput, DenoModuleSetupState,
        DenoModuleState, DenoModuleUploadState, EffortLevel, ProviderCode, ProviderSessionState,
        ThreadState, ThreadStatus, WorkspaceKind,
    };
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn upload_ticket_uses_private_asset_and_tmp_layout() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_path).unwrap();
        initialize_workspace_runtime_tmp(&workspace_path);
        let thread_id = "thread_upload_layout".to_string();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-upload-layout";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace_path, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.clone(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec![],
                skills: vec![],
                status: ThreadStatus::Idle,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        drop(runtime_guard);

        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let payload = b"private asset layout";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_asset_upload(CreateAssetUploadInput {
                thread_id: thread_id.clone(),
                target_path: Some("/nested/upload.txt".into()),
                filename: "upload.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "PUT /uploads/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            ticket.upload_id,
            ticket.token,
            payload.len()
        )
        .unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();

        assert!(response.starts_with("HTTP/1.1 201"), "response: {response}");
        assert!(response.contains(r#"{"path":"/nested/upload.txt"}"#));
        assert!(!response.contains(".pedelec-runtime"));

        let private_asset = workspace_assets_root(&workspace_path).join("nested/upload.txt");
        assert_eq!(std::fs::read(&private_asset).unwrap(), payload.as_slice());
        assert!(!workspace_path.join("assets/nested/upload.txt").exists());

        let private_tmp_root = workspace_tmp_root(&workspace_path);
        let private_tmp = asset_upload_temp_path(&workspace_path, &ticket.upload_id);
        assert_eq!(private_tmp.parent(), Some(private_tmp_root.as_path()));
        assert!(private_tmp_root.is_dir());
        assert!(!private_tmp.exists());
        assert!(!workspace_path.join("tmp").exists());

        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .file_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            AssetUploadState::Completed
        );
    }

    #[test]
    fn deno_module_upload_route_commits_and_rejects_malformed_artifacts() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        let thread_id = "thread_deno_transfer".to_string();
        std::fs::create_dir_all(&workspace_path).unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        let mut runtime_guard = runtime.lock().unwrap();
        let workspace_id = "workspace-deno-transfer";
        runtime_guard
            .register_workspace_for_test(workspace_id, &workspace_path, WorkspaceKind::Custom)
            .unwrap();
        runtime_guard.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.clone(),
                workspace_id: workspace_id.into(),
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Default),
                effort_args: vec![],
                skills: vec![],
                status: ThreadStatus::Idle,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: Some("https://app.example.test".into()),
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        drop(runtime_guard);
        runtime.lock().unwrap().deno_modules.insert(
            thread_id.clone(),
            vec![DenoModuleState {
                name: "sprite-tools".into(),
                description: "Sprite helpers".into(),
                usage: "import \"sprite-tools\";".into(),
                prefer_stdin_execution: false,
                state: DenoModuleSetupState::Pending,
            }],
        );

        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let envelope = serde_json::json!({
            "version": 1,
            "format": "esm",
            "runtimeSource": "export const ready = true;",
            "typesSource": "export declare const ready: boolean;",
        });
        let payload = serde_json::to_vec(&envelope).unwrap();
        let ticket = runtime
            .lock()
            .unwrap()
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: thread_id.clone(),
                module_name: "sprite-tools".into(),
                expected_size_bytes: payload.len() as u64,
            })
            .unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "PUT /deno-modules/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            ticket.upload_id,
            ticket.token,
            payload.len()
        )
        .unwrap();
        stream.write_all(&payload).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 201"), "response: {response}");
        assert!(response.contains(r#""ready":true"#));
        let package = workspace_deno_modules_root(&workspace_path, &thread_id).join("sprite-tools");
        assert_eq!(
            std::fs::read_to_string(package.join("index.d.ts")).unwrap(),
            "export declare const ready: boolean;"
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .deno_module_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            DenoModuleUploadState::Completed
        );

        let malformed = b"not-json";
        runtime
            .lock()
            .unwrap()
            .deno_modules
            .get_mut(&thread_id)
            .unwrap()[0]
            .state = DenoModuleSetupState::Pending;
        let malformed_ticket = runtime
            .lock()
            .unwrap()
            .create_deno_module_upload(CreateDenoModuleUploadInput {
                thread_id: thread_id.clone(),
                module_name: "sprite-tools".into(),
                expected_size_bytes: malformed.len() as u64,
            })
            .unwrap();
        let mut malformed_stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            malformed_stream,
            "PUT /deno-modules/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            malformed_ticket.upload_id,
            malformed_ticket.token,
            malformed.len()
        )
        .unwrap();
        malformed_stream.write_all(malformed).unwrap();
        malformed_stream.shutdown(Shutdown::Write).unwrap();
        let mut malformed_response = Vec::new();
        malformed_stream
            .read_to_end(&mut malformed_response)
            .unwrap();
        let malformed_response = String::from_utf8(malformed_response).unwrap();
        assert!(
            malformed_response.starts_with("HTTP/1.1 422"),
            "response: {malformed_response}"
        );
        assert!(malformed_response.contains("DENO_MODULE_ARTIFACT_INVALID"));
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .deno_module_upload_tickets
                .get(&malformed_ticket.upload_id)
                .unwrap()
                .state,
            DenoModuleUploadState::Failed
        );
    }

    #[test]
    fn short_download_body_fails_before_a_success_response() {
        let mut reader = std::io::Cursor::new(b"hi".to_vec());
        assert!(read_exact_body(&mut reader, 4).is_err());
    }

    #[test]
    fn workspace_upload_and_download_use_workspace_relative_paths() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_path).unwrap();
        initialize_workspace_runtime_tmp(&workspace_path);
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_http", &workspace_path, WorkspaceKind::Custom)
            .unwrap();
        let port = start_asset_upload_server(runtime.clone()).unwrap();

        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_http".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "PUT /uploads/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            ticket.upload_id, ticket.token, payload.len()
        )
        .unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 201"), "response: {response}");
        assert!(response.contains(r#""path":"hello.txt""#));
        assert!(!response.contains("upl_"));
        assert_eq!(
            std::fs::read(workspace_path.join("hello.txt")).unwrap(),
            payload
        );

        let replaced = b"hello!";
        let replace_ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_http".into(),
                target_path: Some("hello.txt".into()),
                filename: "hello.txt".into(),
                size_bytes: replaced.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        let mut replace_stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            replace_stream,
            "PUT /uploads/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer bad\r\nContent-Length: {}\r\n\r\n",
            replace_ticket.upload_id, replaced.len()
        )
        .unwrap();
        replace_stream.write_all(replaced).unwrap();
        replace_stream.shutdown(Shutdown::Write).unwrap();
        let mut rejected = String::new();
        replace_stream.read_to_string(&mut rejected).unwrap();
        assert!(rejected.starts_with("HTTP/1.1 401"), "response: {rejected}");
        assert!(rejected.contains("WORKSPACE_FILE_UPLOAD_UNAUTHORIZED"));
        assert_eq!(
            std::fs::read(workspace_path.join("hello.txt")).unwrap(),
            payload
        );

        let mut reuse = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            reuse,
            "PUT /uploads/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            ticket.upload_id, ticket.token, payload.len()
        )
        .unwrap();
        reuse.write_all(payload).unwrap();
        reuse.shutdown(Shutdown::Write).unwrap();
        let mut reused = String::new();
        reuse.read_to_string(&mut reused).unwrap();
        assert!(reused.starts_with("HTTP/1.1 401"), "response: {reused}");

        let download = runtime
            .lock()
            .unwrap()
            .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
                workspace_id: "ws_http".into(),
                path: "hello.txt".into(),
            })
            .unwrap();
        std::fs::write(workspace_path.join("hello.txt"), b"x").unwrap();
        let mut download_stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            download_stream,
            "GET /downloads/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\r\n",
            download.download_id, download.token
        )
        .unwrap();
        download_stream.shutdown(Shutdown::Write).unwrap();
        let mut download_response = String::new();
        download_stream
            .read_to_string(&mut download_response)
            .unwrap();
        assert!(
            !download_response.starts_with("HTTP/1.1 200"),
            "response: {download_response}"
        );
        assert!(download_response.contains("WORKSPACE_FILE_READ_FAILED"));
    }

    fn initialize_workspace_runtime_tmp(workspace_path: &Path) {
        std::fs::create_dir_all(workspace_tmp_root(workspace_path)).unwrap();
    }

    fn put_upload(port: u16, upload_id: &str, token: &str, payload: &[u8]) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "PUT /uploads/{upload_id} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn removed_workspace_root_is_not_recreated_by_upload() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_path).unwrap();
        let canonical = workspace_path.canonicalize().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_removed", &canonical, WorkspaceKind::Custom)
            .unwrap();
        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_removed".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        std::fs::remove_dir_all(&canonical).unwrap();

        let response = put_upload(port, &ticket.upload_id, &ticket.token, payload);
        assert!(
            !response.starts_with("HTTP/1.1 201"),
            "response: {response}"
        );
        assert!(
            response.contains("WORKSPACE_FILE_UPLOAD_FAILED"),
            "response: {response}"
        );
        assert!(!canonical.exists());
        assert!(!canonical.join("hello.txt").exists());
        let state = runtime
            .lock()
            .unwrap()
            .file_upload_tickets
            .get(&ticket.upload_id)
            .unwrap()
            .state;
        assert_eq!(state, AssetUploadState::Failed);
    }

    #[test]
    fn symlink_replaced_workspace_root_does_not_receive_the_upload() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&workspace_path).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let canonical = workspace_path.canonicalize().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_symlink", &canonical, WorkspaceKind::Custom)
            .unwrap();
        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_symlink".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        std::fs::remove_dir_all(&canonical).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &canonical).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&outside, &canonical).is_err() {
            return;
        }

        let response = put_upload(port, &ticket.upload_id, &ticket.token, payload);
        assert!(
            !response.starts_with("HTTP/1.1 201"),
            "response: {response}"
        );
        assert!(!outside.join("hello.txt").exists());
        assert!(!outside.join(".pedelec-runtime").exists());
        let state = runtime
            .lock()
            .unwrap()
            .file_upload_tickets
            .get(&ticket.upload_id)
            .unwrap()
            .state;
        assert_ne!(state, AssetUploadState::Completed);
        let _ = std::fs::remove_dir(&canonical);
    }

    #[test]
    fn staging_open_does_not_recreate_a_removed_workspace_root() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_path).unwrap();
        initialize_workspace_runtime_tmp(&workspace_path);
        let canonical = workspace_path.canonicalize().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime.lock().unwrap().set_asset_upload_port(9);
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_staging_removed", &canonical, WorkspaceKind::Custom)
            .unwrap();
        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_staging_removed".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .file_upload_tickets
            .get_mut(&ticket.upload_id)
            .unwrap()
            .state = AssetUploadState::Uploading;
        std::fs::remove_dir_all(&canonical).unwrap();

        assert!(open_workspace_upload_staging_file(&canonical, &ticket.upload_id).is_err());
        assert!(!canonical.exists());
        assert!(!workspace_tmp_root(&canonical).exists());
        assert!(!canonical.join("hello.txt").exists());
        assert_ne!(
            runtime
                .lock()
                .unwrap()
                .file_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            AssetUploadState::Completed
        );
    }

    #[test]
    fn missing_tmp_root_is_not_recreated_by_upload() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_path).unwrap();
        initialize_workspace_runtime_tmp(&workspace_path);
        let canonical = workspace_path.canonicalize().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_missing_tmp", &canonical, WorkspaceKind::Custom)
            .unwrap();
        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_missing_tmp".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        let tmp_root = workspace_tmp_root(&canonical);
        std::fs::remove_dir_all(&tmp_root).unwrap();

        let response = put_upload(port, &ticket.upload_id, &ticket.token, payload);
        assert!(
            !response.starts_with("HTTP/1.1 201"),
            "response: {response}"
        );
        assert!(
            response.contains("WORKSPACE_FILE_UPLOAD_FAILED"),
            "response: {response}"
        );
        assert!(canonical.is_dir());
        assert!(!tmp_root.exists());
        assert!(!canonical.join("hello.txt").exists());
        assert!(!asset_upload_temp_path(&canonical, &ticket.upload_id).exists());
        assert_ne!(
            runtime
                .lock()
                .unwrap()
                .file_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            AssetUploadState::Completed
        );
    }

    #[test]
    fn symlink_replaced_tmp_root_does_not_receive_the_upload() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&workspace_path).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        initialize_workspace_runtime_tmp(&workspace_path);
        let canonical = workspace_path.canonicalize().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime
            .lock()
            .unwrap()
            .register_workspace_for_test("ws_tmp_symlink", &canonical, WorkspaceKind::Custom)
            .unwrap();
        let port = start_asset_upload_server(runtime.clone()).unwrap();
        let payload = b"hello";
        let ticket = runtime
            .lock()
            .unwrap()
            .create_workspace_file_upload(CreateWorkspaceFileUploadInput {
                workspace_id: "ws_tmp_symlink".into(),
                target_path: None,
                filename: "hello.txt".into(),
                size_bytes: payload.len() as u64,
                mime_type: "text/plain".into(),
            })
            .unwrap();
        let tmp_root = workspace_tmp_root(&canonical);
        std::fs::remove_dir_all(&tmp_root).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &tmp_root).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&outside, &tmp_root).is_err() {
            return;
        }

        let response = put_upload(port, &ticket.upload_id, &ticket.token, payload);
        assert!(
            !response.starts_with("HTTP/1.1 201"),
            "response: {response}"
        );
        assert!(response.contains("WORKSPACE_FILE_UPLOAD_FAILED"));
        assert!(!outside.join("hello.txt").exists());
        assert!(!outside
            .join(format!("{}.upload", ticket.upload_id))
            .exists());
        assert!(!canonical.join("hello.txt").exists());
        assert_ne!(
            runtime
                .lock()
                .unwrap()
                .file_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            AssetUploadState::Completed
        );
        let _ = std::fs::remove_dir(&tmp_root);
    }
}
