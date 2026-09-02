//! Loopback-only binary asset data plane.  The control plane only creates tickets.
use pedelec_core::{
    error_codes, workspace_assets_root, workspace_tmp_root, AssetDownloadState, AssetUploadState,
    PedelecError, SharedCoreRuntime, MAX_ASSET_UPLOAD_BYTES,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;

fn asset_upload_temp_path(workspace_path: &Path, upload_id: &str) -> std::path::PathBuf {
    workspace_tmp_root(workspace_path).join(format!("{upload_id}.upload"))
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
    let (tmp, asset_root, relative_path, final_path, expected, public_path) = {
        let mut core = runtime.lock().unwrap();
        core.expire_asset_uploads();
        let ticket = match core.asset_upload_tickets.get_mut(upload_id) {
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
        if ticket.state == AssetUploadState::Expired {
            return respond_error(
                &mut stream,
                410,
                error_codes::ASSET_UPLOAD_TICKET_EXPIRED,
                "upload ticket has expired",
            );
        }
        if ticket.state != AssetUploadState::Pending
            || format!("{:x}", Sha256::digest(token.as_bytes())) != ticket.token_hash
        {
            ticket.state = AssetUploadState::Failed;
            return respond_error(
                &mut stream,
                401,
                error_codes::ASSET_UPLOAD_UNAUTHORIZED,
                "upload token is invalid",
            );
        }
        if length.is_some_and(|n| n > ticket.expected_size_bytes || n > MAX_ASSET_UPLOAD_BYTES) {
            ticket.state = AssetUploadState::Failed;
            return respond_error(
                &mut stream,
                413,
                error_codes::ASSET_UPLOAD_SIZE_MISMATCH,
                "upload size does not match ticket",
            );
        }
        ticket.state = AssetUploadState::Uploading;
        (
            asset_upload_temp_path(&ticket.workspace_path, upload_id),
            workspace_assets_root(&ticket.workspace_path),
            ticket.relative_path.clone(),
            workspace_assets_root(&ticket.workspace_path).join(&ticket.relative_path),
            ticket.expected_size_bytes,
            ticket.public_path.clone(),
        )
    };
    let result = (|| -> std::io::Result<u64> {
        fs::create_dir_all(tmp.parent().unwrap())?;
        let mut file = File::create(&tmp)?;
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
    })();
    let ok = matches!(result, Ok(n) if n == expected);
    if ok {
        let moved = finalize_upload(&tmp, &asset_root, &relative_path, &final_path, upload_id);
        if moved.is_ok() {
            runtime
                .lock()
                .unwrap()
                .asset_upload_tickets
                .get_mut(upload_id)
                .map(|t| t.state = AssetUploadState::Completed);
            return respond(
                &mut stream,
                201,
                Some(&format!(r#"{{"path":"{public_path}"}}"#)),
            );
        }
    }
    let _ = fs::remove_file(&tmp);
    runtime
        .lock()
        .unwrap()
        .asset_upload_tickets
        .get_mut(upload_id)
        .map(|t| t.state = AssetUploadState::Failed);
    respond_error(
        &mut stream,
        400,
        error_codes::ASSET_UPLOAD_FAILED,
        "asset upload failed",
    )
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
    let (target, expected_length, mime_type) = {
        let mut core = runtime.lock().unwrap();
        core.expire_asset_downloads();
        let ticket = match core.asset_download_tickets.get_mut(download_id) {
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
        if ticket.state == AssetDownloadState::Expired {
            return respond_error(
                stream,
                410,
                error_codes::ASSET_DOWNLOAD_TICKET_EXPIRED,
                "download ticket has expired",
            );
        }
        if ticket.state != AssetDownloadState::Pending
            || format!("{:x}", Sha256::digest(token.as_bytes())) != ticket.token_hash
        {
            ticket.state = AssetDownloadState::Failed;
            return respond_error(
                stream,
                401,
                error_codes::ASSET_DOWNLOAD_UNAUTHORIZED,
                "download token is invalid",
            );
        }
        let relative = match ticket.public_path.strip_prefix('/') {
            Some(value)
                if !value.is_empty()
                    && !value.contains('\\')
                    && !value
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == "..") =>
            {
                value
            }
            _ => {
                ticket.state = AssetDownloadState::Failed;
                return respond_error(
                    stream,
                    400,
                    error_codes::ASSET_PATH_INVALID,
                    "asset path is invalid",
                );
            }
        };
        let root = workspace_assets_root(&ticket.workspace_path);
        let target = relative
            .split('/')
            .fold(root.clone(), |path, part| path.join(part));
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata)
                if metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() <= MAX_ASSET_UPLOAD_BYTES =>
            {
                metadata
            }
            _ => {
                ticket.state = AssetDownloadState::Failed;
                return respond_error(
                    stream,
                    404,
                    error_codes::ASSET_READ_FAILED,
                    "asset is unavailable",
                );
            }
        };
        let canonical_root = match root.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                ticket.state = AssetDownloadState::Failed;
                return respond_error(
                    stream,
                    404,
                    error_codes::ASSET_READ_FAILED,
                    "asset is unavailable",
                );
            }
        };
        let canonical_target = match target.canonicalize() {
            Ok(path) if path.starts_with(&canonical_root) => path,
            _ => {
                ticket.state = AssetDownloadState::Failed;
                return respond_error(
                    stream,
                    400,
                    error_codes::ASSET_PATH_INVALID,
                    "asset path is invalid",
                );
            }
        };
        ticket.state = AssetDownloadState::Downloading;
        let mime = match canonical_target
            .extension()
            .and_then(|part| part.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "md" | "csv" => "text/plain",
            "json" => "application/json",
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "glb" => "model/gltf-binary",
            _ => "application/octet-stream",
        }
        .to_string();
        (canonical_target, metadata.len(), mime)
    };
    let result = (|| -> std::io::Result<()> {
        let mut file = File::open(target)?;
        write!(stream, "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, PUT, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nContent-Type: {mime_type}\r\nContent-Length: {expected_length}\r\n\r\n")?;
        let mut remaining = expected_length;
        let mut buf = [0u8; 64 * 1024];
        while remaining > 0 {
            let limit = remaining.min(64 * 1024) as usize;
            let count = file.read(&mut buf[..limit])?;
            if count == 0 {
                break;
            }
            stream.write_all(&buf[..count])?;
            remaining -= count as u64;
        }
        if remaining != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "asset changed during read",
            ));
        }
        Ok(())
    })();
    runtime
        .lock()
        .unwrap()
        .asset_download_tickets
        .get_mut(download_id)
        .map(|ticket| {
            ticket.state = if result.is_ok() {
                AssetDownloadState::Completed
            } else {
                AssetDownloadState::Failed
            }
        });
    result
}

fn finalize_upload(
    tmp: &std::path::Path,
    asset_root: &std::path::Path,
    relative: &std::path::Path,
    target: &std::path::Path,
    upload_id: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(asset_root)?;
    let canonical_root = asset_root.canonicalize()?;
    let parent_relative = relative
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut parent = asset_root.to_path_buf();
    for component in parent_relative.components() {
        parent.push(component);
        if parent.exists() {
            let metadata = fs::symlink_metadata(&parent)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || !parent.canonicalize()?.starts_with(&canonical_root)
            {
                return Err(std::io::Error::other("asset parent is unsafe"));
            }
        } else {
            fs::create_dir(&parent)?;
            if !parent.canonicalize()?.starts_with(&canonical_root) {
                return Err(std::io::Error::other("asset parent escapes root"));
            }
        }
    }
    if let Ok(metadata) = fs::symlink_metadata(target) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::other("asset target is not a regular file"));
        }
        let backup = target.with_file_name(format!(".pedelec-{upload_id}.backup"));
        fs::rename(target, &backup)?;
        match fs::rename(tmp, target) {
            Ok(()) => {
                let _ = fs::remove_file(backup);
                Ok(())
            }
            Err(error) => {
                let _ = fs::rename(&backup, target);
                Err(error)
            }
        }
    } else {
        fs::rename(tmp, target)
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: Option<&str>) -> std::io::Result<()> {
    let body = body.unwrap_or("");
    write!(stream, "HTTP/1.1 {status} OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, PUT, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
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
    use chrono::Utc;
    use pedelec_core::{
        workspace_assets_root, workspace_tmp_root, AssetUploadState, CoreRuntime,
        CreateAssetUploadInput, EffortLevel, ProviderAdapterState, ProviderCode, ThreadState,
        ThreadStatus,
    };
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn upload_ticket_uses_private_asset_and_tmp_layout() {
        let temp = tempdir().unwrap();
        let workspace_path = temp.path().join("workspace");
        let thread_id = "thread_upload_layout".to_string();
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime.lock().unwrap().thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.clone(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: vec![],
                workspace_path: workspace_path.clone(),
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                sdk_origin: None,
            },
            ProviderAdapterState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );

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
                .asset_upload_tickets
                .get(&ticket.upload_id)
                .unwrap()
                .state,
            AssetUploadState::Completed
        );
    }
}
