use pedelec_core::{
    error_codes, workspace_runtime_data_root, DenoExecutionIntent, DenoRunOutput,
    DenoRuntimeDispatcher, PedelecError,
};
use pedelec_shared::paths::path_for_external_use;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const DEFAULT_DENO_EXECUTION_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_DENO_STDOUT_CAP_BYTES: usize = 1024 * 1024;
pub const DEFAULT_DENO_STDERR_CAP_BYTES: usize = 1024 * 1024;

/// Fixed Pedelec-owned limits for one Deno invocation.  The public command
/// contract intentionally has no way to override these values; the policy is
/// injectable only for deterministic Desktop/runtime tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenoRuntimePolicy {
    pub execution_timeout: Duration,
    pub stdout_cap_bytes: usize,
    pub stderr_cap_bytes: usize,
}

impl Default for DenoRuntimePolicy {
    fn default() -> Self {
        Self {
            execution_timeout: DEFAULT_DENO_EXECUTION_TIMEOUT,
            stdout_cap_bytes: DEFAULT_DENO_STDOUT_CAP_BYTES,
            stderr_cap_bytes: DEFAULT_DENO_STDERR_CAP_BYTES,
        }
    }
}

#[derive(Debug)]
struct ActiveDenoRun {
    child: Mutex<Option<Child>>,
    cancel_requested: AtomicBool,
}

impl Drop for ActiveDenoRun {
    fn drop(&mut self) {
        if let Ok(mut child_slot) = self.child.lock() {
            if let Some(child) = child_slot.as_mut() {
                let _ = terminate_child(child);
            }
        }
    }
}

#[derive(Debug)]
struct DenoRuntimeInner {
    executable_path: Mutex<PathBuf>,
    policy: DenoRuntimePolicy,
    active_runs: Mutex<HashMap<String, Arc<ActiveDenoRun>>>,
    shutting_down: AtomicBool,
}

/// Desktop-owned raw-Deno process owner.  It has no dependency on the App
/// Tool broker and maintains one active child per Pedelec thread.
#[derive(Debug, Clone)]
pub struct DenoRuntimeOwner {
    inner: Arc<DenoRuntimeInner>,
}

impl DenoRuntimeOwner {
    pub fn new(executable_path: impl Into<PathBuf>) -> Self {
        Self::with_policy(executable_path, DenoRuntimePolicy::default())
    }

    pub fn with_policy(executable_path: impl Into<PathBuf>, policy: DenoRuntimePolicy) -> Self {
        Self {
            inner: Arc::new(DenoRuntimeInner {
                executable_path: Mutex::new(executable_path.into()),
                policy,
                active_runs: Mutex::new(HashMap::new()),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// Lets Desktop resolve a Tauri resource after the application handle is
    /// available while keeping the owner constructible in tests first.
    pub fn set_executable_path(&self, executable_path: impl Into<PathBuf>) {
        if let Ok(mut path) = self.inner.executable_path.lock() {
            *path = executable_path.into();
        }
    }

    pub fn executable_path(&self) -> PathBuf {
        self.inner
            .executable_path
            .lock()
            .map(|path| path.clone())
            .unwrap_or_default()
    }

    pub fn active_run_count(&self) -> usize {
        self.inner
            .active_runs
            .lock()
            .map(|runs| runs.len())
            .unwrap_or_default()
    }

    pub fn is_thread_active(&self, thread_id: &str) -> bool {
        self.inner
            .active_runs
            .lock()
            .map(|runs| runs.contains_key(thread_id))
            .unwrap_or(false)
    }

    pub fn dispatch(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
        self.dispatch_intent(intent)
    }

    /// Requests and waits for one thread's child to terminate.  The
    /// dispatcher remains the owner of the registry entry until its own
    /// terminal path has collected output and returned.
    pub fn cancel_thread(&self, thread_id: &str) {
        let run = self
            .inner
            .active_runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(thread_id).cloned());
        if let Some(run) = run {
            run.cancel_requested.store(true, Ordering::Release);
            let _ = terminate_active_child(&run);
        }
    }

    /// Stops every active child.  The owner is permanently closed after this
    /// call because it belongs to one Desktop application lifetime.
    pub fn shutdown_all(&self) -> Vec<String> {
        self.inner.shutting_down.store(true, Ordering::Release);
        let runs = self
            .inner
            .active_runs
            .lock()
            .map(|runs| runs.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut errors = Vec::new();
        for run in &runs {
            run.cancel_requested.store(true, Ordering::Release);
            if let Some(error) = terminate_active_child(run) {
                errors.push(error);
            }
        }

        if let Ok(mut active_runs) = self.inner.active_runs.lock() {
            active_runs.retain(|_, active| !runs.iter().any(|run| Arc::ptr_eq(run, active)));
        }
        errors
    }

    fn dispatch_intent(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
        let run = self.reserve_run(&intent.thread_id)?;
        let _registration = ActiveRunRegistration {
            owner: self.clone(),
            thread_id: intent.thread_id.clone(),
            run: Arc::clone(&run),
        };

        let executable_path = self.executable_path();
        validate_executable_path(&executable_path, &intent.thread_id)?;
        let prepared = PreparedDenoExecution::prepare(&intent)?;
        let command_args = build_deno_command_args(&prepared);

        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(deno_unavailable_error(
                &intent.thread_id,
                &executable_path,
                "Deno runtime owner is shutting down",
            ));
        }

        let mut command = Command::new(&executable_path);
        command
            .args(command_args)
            .current_dir(prepared.workspace())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Do not inherit arbitrary host variables.  These are runtime
            // implementation values, not script-visible host configuration.
            .env_clear()
            .env("DENO_DIR", prepared.cache_dir())
            .env("DENO_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1");
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command.spawn().map_err(|err| {
            PedelecError::with_details(
                error_codes::DENO_PROCESS_SPAWN_FAILED,
                "could not start the configured Deno runtime",
                serde_json::json!({
                    "threadId": intent.thread_id,
                    "executablePath": executable_path,
                    "error": err.to_string(),
                }),
            )
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            terminate_child_best_effort(&mut child);
            PedelecError::new(
                error_codes::DENO_PROCESS_IO_FAILED,
                "Deno stdout pipe was not available",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            terminate_child_best_effort(&mut child);
            PedelecError::new(
                error_codes::DENO_PROCESS_IO_FAILED,
                "Deno stderr pipe was not available",
            )
        })?;

        let stdout_rx = spawn_capture(stdout, self.inner.policy.stdout_cap_bytes);
        let stderr_rx = spawn_capture(stderr, self.inner.policy.stderr_cap_bytes);
        {
            let mut child_slot = run.child.lock().map_err(|_| {
                terminate_child_best_effort(&mut child);
                PedelecError::new(
                    error_codes::DENO_PROCESS_IO_FAILED,
                    "Deno runtime child state was poisoned",
                )
            })?;
            *child_slot = Some(child);
        }

        if run.cancel_requested.load(Ordering::Acquire) {
            let _ = terminate_active_child(&run);
        }

        let started_at = Instant::now();
        let exit_status = loop {
            if run.cancel_requested.load(Ordering::Acquire) {
                let _ = terminate_active_child(&run);
                let capture = collect_captures(stdout_rx, stderr_rx);
                return Err(cancelled_error(&intent.thread_id, capture));
            }
            if self.inner.shutting_down.load(Ordering::Acquire) {
                let _ = terminate_active_child(&run);
                let capture = collect_captures(stdout_rx, stderr_rx);
                return Err(cancelled_error(&intent.thread_id, capture));
            }

            let status = {
                let mut child_slot = run.child.lock().map_err(|_| {
                    PedelecError::new(
                        error_codes::DENO_PROCESS_IO_FAILED,
                        "Deno runtime child state was poisoned",
                    )
                })?;
                match child_slot.as_mut() {
                    Some(child) => child.try_wait().map_err(|err| {
                        PedelecError::with_details(
                            error_codes::DENO_PROCESS_IO_FAILED,
                            "could not poll the Deno runtime process",
                            serde_json::json!({ "error": err.to_string() }),
                        )
                    })?,
                    None => None,
                }
            };

            if let Some(status) = status {
                break status;
            }

            if started_at.elapsed() >= self.inner.policy.execution_timeout {
                let _ = terminate_active_child(&run);
                let capture = collect_captures(stdout_rx, stderr_rx);
                return Err(timeout_error(&intent.thread_id, capture));
            }
            thread::sleep(Duration::from_millis(10));
        };

        let capture = collect_captures(stdout_rx, stderr_rx).map_err(|error| {
            PedelecError::with_details(
                error_codes::DENO_PROCESS_IO_FAILED,
                "could not collect Deno process output",
                serde_json::json!({ "threadId": intent.thread_id, "error": error }),
            )
        })?;

        Ok(DenoRunOutput {
            exit_code: exit_status.code().unwrap_or(-1),
            stdout: capture.stdout,
            stderr: capture.stderr,
            stdout_truncated: capture.stdout_truncated,
            stderr_truncated: capture.stderr_truncated,
        })
    }

    fn reserve_run(&self, thread_id: &str) -> Result<Arc<ActiveDenoRun>, PedelecError> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(PedelecError::new(
                error_codes::DENO_RUNTIME_UNAVAILABLE,
                "Deno runtime owner is shutting down",
            ));
        }
        let mut active_runs = self.inner.active_runs.lock().map_err(|_| {
            PedelecError::new(
                error_codes::DENO_PROCESS_IO_FAILED,
                "Deno active-run registry was poisoned",
            )
        })?;
        if active_runs.contains_key(thread_id) {
            return Err(PedelecError::with_details(
                error_codes::DENO_EXECUTION_BUSY,
                "a Deno execution is already active for this thread",
                serde_json::json!({ "threadId": thread_id }),
            ));
        }
        let run = Arc::new(ActiveDenoRun {
            child: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
        });
        active_runs.insert(thread_id.to_string(), Arc::clone(&run));
        Ok(run)
    }
}

impl DenoRuntimeDispatcher for DenoRuntimeOwner {
    fn dispatch(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
        self.dispatch_intent(intent)
    }

    fn cancel_thread(&self, thread_id: &str) {
        self.cancel_thread(thread_id);
    }

    fn shutdown(&self) {
        let _ = self.shutdown_all();
    }
}

struct ActiveRunRegistration {
    owner: DenoRuntimeOwner,
    thread_id: String,
    run: Arc<ActiveDenoRun>,
}

impl Drop for ActiveRunRegistration {
    fn drop(&mut self) {
        if let Ok(mut active_runs) = self.owner.inner.active_runs.lock() {
            if active_runs
                .get(&self.thread_id)
                .is_some_and(|active| Arc::ptr_eq(active, &self.run))
            {
                active_runs.remove(&self.thread_id);
            }
        }
    }
}

/// One Deno execution resolved to a single canonical workspace root.
///
/// Every filesystem-sensitive execution value - sandbox permission flags,
/// process cwd, the executed entrypoint, and the Pedelec-owned Deno cache -
/// is derived from `workspace`, so no two aliases of the same directory (for
/// example macOS `/var` versus `/private/var`) can be mixed inside one
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDenoExecution {
    workspace: PathBuf,
    entrypoint: PathBuf,
    cache_dir: PathBuf,
    args: Vec<String>,
}

impl PreparedDenoExecution {
    /// Resolves the canonical workspace root once and validates every other
    /// path against it.  Caller-provided script arguments are carried through
    /// unchanged; they only ever become script arguments.
    pub fn prepare(intent: &DenoExecutionIntent) -> Result<Self, PedelecError> {
        let workspace = canonical_execution_path(&intent.workspace_path, "Deno workspace")?;
        if !workspace.is_dir() {
            return Err(PedelecError::with_details(
                error_codes::DENO_RUNTIME_UNAVAILABLE,
                "the Deno workspace is not a directory",
                serde_json::json!({ "path": path_for_external_use(&workspace) }),
            ));
        }

        let entrypoint = canonical_execution_path(&intent.entrypoint, "Deno entrypoint")?;
        if !entrypoint.starts_with(&workspace) {
            return Err(PedelecError::with_details(
                error_codes::DENO_ENTRYPOINT_INVALID,
                "Deno entrypoint resolves outside the canonical workspace",
                serde_json::json!({
                    "threadId": intent.thread_id,
                    "path": path_for_external_use(&entrypoint),
                    "workspacePath": path_for_external_use(&workspace),
                }),
            ));
        }
        if !entrypoint.is_file() {
            return Err(PedelecError::with_details(
                error_codes::DENO_ENTRYPOINT_INVALID,
                "Deno entrypoint must resolve to a regular file",
                serde_json::json!({
                    "threadId": intent.thread_id,
                    "path": path_for_external_use(&entrypoint),
                }),
            ));
        }

        let cache_dir = prepare_deno_cache_dir(&workspace)?;
        Ok(Self {
            workspace,
            entrypoint,
            cache_dir,
            args: intent.args.clone(),
        })
    }

    /// The single canonical workspace root backing this execution.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The canonical entrypoint, always inside [`Self::workspace`].
    pub fn entrypoint(&self) -> &Path {
        &self.entrypoint
    }

    /// The Pedelec-owned Deno cache, always inside [`Self::workspace`].
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

/// Builds the only raw-Deno argv accepted by the Desktop owner.  The first
/// `--` ends Deno option parsing before the authoritative script path; every
/// caller-provided argument is appended after the script path and therefore
/// cannot become a permission/runtime option.
pub fn build_deno_command_args(prepared: &PreparedDenoExecution) -> Vec<OsString> {
    let workspace = path_for_external_use(prepared.workspace());
    let mut args = vec![
        OsString::from("run"),
        OsString::from("--no-prompt"),
        OsString::from("--no-config"),
        OsString::from("--no-remote"),
        OsString::from("--cached-only"),
        OsString::from("--no-npm"),
        OsString::from(format!("--allow-read={workspace}")),
        OsString::from(format!("--allow-write={workspace}")),
        OsString::from("--deny-net"),
        OsString::from("--deny-env"),
        OsString::from("--deny-run"),
        OsString::from("--deny-ffi"),
        OsString::from("--deny-sys"),
        OsString::from("--"),
        prepared.entrypoint().as_os_str().to_os_string(),
    ];
    args.extend(prepared.args.iter().cloned().map(OsString::from));
    args
}

/// Canonicalizes one execution path and returns it in the form Deno itself
/// reports for that file.  On Windows `canonicalize` yields a verbatim
/// `\\?\` path, which would not match the plain-drive paths Deno compares
/// permission prefixes against, so the verbatim prefix is removed again.
fn canonical_execution_path(path: &Path, label: &str) -> Result<PathBuf, PedelecError> {
    let canonical = path.canonicalize().map_err(|err| {
        PedelecError::with_details(
            error_codes::DENO_RUNTIME_UNAVAILABLE,
            format!("cannot canonicalize the {label}"),
            serde_json::json!({
                "path": path_for_external_use(path),
                "error": err.to_string(),
            }),
        )
    })?;
    Ok(PathBuf::from(path_for_external_use(&canonical)))
}

fn validate_executable_path(path: &Path, thread_id: &str) -> Result<(), PedelecError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(deno_unavailable_error(
            thread_id,
            path,
            "configured Deno executable is unavailable",
        ));
    }
    Ok(())
}

/// Creates and validates the Pedelec-owned Deno cache.  `canonical_workspace`
/// must already be the canonical execution root so that the containment check
/// below compares two paths in the same form.
fn prepare_deno_cache_dir(canonical_workspace: &Path) -> Result<PathBuf, PedelecError> {
    let runtime_root = workspace_runtime_data_root(canonical_workspace);
    ensure_owned_directory(&runtime_root, "Pedelec runtime data root")?;
    let cache_dir = runtime_root.join("deno");
    ensure_owned_directory(&cache_dir, "Pedelec-owned Deno cache directory")?;
    let canonical_cache = canonical_execution_path(&cache_dir, "Pedelec-owned Deno cache")?;
    if !canonical_cache.starts_with(canonical_workspace) {
        return Err(PedelecError::new(
            error_codes::DENO_RUNTIME_UNAVAILABLE,
            "Pedelec-owned Deno cache resolves outside the workspace",
        ));
    }
    Ok(canonical_cache)
}

fn ensure_owned_directory(path: &Path, label: &str) -> Result<(), PedelecError> {
    let invalid = |reason: &str| {
        PedelecError::with_details(
            error_codes::DENO_RUNTIME_UNAVAILABLE,
            format!("{label} is not a safe directory"),
            serde_json::json!({ "path": path, "reason": reason }),
        )
    };

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(invalid("symbolic links are not accepted"));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid("path is not a directory"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                PedelecError::with_details(
                    error_codes::DENO_RUNTIME_UNAVAILABLE,
                    format!("cannot create {label}"),
                    serde_json::json!({ "path": path, "error": error.to_string() }),
                )
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                PedelecError::with_details(
                    error_codes::DENO_RUNTIME_UNAVAILABLE,
                    format!("cannot inspect {label}"),
                    serde_json::json!({ "path": path, "error": error.to_string() }),
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid("path changed to a non-directory entry"));
            }
        }
        Err(error) => {
            return Err(PedelecError::with_details(
                error_codes::DENO_RUNTIME_UNAVAILABLE,
                format!("cannot inspect {label}"),
                serde_json::json!({ "path": path, "error": error.to_string() }),
            ));
        }
    }
    Ok(())
}

fn deno_unavailable_error(
    thread_id: &str,
    executable_path: &Path,
    message: &'static str,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::DENO_RUNTIME_UNAVAILABLE,
        message,
        serde_json::json!({
            "threadId": thread_id,
            "executablePath": executable_path,
        }),
    )
}

#[derive(Debug)]
struct CapturedOutput {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Debug)]
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

fn spawn_capture<R>(mut reader: R, cap: usize) -> Receiver<io::Result<CapturedStream>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
        let mut truncated = false;
        let mut buffer = [0u8; 16 * 1024];
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(CapturedStream { bytes, truncated }),
                Ok(read) => {
                    if bytes.len() < cap {
                        let remaining = cap - bytes.len();
                        let retained = read.min(remaining);
                        bytes.extend_from_slice(&buffer[..retained]);
                        if retained < read {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let _ = tx.send(result);
    });
    rx
}

fn collect_captures(
    stdout_rx: Receiver<io::Result<CapturedStream>>,
    stderr_rx: Receiver<io::Result<CapturedStream>>,
) -> Result<CapturedOutput, String> {
    const CAPTURE_COMPLETION_GRACE: Duration = Duration::from_secs(1);
    let stdout = stdout_rx
        .recv_timeout(CAPTURE_COMPLETION_GRACE)
        .map_err(|_| "stdout capture thread did not finish".to_string())?
        .map_err(|err| err.to_string())?;
    let stderr = stderr_rx
        .recv_timeout(CAPTURE_COMPLETION_GRACE)
        .map_err(|_| "stderr capture thread did not finish".to_string())?
        .map_err(|err| err.to_string())?;
    Ok(CapturedOutput {
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn timeout_error(thread_id: &str, capture: Result<CapturedOutput, String>) -> PedelecError {
    let mut details = serde_json::json!({ "threadId": thread_id });
    if let Ok(capture) = capture {
        details["stdout"] = serde_json::json!(capture.stdout);
        details["stderr"] = serde_json::json!(capture.stderr);
        details["stdoutTruncated"] = serde_json::json!(capture.stdout_truncated);
        details["stderrTruncated"] = serde_json::json!(capture.stderr_truncated);
    }
    PedelecError::with_details(
        error_codes::DENO_EXECUTION_TIMEOUT,
        "Deno execution timed out",
        details,
    )
}

fn cancelled_error(thread_id: &str, capture: Result<CapturedOutput, String>) -> PedelecError {
    let mut details = serde_json::json!({ "threadId": thread_id });
    if let Ok(capture) = capture {
        details["stdout"] = serde_json::json!(capture.stdout);
        details["stderr"] = serde_json::json!(capture.stderr);
        details["stdoutTruncated"] = serde_json::json!(capture.stdout_truncated);
        details["stderrTruncated"] = serde_json::json!(capture.stderr_truncated);
    }
    PedelecError::with_details(
        error_codes::DENO_EXECUTION_CANCELLED,
        "Deno execution was cancelled",
        details,
    )
}

fn terminate_active_child(run: &ActiveDenoRun) -> Option<String> {
    let mut child_slot = run.child.lock().ok()?;
    let child = child_slot.as_mut()?;
    terminate_child(child)
        .err()
        .map(|error| format!("could not terminate Deno child: {error}"))
}

fn terminate_child_best_effort(child: &mut Child) {
    let _ = terminate_child(child);
}

fn terminate_child(child: &mut Child) -> io::Result<ExitStatus> {
    match child.try_wait()? {
        Some(status) => Ok(status),
        None => {
            child.kill()?;
            child.wait()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_core::DenoExecutionIntent;
    use std::fs;
    use std::sync::mpsc;

    /// Every intent used here points at a real entrypoint because the runtime
    /// resolves and contains the script before spawning Deno.
    fn intent(workspace: &Path, entrypoint: &str, args: Vec<String>) -> DenoExecutionIntent {
        let entrypoint_path = workspace.join(entrypoint);
        if !entrypoint_path.exists() {
            if let Some(parent) = entrypoint_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&entrypoint_path, "// test entrypoint\n").unwrap();
        }
        DenoExecutionIntent {
            thread_id: "thread-deno-test".into(),
            workspace_path: workspace.to_path_buf(),
            entrypoint: entrypoint_path,
            args,
        }
    }

    fn prepared(workspace: &Path, entrypoint: &str, args: Vec<String>) -> PreparedDenoExecution {
        PreparedDenoExecution::prepare(&intent(workspace, entrypoint, args)).unwrap()
    }

    fn canonical(path: &Path) -> PathBuf {
        PathBuf::from(path_for_external_use(&path.canonicalize().unwrap()))
    }

    fn permission_value(args: &[OsString], flag: &str) -> String {
        args.iter()
            .find_map(|arg| {
                arg.to_string_lossy()
                    .strip_prefix(&format!("{flag}="))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("missing {flag}"))
    }

    #[test]
    fn command_args_put_caller_values_after_the_runtime_separator() {
        let temp = tempfile::tempdir().unwrap();
        let args = build_deno_command_args(&prepared(
            temp.path(),
            "script.ts",
            vec!["--allow-net".into(), "value".into()],
        ));
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert!(args[..separator].iter().all(|arg| arg != "--allow-net"));
        assert_eq!(
            args[separator + 1],
            canonical(&temp.path().join("script.ts")).as_os_str()
        );
        assert_eq!(args[separator + 2], "--allow-net");
        for fixed_flag in [
            "--no-prompt",
            "--no-config",
            "--no-remote",
            "--cached-only",
            "--no-npm",
            "--deny-net",
            "--deny-env",
            "--deny-run",
            "--deny-ffi",
            "--deny-sys",
        ] {
            assert!(
                args[..separator].iter().any(|arg| arg == fixed_flag),
                "missing fixed flag: {fixed_flag}"
            );
        }
    }

    #[test]
    fn output_capture_is_bounded_and_marks_truncation() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(CapturedStream {
            bytes: b"012345".to_vec(),
            truncated: true,
        }))
        .unwrap();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        stderr_tx
            .send(Ok(CapturedStream {
                bytes: b"err".to_vec(),
                truncated: false,
            }))
            .unwrap();
        let output = collect_captures(rx, stderr_rx).unwrap();
        assert_eq!(output.stdout, "012345");
        assert!(output.stdout_truncated);
        assert_eq!(output.stderr, "err");
    }

    #[test]
    fn owner_reports_missing_executable_as_runtime_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let owner = DenoRuntimeOwner::new(temp.path().join("missing-deno"));
        let error = owner
            .dispatch_intent(intent(temp.path(), "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_RUNTIME_UNAVAILABLE);
        assert_eq!(owner.active_run_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn fake_executable_captures_output_and_uses_workspace_cwd() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-deno.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'cwd=%s\\n' \"$PWD\"\nprintf 'args=%s\\n' \"$*\"\nprintf 'stderr\\n' >&2\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_secs(2),
                stdout_cap_bytes: 4096,
                stderr_cap_bytes: 4096,
            },
        );
        let output = owner
            .dispatch_intent(intent(
                temp.path(),
                "script.ts",
                vec!["--allow-net".into(), "hello".into()],
            ))
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output
            .stdout
            .contains(&canonical(temp.path()).to_string_lossy().into_owned()));
        assert!(output.stdout.contains("--allow-net"));
        assert!(output.stderr.contains("stderr"));
        assert_eq!(owner.active_run_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn fake_executable_returns_non_zero_exit_and_bounded_streams() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-deno-output.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '0123456789'\nprintf 'abcdefghij' >&2\nexit 7\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_secs(2),
                stdout_cap_bytes: 5,
                stderr_cap_bytes: 5,
            },
        );

        let output = owner
            .dispatch_intent(intent(temp.path(), "script.ts", Vec::new()))
            .unwrap();
        assert_eq!(output.exit_code, 7);
        assert_eq!(output.stdout, "01234");
        assert_eq!(output.stderr, "abcde");
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
        assert_eq!(owner.active_run_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_failure_releases_the_active_run_registry() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("not-executable-deno");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_secs(2),
                stdout_cap_bytes: 1024,
                stderr_cap_bytes: 1024,
            },
        );

        let error = owner
            .dispatch_intent(intent(temp.path(), "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_PROCESS_SPAWN_FAILED);
        assert_eq!(owner.active_run_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancel_release_the_registry() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("sleep-deno.sh");
        fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_millis(100),
                stdout_cap_bytes: 1024,
                stderr_cap_bytes: 1024,
            },
        );
        let error = owner
            .dispatch_intent(intent(temp.path(), "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_EXECUTION_TIMEOUT);
        assert_eq!(owner.active_run_count(), 0);

        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_secs(5),
                stdout_cap_bytes: 1024,
                stderr_cap_bytes: 1024,
            },
        );
        let thread_owner = owner.clone();
        let workspace = temp.path().to_path_buf();
        let handle = thread::spawn(move || {
            thread_owner.dispatch_intent(intent(&workspace, "script.ts", Vec::new()))
        });
        for _ in 0..100 {
            if owner.is_thread_active("thread-deno-test") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let busy = owner
            .dispatch_intent(intent(temp.path(), "script.ts", Vec::new()))
            .unwrap_err();
        assert_eq!(busy.code, error_codes::DENO_EXECUTION_BUSY);
        owner.cancel_thread("thread-deno-test");
        let error = handle.join().unwrap().unwrap_err();
        assert_eq!(error.code, error_codes::DENO_EXECUTION_CANCELLED);
        assert_eq!(owner.active_run_count(), 0);

        let owner = DenoRuntimeOwner::with_policy(
            &script,
            DenoRuntimePolicy {
                execution_timeout: Duration::from_secs(5),
                stdout_cap_bytes: 1024,
                stderr_cap_bytes: 1024,
            },
        );
        let shutdown_owner = owner.clone();
        let workspace = temp.path().to_path_buf();
        let handle = thread::spawn(move || {
            shutdown_owner.dispatch_intent(intent(&workspace, "script.ts", Vec::new()))
        });
        for _ in 0..100 {
            if owner.is_thread_active("thread-deno-test") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(owner.shutdown_all().is_empty());
        let error = handle.join().unwrap().unwrap_err();
        assert_eq!(error.code, error_codes::DENO_EXECUTION_CANCELLED);
        assert_eq!(owner.active_run_count(), 0);
    }

    #[test]
    fn prepared_execution_derives_every_path_from_one_canonical_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        fs::write(workspace.join("input.txt"), "workspace input\n").unwrap();

        let prepared = prepared(workspace, "script.ts", Vec::new());
        let canonical_workspace = canonical(workspace);

        assert_eq!(prepared.workspace(), canonical_workspace);
        assert!(prepared.entrypoint().starts_with(&canonical_workspace));
        assert!(prepared.cache_dir().starts_with(&canonical_workspace));
        assert!(prepared.cache_dir().is_dir());
        // Workspace-local files stay reachable through the canonical root.
        assert!(canonical_workspace.join("input.txt").is_file());

        let args = build_deno_command_args(&prepared);
        let expected = path_for_external_use(&canonical_workspace);
        assert_eq!(permission_value(&args, "--allow-read"), expected);
        assert_eq!(permission_value(&args, "--allow-write"), expected);
        // The cwd used by the dispatcher is the very same value.
        assert_eq!(path_for_external_use(prepared.workspace()), expected);
    }

    #[test]
    fn caller_arguments_cannot_broaden_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().parent().unwrap().to_path_buf();
        let prepared = prepared(
            temp.path(),
            "script.ts",
            vec![
                "--allow-all".into(),
                format!("--allow-read={}", outside.display()),
                "--deny-net=false".into(),
            ],
        );
        let args = build_deno_command_args(&prepared);
        let separator = args.iter().position(|arg| arg == "--").unwrap();

        let expected = path_for_external_use(prepared.workspace());
        assert_eq!(
            permission_value(&args[..separator], "--allow-read"),
            expected
        );
        assert_eq!(
            permission_value(&args[..separator], "--allow-write"),
            expected
        );
        assert!(args[..separator].iter().all(|arg| arg != "--allow-all"));
        // Everything the caller supplied sits after the entrypoint.
        assert_eq!(args[separator + 2], "--allow-all");
        assert_eq!(args.last().unwrap(), "--deny-net=false");
    }

    #[test]
    fn entrypoint_outside_the_workspace_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let outside = temp.path().join("outside.ts");
        fs::write(&outside, "// outside\n").unwrap();

        let error = PreparedDenoExecution::prepare(&DenoExecutionIntent {
            thread_id: "thread-deno-test".into(),
            workspace_path: workspace,
            entrypoint: outside,
            args: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ENTRYPOINT_INVALID);
    }

    #[test]
    fn traversal_out_of_the_workspace_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(temp.path().join("outside.ts"), "// outside\n").unwrap();

        let error = PreparedDenoExecution::prepare(&DenoExecutionIntent {
            thread_id: "thread-deno-test".into(),
            workspace_path: workspace.clone(),
            entrypoint: workspace.join("..").join("outside.ts"),
            args: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ENTRYPOINT_INVALID);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_reached_through_a_symlink_uses_the_canonical_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_workspace = temp.path().join("real-workspace");
        fs::create_dir_all(&real_workspace).unwrap();
        fs::write(real_workspace.join("input.txt"), "workspace input\n").unwrap();
        fs::write(real_workspace.join("script.ts"), "// script\n").unwrap();
        let alias = temp.path().join("alias-workspace");
        symlink(&real_workspace, &alias).unwrap();

        let prepared = PreparedDenoExecution::prepare(&DenoExecutionIntent {
            thread_id: "thread-deno-test".into(),
            workspace_path: alias.clone(),
            entrypoint: alias.join("script.ts"),
            args: Vec::new(),
        })
        .unwrap();

        let canonical_workspace = canonical(&real_workspace);
        assert_eq!(prepared.workspace(), canonical_workspace);
        assert_eq!(prepared.entrypoint(), canonical_workspace.join("script.ts"));
        assert!(prepared.cache_dir().starts_with(&canonical_workspace));

        // The granted permission covers the canonical contents the script
        // actually reads and writes, not the alias Deno never sees.
        let args = build_deno_command_args(&prepared);
        let granted = PathBuf::from(permission_value(&args, "--allow-read"));
        assert_eq!(granted, canonical_workspace);
        assert!(canonical(&real_workspace.join("input.txt")).starts_with(&granted));
        assert_eq!(
            permission_value(&args, "--allow-write"),
            permission_value(&args, "--allow-read")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_workspace_is_not_made_accessible() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let outside = temp.path().join("outside.ts");
        fs::write(&outside, "// outside\n").unwrap();
        let escape = workspace.join("escape.ts");
        symlink(&outside, &escape).unwrap();

        let error = PreparedDenoExecution::prepare(&DenoExecutionIntent {
            thread_id: "thread-deno-test".into(),
            workspace_path: workspace.clone(),
            entrypoint: escape,
            args: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ENTRYPOINT_INVALID);

        // The escape target is outside the only granted permission root.
        let prepared = prepared(&workspace, "script.ts", Vec::new());
        let granted = PathBuf::from(permission_value(
            &build_deno_command_args(&prepared),
            "--allow-read",
        ));
        assert!(!canonical(&outside).starts_with(&granted));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_var_alias_is_resolved_before_granting_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path(), "script.ts", Vec::new());
        let args = build_deno_command_args(&prepared);
        let granted = PathBuf::from(permission_value(&args, "--allow-read"));

        // macOS exposes the temporary root through the /var alias while
        // canonicalization resolves it through /private/var.
        assert_eq!(granted, canonical(temp.path()));
        assert!(prepared.entrypoint().starts_with(&granted));
        assert!(prepared.cache_dir().starts_with(&granted));
        if temp.path().starts_with("/var/") {
            assert!(granted.starts_with("/private/var/"));
        }
    }
}
