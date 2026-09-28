use pedelec_shared::deno_release::DenoArtifact;
use pedelec_shared::paths::{managed_deno_partial_dir, managed_deno_runtime_dir};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zip::ZipArchive;

pub const APP_PREPARATION_EVENT: &str = "app-preparation-state";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPreparationState {
    Checking,
    Downloading {
        downloaded_bytes: u64,
        total_bytes: u64,
        progress_percent: u8,
    },
    Finalizing {
        progress_percent: u8,
    },
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPreparationSnapshot {
    pub attempt: u64,
    pub state: AppPreparationState,
}

impl Serialize for AppPreparationSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match &self.state {
            AppPreparationState::Checking => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("attempt", &self.attempt)?;
                map.serialize_entry("status", "checking")?;
                map.end()
            }
            AppPreparationState::Downloading {
                downloaded_bytes,
                total_bytes,
                progress_percent,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("attempt", &self.attempt)?;
                map.serialize_entry("status", "downloading")?;
                map.serialize_entry("downloadedBytes", downloaded_bytes)?;
                map.serialize_entry("totalBytes", total_bytes)?;
                map.serialize_entry("progressPercent", progress_percent)?;
                map.end()
            }
            AppPreparationState::Finalizing { progress_percent } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("attempt", &self.attempt)?;
                map.serialize_entry("status", "finalizing")?;
                map.serialize_entry("progressPercent", progress_percent)?;
                map.end()
            }
            AppPreparationState::Ready => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("attempt", &self.attempt)?;
                map.serialize_entry("status", "ready")?;
                map.end()
            }
            AppPreparationState::Failed => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("attempt", &self.attempt)?;
                map.serialize_entry("status", "failed")?;
                map.end()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProvisionError {
    message: String,
}

impl ProvisionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProvisionError {}

pub trait ArchiveDownload: Send + Sync {
    fn download_to(
        &self,
        url: &str,
        expected_size: u64,
        destination: &Path,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ProvisionError>;
}

pub struct ReqwestArchiveDownloader;

impl ArchiveDownload for ReqwestArchiveDownloader {
    fn download_to(
        &self,
        url: &str,
        expected_size: u64,
        destination: &Path,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ProvisionError> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(900))
            .build()
            .map_err(|err| ProvisionError::new(format!("cannot start runtime download: {err}")))?;
        let mut response = client
            .get(url)
            .header(reqwest::header::USER_AGENT, "pedelec")
            .send()
            .map_err(|err| ProvisionError::new(format!("runtime download failed: {err}")))?;
        if !response.status().is_success() {
            return Err(ProvisionError::new(format!(
                "runtime download failed with HTTP {}",
                response.status()
            )));
        }
        let content_length = response.content_length();
        ingest_archive(
            &mut response,
            content_length,
            expected_size,
            destination,
            on_progress,
        )
    }
}

pub fn download_percent(downloaded: u64, total: u64) -> u8 {
    if total == 0 || downloaded == 0 {
        return 0;
    }
    if downloaded >= total {
        return 100;
    }
    let percent = downloaded.saturating_mul(100) / total;
    u8::try_from(percent).unwrap_or(99).min(99)
}

pub fn ingest_archive<R: Read>(
    reader: &mut R,
    content_length: Option<u64>,
    expected_size: u64,
    destination: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), ProvisionError> {
    if expected_size == 0 {
        return Err(ProvisionError::new(
            "pinned archive size must be a positive integer",
        ));
    }
    if let Some(length) = content_length {
        if length != expected_size {
            return Err(ProvisionError::new(
                "archive content length does not match pinned size",
            ));
        }
    }

    let mut file = File::create(destination).map_err(io_error)?;
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        let next = downloaded.saturating_add(read as u64);
        if next > expected_size {
            drop(file);
            let _ = fs::remove_file(destination);
            return Err(ProvisionError::new(
                "archive is larger than the pinned size",
            ));
        }
        file.write_all(&buffer[..read]).map_err(io_error)?;
        downloaded = next;
        on_progress(downloaded, expected_size);
    }
    file.sync_all().map_err(io_error)?;
    drop(file);
    if downloaded != expected_size {
        let _ = fs::remove_file(destination);
        return Err(ProvisionError::new(
            "archive is smaller than the pinned size",
        ));
    }
    Ok(())
}

pub fn provision_managed_runtime(
    plan: &DenoArtifact,
    pedelec_home: &Path,
    attempt_id: &str,
    downloader: &dyn ArchiveDownload,
    on_state: &mut dyn FnMut(AppPreparationState),
) -> Result<PathBuf, ProvisionError> {
    validate_attempt_id(attempt_id)?;
    if plan.archive_size_bytes == 0 || !is_lowercase_sha256(&plan.archive_sha256) {
        return Err(ProvisionError::new("pinned runtime metadata is invalid"));
    }
    if plan.executable_name != "deno" && plan.executable_name != "deno.exe" {
        return Err(ProvisionError::new(
            "pinned runtime executable name is invalid",
        ));
    }

    let final_dir = managed_deno_runtime_dir(pedelec_home, &plan.version, &plan.target)
        .map_err(|err| ProvisionError::new(err.message))?;
    let final_executable = final_dir.join(&plan.executable_name);
    if final_executable.is_file() {
        return Ok(final_executable);
    }

    let partial_dir = managed_deno_partial_dir(pedelec_home, &plan.version, &plan.target)
        .map_err(|err| ProvisionError::new(err.message))?;
    let work_dir = partial_dir.join(attempt_id);
    if work_dir.exists() {
        fs::remove_dir_all(&work_dir).map_err(io_error)?;
    }
    fs::create_dir_all(&work_dir).map_err(io_error)?;

    let provisioned = provision_in_work(plan, &final_dir, &work_dir, downloader, on_state);
    let _ = fs::remove_dir_all(&work_dir);
    provisioned
}

fn provision_in_work(
    plan: &DenoArtifact,
    final_dir: &Path,
    work_dir: &Path,
    downloader: &dyn ArchiveDownload,
    on_state: &mut dyn FnMut(AppPreparationState),
) -> Result<PathBuf, ProvisionError> {
    let archive_path = work_dir.join("archive.zip");
    let mut last_percent = 0u8;
    downloader.download_to(
        &plan.url,
        plan.archive_size_bytes,
        &archive_path,
        &mut |downloaded, total| {
            let percent = download_percent(downloaded, total);
            if percent < last_percent || percent > 100 {
                return;
            }
            last_percent = percent;
            on_state(AppPreparationState::Downloading {
                downloaded_bytes: downloaded,
                total_bytes: total,
                progress_percent: percent,
            });
        },
    )?;

    let received = fs::metadata(&archive_path).map_err(io_error)?.len();
    if received != plan.archive_size_bytes {
        return Err(ProvisionError::new(
            "archive is not the pinned size after download",
        ));
    }
    on_state(AppPreparationState::Finalizing {
        progress_percent: 100,
    });

    let actual_sha256 = sha256_file(&archive_path)?;
    if actual_sha256 != plan.archive_sha256 {
        return Err(ProvisionError::new(
            "archive checksum does not match pinned metadata",
        ));
    }

    let stage_dir = work_dir.join("stage");
    fs::create_dir_all(&stage_dir).map_err(io_error)?;
    let staged_executable = stage_dir.join(&plan.executable_name);
    extract_root_executable(&archive_path, &plan.executable_name, &staged_executable)?;
    make_executable(&staged_executable)?;
    publish_runtime_dir(&stage_dir, final_dir, &plan.executable_name)
}

pub fn publish_runtime_dir(
    staging_dir: &Path,
    final_dir: &Path,
    executable_name: &str,
) -> Result<PathBuf, ProvisionError> {
    let _guard = publish_lock().lock().unwrap_or_else(|err| err.into_inner());
    let final_executable = final_dir.join(executable_name);
    if final_executable.is_file() {
        return Ok(final_executable);
    }
    if let Some(parent) = final_dir.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    if final_dir.exists() {
        fs::remove_dir_all(final_dir).map_err(io_error)?;
    }
    fs::rename(staging_dir, final_dir).map_err(io_error)?;
    if !final_executable.is_file() {
        return Err(ProvisionError::new("runtime executable was not published"));
    }
    Ok(final_executable)
}

fn extract_root_executable(
    archive_path: &Path,
    executable_name: &str,
    destination: &Path,
) -> Result<(), ProvisionError> {
    let file = File::open(archive_path).map_err(io_error)?;
    let mut archive =
        ZipArchive::new(file).map_err(|_| ProvisionError::new("archive is not a supported ZIP"))?;
    if archive.len() != 1 {
        return Err(ProvisionError::new(
            "archive does not contain the expected runtime executable",
        ));
    }
    let mut entry = archive
        .by_index(0)
        .map_err(|_| ProvisionError::new("archive is not a supported ZIP"))?;
    if entry.encrypted() {
        return Err(ProvisionError::new(
            "encrypted runtime archives are not supported",
        ));
    }
    let compression = entry.compression();
    if compression != zip::CompressionMethod::Stored
        && compression != zip::CompressionMethod::Deflated
    {
        return Err(ProvisionError::new(
            "runtime archive uses an unsupported compression method",
        ));
    }
    if entry.is_dir() || entry.name() != executable_name || entry.enclosed_name().is_none() {
        return Err(ProvisionError::new(
            "archive does not contain the expected runtime executable",
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    let mut output = File::create(destination).map_err(io_error)?;
    io::copy(&mut entry, &mut output).map_err(|_| {
        ProvisionError::new("archive does not contain the expected runtime executable")
    })?;
    output.sync_all().map_err(io_error)?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, ProvisionError> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn make_executable(path: &Path) -> Result<(), ProvisionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).map_err(io_error)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(io_error)?;
    }
    let _ = path;
    Ok(())
}

fn publish_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

fn validate_attempt_id(attempt_id: &str) -> Result<(), ProvisionError> {
    if attempt_id.is_empty()
        || attempt_id.contains(['/', '\\', '\0'])
        || attempt_id == "."
        || attempt_id == ".."
    {
        return Err(ProvisionError::new("invalid provisioning attempt"));
    }
    Ok(())
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn io_error(err: io::Error) -> ProvisionError {
    ProvisionError::new(err.to_string())
}

#[cfg(test)]
pub(crate) fn zip_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, bytes) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_shared::paths::{deno_executable_file_name, managed_deno_executable_path};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    struct MemoryDownload {
        body: Vec<u8>,
        content_length: Option<u64>,
        calls: AtomicUsize,
        failures_remaining: AtomicUsize,
    }

    impl MemoryDownload {
        fn new(body: Vec<u8>) -> Self {
            Self {
                body,
                content_length: None,
                calls: AtomicUsize::new(0),
                failures_remaining: AtomicUsize::new(0),
            }
        }
    }

    impl ArchiveDownload for MemoryDownload {
        fn download_to(
            &self,
            url: &str,
            expected_size: u64,
            destination: &Path,
            on_progress: &mut dyn FnMut(u64, u64),
        ) -> Result<(), ProvisionError> {
            assert!(!url.is_empty());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let failed = self
                .failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    current.checked_sub(1)
                })
                .is_ok();
            if failed {
                return Err(ProvisionError::new("simulated download failure"));
            }
            ingest_archive(
                &mut Cursor::new(self.body.as_slice()),
                self.content_length,
                expected_size,
                destination,
                on_progress,
            )
        }
    }

    struct PanicDownload;

    impl ArchiveDownload for PanicDownload {
        fn download_to(
            &self,
            _url: &str,
            _expected_size: u64,
            _destination: &Path,
            _on_progress: &mut dyn FnMut(u64, u64),
        ) -> Result<(), ProvisionError> {
            panic!("downloader invoked on the installed fast path");
        }
    }

    fn plan_for(body: &[u8], executable_name: &str) -> DenoArtifact {
        DenoArtifact {
            version: "2.9.5".to_string(),
            target: "x86_64-pc-windows-msvc".to_string(),
            platform: "win32".to_string(),
            artifact: "deno-x86_64-pc-windows-msvc.zip".to_string(),
            url: "https://github.com/denoland/deno/releases/download/v2.9.5/deno-x86_64-pc-windows-msvc.zip".to_string(),
            archive_sha256: sha256_bytes(body),
            archive_size_bytes: body.len() as u64,
            executable_name: executable_name.to_string(),
        }
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_encode(&hasher.finalize())
    }

    struct ChunkReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index];
            let size = chunk.len().min(buf.len());
            buf[..size].copy_from_slice(&chunk[..size]);
            self.index += 1;
            Ok(size)
        }
    }

    #[test]
    fn download_progress_is_monotonic_and_reaches_100_at_the_pinned_size() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("archive.zip");
        let chunks = vec![vec![1u8; 10], vec![2u8; 10], vec![3u8; 10], vec![4u8; 10]];
        let mut reader = ChunkReader { chunks, index: 0 };
        let mut seen = Vec::new();
        ingest_archive(
            &mut reader,
            None,
            40,
            &destination,
            &mut |downloaded, total| {
                seen.push((downloaded, download_percent(downloaded, total)));
            },
        )
        .unwrap();

        let mut previous = 0u8;
        for (downloaded, percent) in &seen {
            assert!(*percent >= previous);
            assert!(*percent <= 100);
            if *percent == 100 {
                assert_eq!(*downloaded, 40);
            }
            previous = *percent;
        }
        assert_eq!(seen.last().copied(), Some((40, 100)));
        assert_eq!(fs::metadata(&destination).unwrap().len(), 40);
    }

    #[test]
    fn conflicting_content_length_fails_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("archive.zip");
        let err = ingest_archive(
            &mut Cursor::new(vec![1, 2, 3, 4]),
            Some(8),
            4,
            &destination,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("content length"));
        assert!(!destination.exists());
    }

    #[test]
    fn archives_larger_or_smaller_than_the_pinned_size_fail() {
        let temp = tempfile::tempdir().unwrap();
        let larger = temp.path().join("larger.zip");
        let err = ingest_archive(
            &mut Cursor::new(vec![1, 2, 3, 4, 5]),
            None,
            4,
            &larger,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("larger"));
        assert!(!larger.exists());

        let smaller = temp.path().join("smaller.zip");
        let err = ingest_archive(
            &mut Cursor::new(vec![1, 2, 3]),
            None,
            4,
            &smaller,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("smaller"));
        assert!(!smaller.exists());
    }

    #[test]
    fn installed_runtime_returns_ready_without_invoking_the_downloader() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"already-installed")]);
        let plan = plan_for(&body, executable_name);
        let executable =
            managed_deno_executable_path(temp.path(), &plan.version, &plan.target, executable_name)
                .unwrap();
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"already-installed").unwrap();
        let mut states = Vec::new();
        let resolved =
            provision_managed_runtime(&plan, temp.path(), "1", &PanicDownload, &mut |state| {
                states.push(state)
            })
            .unwrap();
        assert_eq!(resolved, executable);
        assert!(states.is_empty());
        assert_eq!(fs::read(&executable).unwrap(), b"already-installed");
    }

    #[test]
    fn checksum_mismatch_does_not_publish_the_executable() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"runtime")]);
        let mut plan = plan_for(&body, executable_name);
        plan.archive_sha256 = "a".repeat(64);
        let downloader = MemoryDownload::new(body);
        let err = provision_managed_runtime(&plan, temp.path(), "1", &downloader, &mut |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("checksum"));
        let final_executable =
            managed_deno_executable_path(temp.path(), &plan.version, &plan.target, executable_name)
                .unwrap();
        assert!(!final_executable.exists());
        assert_eq!(downloader.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn malformed_archive_and_missing_executable_do_not_publish() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let garbage = b"this is not a zip archive".to_vec();
        let plan = plan_for(&garbage, executable_name);
        let err = provision_managed_runtime(
            &plan,
            temp.path(),
            "1",
            &MemoryDownload::new(garbage),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("ZIP"));

        let nested = zip_archive(&[(&format!("nested/{executable_name}"), b"runtime")]);
        let plan = plan_for(&nested, executable_name);
        let err = provision_managed_runtime(
            &plan,
            temp.path(),
            "2",
            &MemoryDownload::new(nested),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected runtime executable"));
        let final_executable =
            managed_deno_executable_path(temp.path(), &plan.version, &plan.target, executable_name)
                .unwrap();
        assert!(!final_executable.exists());
    }

    #[test]
    fn successful_install_publishes_the_versioned_executable() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"managed-runtime")]);
        let plan = plan_for(&body, executable_name);
        let mut states = Vec::new();
        let installed = provision_managed_runtime(
            &plan,
            temp.path(),
            "7",
            &MemoryDownload::new(body),
            &mut |state| states.push(state),
        )
        .unwrap();
        let expected =
            managed_deno_executable_path(temp.path(), &plan.version, &plan.target, executable_name)
                .unwrap();
        assert_eq!(installed, expected);
        assert_eq!(fs::read(&installed).unwrap(), b"managed-runtime");
        assert!(states.iter().any(|state| matches!(
            state,
            AppPreparationState::Downloading {
                progress_percent: 100,
                ..
            }
        )));
        assert!(states.iter().any(|state| matches!(
            state,
            AppPreparationState::Finalizing {
                progress_percent: 100
            }
        )));
        let percents: Vec<u8> = states
            .iter()
            .filter_map(|state| match state {
                AppPreparationState::Downloading {
                    progress_percent, ..
                } => Some(*progress_percent),
                _ => None,
            })
            .collect();
        let mut previous = 0u8;
        for percent in percents {
            assert!(percent >= previous && percent <= 100);
            previous = percent;
        }
    }

    #[cfg(unix)]
    #[test]
    fn published_executable_is_marked_executable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let executable_name = "deno";
        let body = zip_archive(&[(executable_name, b"#!/bin/sh\n")]);
        let plan = plan_for(&body, executable_name);
        let installed = provision_managed_runtime(
            &plan,
            temp.path(),
            "1",
            &MemoryDownload::new(body),
            &mut |_| {},
        )
        .unwrap();
        let mode = fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[test]
    fn retry_can_succeed_after_a_failed_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"retried-runtime")]);
        let plan = plan_for(&body, executable_name);
        let downloader = MemoryDownload::new(body);
        downloader.failures_remaining.store(1, Ordering::SeqCst);
        let first = provision_managed_runtime(&plan, temp.path(), "1", &downloader, &mut |_| {});
        assert!(first.is_err());
        let installed =
            provision_managed_runtime(&plan, temp.path(), "2", &downloader, &mut |_| {}).unwrap();
        assert_eq!(fs::read(installed).unwrap(), b"retried-runtime");
        assert_eq!(downloader.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_attempt_preserves_a_completed_runtime_for_another_version() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let previous = managed_deno_executable_path(
            temp.path(),
            "1.0.0",
            "x86_64-pc-windows-msvc",
            executable_name,
        )
        .unwrap();
        fs::create_dir_all(previous.parent().unwrap()).unwrap();
        fs::write(&previous, b"previous-runtime").unwrap();

        let body = b"not-a-zip".to_vec();
        let plan = plan_for(&body, executable_name);
        let err = provision_managed_runtime(
            &plan,
            temp.path(),
            "1",
            &MemoryDownload::new(body),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("ZIP"));
        assert_eq!(fs::read(&previous).unwrap(), b"previous-runtime");
    }

    #[test]
    fn concurrent_publication_keeps_a_single_valid_executable() {
        let temp = tempfile::tempdir().unwrap();
        let final_dir = temp
            .path()
            .join("runtimes")
            .join("deno")
            .join("2.9.5")
            .join("target");
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        fs::write(left.join("deno.exe"), b"runtime-ok").unwrap();
        fs::write(right.join("deno.exe"), b"runtime-ok").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let left_final = final_dir.clone();
        let right_final = final_dir.clone();
        let left_barrier = Arc::clone(&barrier);
        let left_handle = thread::spawn(move || {
            left_barrier.wait();
            publish_runtime_dir(&left, &left_final, "deno.exe").unwrap()
        });
        let right_handle = thread::spawn(move || {
            barrier.wait();
            publish_runtime_dir(&right, &right_final, "deno.exe").unwrap()
        });
        let left_path = left_handle.join().unwrap();
        let right_path = right_handle.join().unwrap();
        assert_eq!(left_path, right_path);
        assert_eq!(fs::read(&left_path).unwrap(), b"runtime-ok");
    }

    #[test]
    fn preparation_snapshot_stays_generic() {
        let snapshot = AppPreparationSnapshot {
            attempt: 3,
            state: AppPreparationState::Downloading {
                downloaded_bytes: 42,
                total_bytes: 100,
                progress_percent: 42,
            },
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["status"], "downloading");
        assert_eq!(json["downloadedBytes"], 42);
        assert_eq!(json["totalBytes"], 100);
        assert_eq!(json["progressPercent"], 42);
        let rendered = serde_json::to_string(&snapshot).unwrap();
        let lowered = rendered.to_ascii_lowercase();
        assert!(!lowered.contains("deno"));
        assert!(!lowered.contains("http"));
        assert!(!rendered.contains("sha"));
        assert!(
            !rendered.contains('\\') && !rendered.contains("/Users") && !rendered.contains("/home")
        );
    }

    #[test]
    fn percent_never_reports_100_before_the_final_byte() {
        assert_eq!(download_percent(0, 10), 0);
        assert_eq!(download_percent(9, 10), 90);
        assert_eq!(download_percent(199, 200), 99);
        assert_eq!(download_percent(200, 200), 100);
        assert!(download_percent(u64::MAX, u64::MAX) <= 100);
    }
}
