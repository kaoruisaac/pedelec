//! Loopback-only binary asset data plane.  The control plane only creates tickets.
use crate::pedelec_core::{
    error_codes, AssetUploadState, PedelecError, SharedCoreRuntime, MAX_ASSET_UPLOAD_BYTES,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

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
    let (tmp, final_path, expected) = {
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
            ticket
                .sandbox_path
                .join("tmp")
                .join(format!("{upload_id}.upload")),
            ticket
                .sandbox_path
                .join("input")
                .join(format!("{upload_id}-{}", ticket.safe_filename)),
            ticket.expected_size_bytes,
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
        let moved = fs::rename(&tmp, &final_path);
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
                Some(&format!(
                    r#"{{"path":"input/{}"}}"#,
                    final_path.file_name().unwrap().to_string_lossy()
                )),
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
        error_codes::ASSET_UPLOAD_SIZE_MISMATCH,
        "upload did not match the expected size",
    )
}

fn respond(stream: &mut TcpStream, status: u16, body: Option<&str>) -> std::io::Result<()> {
    let body = body.unwrap_or("");
    write!(stream, "HTTP/1.1 {status} OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: PUT, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
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
