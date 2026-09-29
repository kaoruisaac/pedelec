use crate::runtime_provision::{
    provision_managed_runtime, AppPreparationSnapshot, AppPreparationState, ArchiveDownload,
    ProvisionError, APP_PREPARATION_EVENT,
};
use pedelec_shared::deno_release::DenoArtifact;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::Emitter;

#[derive(Clone)]
pub struct AppPreparation {
    inner: Arc<AppPreparationInner>,
}

struct PreparationRecord {
    snapshot: AppPreparationSnapshot,
    inflight: bool,
}

struct AppPreparationInner {
    record: Mutex<PreparationRecord>,
    synchronous: bool,
    gate: StartupGate,
    config: Mutex<Option<PreparationConfig>>,
    app: Mutex<Option<tauri::AppHandle>>,
    listener: Mutex<Option<Arc<dyn Fn(AppPreparationSnapshot) + Send + Sync>>>,
}

struct PreparationConfig {
    home: PathBuf,
    plan: DenoArtifact,
    downloader: Arc<dyn ArchiveDownload>,
    on_ready: Arc<dyn Fn(PathBuf) -> Result<(), String> + Send + Sync>,
}

pub struct StartupGate {
    entered: AtomicBool,
    completed: AtomicBool,
}

impl StartupGate {
    pub fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            completed: AtomicBool::new(false),
        }
    }

    /// Runs startup at most once after a successful return.
    /// A failed attempt can be retried. A completed attempt is not repeated.
    pub fn on_ready(&self, start: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        if self.completed.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self
            .entered
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        match start() {
            Ok(()) => {
                self.completed.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(err) => {
                self.entered.store(false, Ordering::SeqCst);
                Err(err)
            }
        }
    }
}

impl Default for StartupGate {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StartupProgress {
    pub upload: AtomicBool,
    pub ipc: AtomicBool,
    pub events: AtomicBool,
}

impl StartupProgress {
    pub fn new() -> Self {
        Self {
            upload: AtomicBool::new(false),
            ipc: AtomicBool::new(false),
            events: AtomicBool::new(false),
        }
    }
}

impl Default for StartupProgress {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_idempotent_startup(
    progress: &StartupProgress,
    set_path: impl FnOnce(),
    upload: impl FnOnce() -> Result<(), String>,
    ipc: impl FnOnce() -> Result<(), String>,
    events: impl FnOnce(),
) -> Result<(), String> {
    set_path();
    if !progress.upload.load(Ordering::SeqCst) {
        upload()?;
        progress.upload.store(true, Ordering::SeqCst);
    }
    if !progress.ipc.load(Ordering::SeqCst) {
        ipc()?;
        progress.ipc.store(true, Ordering::SeqCst);
    }
    if !progress.events.swap(true, Ordering::SeqCst) {
        events();
    }
    Ok(())
}

enum AttemptMode {
    Sync,
    Async,
}

impl AppPreparation {
    pub fn new() -> Self {
        Self::with_mode(false)
    }

    fn with_mode(synchronous: bool) -> Self {
        Self {
            inner: Arc::new(AppPreparationInner {
                record: Mutex::new(PreparationRecord {
                    snapshot: AppPreparationSnapshot {
                        attempt: 0,
                        state: AppPreparationState::Checking,
                    },
                    inflight: false,
                }),
                synchronous,
                gate: StartupGate::new(),
                config: Mutex::new(None),
                app: Mutex::new(None),
                listener: Mutex::new(None),
            }),
        }
    }

    pub fn attach_app(&self, app: tauri::AppHandle) {
        *self.inner.app.lock().unwrap() = Some(app);
    }

    pub fn configure(
        &self,
        home: PathBuf,
        plan: DenoArtifact,
        downloader: Arc<dyn ArchiveDownload>,
        on_ready: Arc<dyn Fn(PathBuf) -> Result<(), String> + Send + Sync>,
    ) {
        *self.inner.config.lock().unwrap() = Some(PreparationConfig {
            home,
            plan,
            downloader,
            on_ready,
        });
    }

    pub fn snapshot(&self) -> AppPreparationSnapshot {
        self.inner.record.lock().unwrap().snapshot.clone()
    }

    pub fn bootstrap(&self) -> Result<(), String> {
        self.start_attempt(if self.inner.synchronous {
            AttemptMode::Sync
        } else {
            AttemptMode::Async
        })
    }

    pub fn retry(&self) -> AppPreparationSnapshot {
        if !matches!(self.snapshot().state, AppPreparationState::Failed) {
            return self.snapshot();
        }
        let _ = self.bootstrap();
        self.snapshot()
    }

    fn archive_size(&self) -> u64 {
        self.inner
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|config| config.plan.archive_size_bytes)
            .unwrap_or(0)
    }

    fn start_attempt(&self, mode: AttemptMode) -> Result<(), String> {
        let installed = self.installed_executable();
        let total_bytes = self.archive_size();
        enum Start {
            Idle,
            Installed { attempt: u64, path: PathBuf },
            Missing { attempt: u64 },
        }
        // Publish the deterministic state before releasing the record so a
        // snapshot read cannot observe a non-blocking check while the runtime
        // is already known to be missing.
        let start = {
            let mut record = self.inner.record.lock().unwrap();
            if matches!(record.snapshot.state, AppPreparationState::Ready) || record.inflight {
                Start::Idle
            } else {
                record.inflight = true;
                record.snapshot.attempt = record.snapshot.attempt.saturating_add(1);
                let attempt = record.snapshot.attempt;
                if let Some(path) = installed {
                    record.snapshot.state = AppPreparationState::Checking;
                    Start::Installed { attempt, path }
                } else {
                    record.snapshot.state = AppPreparationState::Downloading {
                        downloaded_bytes: 0,
                        total_bytes,
                        progress_percent: 0,
                    };
                    Start::Missing { attempt }
                }
            }
        };
        match start {
            Start::Idle => Ok(()),
            Start::Installed { attempt, path } => {
                self.finish(Ok(path), attempt);
                self.failure_if_needed()
            }
            Start::Missing { attempt } => {
                self.emit();
                match mode {
                    AttemptMode::Sync => {
                        let result = self.provision(attempt);
                        self.finish(result, attempt);
                        self.failure_if_needed()
                    }
                    AttemptMode::Async => {
                        self.spawn(attempt);
                        Ok(())
                    }
                }
            }
        }
    }

    fn installed_executable(&self) -> Option<PathBuf> {
        let config = self.inner.config.lock().unwrap();
        let config = config.as_ref()?;
        let final_dir = pedelec_shared::paths::managed_deno_runtime_dir(
            &config.home,
            &config.plan.version,
            &config.plan.target,
        )
        .ok()?;
        let executable = final_dir.join(&config.plan.executable_name);
        executable.is_file().then_some(executable)
    }

    fn provision(&self, attempt: u64) -> Result<PathBuf, ProvisionError> {
        let config = self.inner.config.lock().unwrap().as_ref().map(|config| {
            (
                config.home.clone(),
                config.plan.clone(),
                Arc::clone(&config.downloader),
            )
        });
        let Some((home, plan, downloader)) = config else {
            return Err(ProvisionError::new(
                "runtime provisioning is not configured",
            ));
        };
        let preparation = self.clone();
        provision_managed_runtime(
            &plan,
            &home,
            &attempt.to_string(),
            downloader.as_ref(),
            &mut move |state| preparation.publish_attempt(attempt, state),
        )
    }

    fn spawn(&self, attempt: u64) {
        let preparation = self.clone();
        let spawned = thread::Builder::new()
            .name("pedelec-runtime-provision".to_string())
            .spawn(move || {
                let result = preparation.provision(attempt);
                preparation.finish(result, attempt);
            });
        if let Err(err) = spawned {
            self.finish(Err(ProvisionError::new(err.to_string())), attempt);
        }
    }

    fn finish(&self, result: Result<PathBuf, ProvisionError>, attempt: u64) {
        let next = match result {
            Ok(path) => match self.continue_startup(&path) {
                Ok(()) => Ok(AppPreparationState::Ready),
                Err(err) => {
                    eprintln!("app preparation failed: {err}");
                    Err(AppPreparationState::Failed)
                }
            },
            Err(err) => {
                eprintln!("app preparation failed: {err}");
                Err(AppPreparationState::Failed)
            }
        };
        let state = next.unwrap_or_else(|state| state);
        let changed = {
            let mut record = self.inner.record.lock().unwrap();
            let current_attempt = record.snapshot.attempt == attempt;
            if current_attempt && transition_allowed(&record.snapshot.state, &state) {
                record.snapshot.state = state;
            }
            if record.inflight && current_attempt {
                record.inflight = false;
            }
            current_attempt
        };
        if changed {
            self.emit();
        }
    }

    fn continue_startup(&self, path: &Path) -> Result<(), String> {
        let on_ready = {
            let config = self.inner.config.lock().unwrap();
            config.as_ref().map(|config| Arc::clone(&config.on_ready))
        };
        let Some(on_ready) = on_ready else {
            return Err("runtime provisioning is not configured".to_string());
        };
        let executable = path.to_path_buf();
        self.inner.gate.on_ready(move || on_ready(executable))
    }

    fn publish_attempt(&self, attempt: u64, state: AppPreparationState) {
        let changed = {
            let mut record = self.inner.record.lock().unwrap();
            if record.snapshot.attempt != attempt
                || !transition_allowed(&record.snapshot.state, &state)
            {
                false
            } else {
                record.snapshot.state = state;
                true
            }
        };
        if changed {
            self.emit();
        }
    }

    fn emit(&self) {
        let snapshot = self.snapshot();
        if let Some(listener) = self.inner.listener.lock().unwrap().clone() {
            listener(snapshot.clone());
        }
        if let Some(app) = self.inner.app.lock().unwrap().clone() {
            let _ = app.emit(APP_PREPARATION_EVENT, snapshot);
        }
    }

    fn failure_if_needed(&self) -> Result<(), String> {
        if matches!(self.snapshot().state, AppPreparationState::Failed) {
            Err("Pedelec couldn't finish preparing.".to_string())
        } else {
            Ok(())
        }
    }
}

impl Default for AppPreparation {
    fn default() -> Self {
        Self::new()
    }
}

fn transition_allowed(current: &AppPreparationState, next: &AppPreparationState) -> bool {
    if matches!(current, AppPreparationState::Ready) {
        return false;
    }
    match next {
        AppPreparationState::Failed => true,
        AppPreparationState::Checking => false,
        AppPreparationState::Downloading {
            progress_percent: next_percent,
            ..
        } => match current {
            AppPreparationState::Checking => true,
            AppPreparationState::Downloading {
                progress_percent: previous,
                ..
            } => next_percent >= previous,
            _ => false,
        },
        AppPreparationState::Finalizing { .. } => {
            matches!(
                current,
                AppPreparationState::Checking
                    | AppPreparationState::Downloading { .. }
                    | AppPreparationState::Finalizing { .. }
            )
        }
        AppPreparationState::Ready => matches!(
            current,
            AppPreparationState::Checking
                | AppPreparationState::Downloading { .. }
                | AppPreparationState::Finalizing { .. }
        ),
    }
}

/// Background provisioning must not force the main window visible.
pub fn should_present_main_window(background_launch: bool) -> bool {
    !background_launch
}

/// Foreground launches reveal the main window once, after the frontend has a
/// deterministic preparation frame. Background launches stay hidden.
pub struct WindowPresentation {
    foreground: bool,
    revealed: AtomicBool,
}

impl WindowPresentation {
    pub fn new(foreground: bool) -> Self {
        Self {
            foreground,
            revealed: AtomicBool::new(false),
        }
    }

    pub fn reveal_once(&self) -> bool {
        if !self.foreground {
            return false;
        }
        self.revealed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

#[tauri::command]
pub fn get_app_preparation_state(
    state: tauri::State<'_, AppPreparation>,
) -> AppPreparationSnapshot {
    state.snapshot()
}

#[tauri::command]
pub fn retry_app_preparation(state: tauri::State<'_, AppPreparation>) -> AppPreparationSnapshot {
    state.retry()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_provision::{zip_archive, ArchiveDownload, ProvisionError};
    use pedelec_shared::deno_release::DenoArtifact;
    use pedelec_shared::paths::deno_executable_file_name;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, AtomicUsize};
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    struct FlakyDownload {
        body: Vec<u8>,
        calls: AtomicUsize,
        failures_remaining: AtomicUsize,
    }

    impl ArchiveDownload for FlakyDownload {
        fn download_to(
            &self,
            _url: &str,
            expected_size: u64,
            destination: &Path,
            on_progress: &mut dyn FnMut(u64, u64),
        ) -> Result<(), ProvisionError> {
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
            crate::runtime_provision::ingest_archive(
                &mut Cursor::new(self.body.as_slice()),
                None,
                expected_size,
                destination,
                on_progress,
            )
        }
    }

    fn plan_for(body: &[u8]) -> DenoArtifact {
        let mut hasher = Sha256::new();
        hasher.update(body);
        let digest = hasher.finalize();
        let archive_sha256 = digest.iter().fold(String::new(), |mut output, byte| {
            output.push_str(&format!("{byte:02x}"));
            output
        });
        DenoArtifact {
            version: "2.9.5".into(),
            target: "x86_64-pc-windows-msvc".into(),
            platform: "win32".into(),
            artifact: "deno-x86_64-pc-windows-msvc.zip".into(),
            primary_url: "https://runtime.pedelec.cc/deno/v2.9.5/deno-x86_64-pc-windows-msvc.zip"
                .into(),
            fallback_url:
                "https://github.com/denoland/deno/releases/download/v2.9.5/deno-x86_64-pc-windows-msvc.zip"
                    .into(),
            archive_sha256,
            archive_size_bytes: body.len() as u64,
            executable_name: deno_executable_file_name().into(),
        }
    }

    #[test]
    fn startup_continuation_runs_once_across_provisioning_retries() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"managed-runtime")]);
        let plan = plan_for(&body);
        let downloader = Arc::new(FlakyDownload {
            body,
            calls: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(2),
        });
        let runs = Arc::new(AtomicU32::new(0));
        let preparation = AppPreparation::with_mode(true);
        let recorded = Arc::clone(&runs);
        let provisioner = Arc::clone(&downloader);
        preparation.configure(
            temp.path().to_path_buf(),
            plan,
            provisioner,
            Arc::new(move |path| {
                assert!(path.is_file());
                recorded.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );

        let first = preparation.bootstrap();
        assert!(first.is_err());
        assert_eq!(downloader.calls.load(Ordering::SeqCst), 2);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(matches!(
            preparation.snapshot().state,
            AppPreparationState::Failed
        ));

        let retried = preparation.retry();
        assert!(matches!(retried.state, AppPreparationState::Ready));
        assert_eq!(downloader.calls.load(Ordering::SeqCst), 3);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let again = preparation.retry();
        assert!(matches!(again.state, AppPreparationState::Ready));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn startup_gate_allows_one_success_and_retries_a_failure() {
        let gate = StartupGate::new();
        let runs = AtomicU32::new(0);
        let err = gate.on_ready(|| {
            runs.fetch_add(1, Ordering::SeqCst);
            Err("not ready".into())
        });
        assert!(err.is_err());
        gate.on_ready(|| {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        gate.on_ready(|| {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn idempotent_startup_does_not_repeat_completed_steps() {
        let progress = StartupProgress::new();
        let uploads = AtomicU32::new(0);
        let ipcs = AtomicU32::new(0);
        let events = AtomicU32::new(0);
        let err = run_idempotent_startup(
            &progress,
            || {},
            || {
                uploads.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                ipcs.fetch_add(1, Ordering::SeqCst);
                Err("ipc unavailable".into())
            },
            || {
                events.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert!(err.is_err());
        run_idempotent_startup(
            &progress,
            || {},
            || {
                uploads.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                ipcs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                events.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        run_idempotent_startup(
            &progress,
            || {},
            || {
                uploads.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                ipcs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                events.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        assert_eq!(uploads.load(Ordering::SeqCst), 1);
        assert_eq!(ipcs.load(Ordering::SeqCst), 2);
        assert_eq!(events.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn background_launch_stays_hidden_while_provisioning_is_required() {
        assert!(!should_present_main_window(true));
        assert!(should_present_main_window(false));
    }

    #[test]
    fn foreground_frame_reveals_the_window_once() {
        let presentation = WindowPresentation::new(should_present_main_window(false));
        assert!(presentation.reveal_once());
        assert!(!presentation.reveal_once());
    }

    #[test]
    fn background_frame_does_not_reveal_the_window() {
        let presentation = WindowPresentation::new(should_present_main_window(true));
        assert!(!presentation.reveal_once());
        assert!(!presentation.reveal_once());
    }

    #[test]
    fn fast_path_marks_ready_without_download_progress() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"unused")]);
        let plan = plan_for(&body);
        let executable = temp
            .path()
            .join("runtimes")
            .join("deno")
            .join(&plan.version)
            .join(&plan.target)
            .join(executable_name);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"already").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let downloader = Arc::new(CountingDownload {
            calls: Arc::clone(&calls),
        });
        let preparation = AppPreparation::with_mode(true);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        *preparation.inner.listener.lock().unwrap() = Some(Arc::new(move |snapshot| {
            recorded.lock().unwrap().push(snapshot);
        }));
        preparation.configure(
            temp.path().to_path_buf(),
            plan,
            downloader,
            Arc::new(|path| {
                assert_eq!(std::fs::read(path).unwrap(), b"already");
                Ok(())
            }),
        );
        preparation.bootstrap().unwrap();
        assert!(matches!(
            preparation.snapshot().state,
            AppPreparationState::Ready
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let events = seen.lock().unwrap().clone();
        assert!(events
            .iter()
            .all(|snapshot| matches!(snapshot.state, AppPreparationState::Ready)));
        assert!(events
            .iter()
            .any(|snapshot| matches!(snapshot.state, AppPreparationState::Ready)));
    }

    struct CountingDownload {
        calls: Arc<AtomicUsize>,
    }

    impl ArchiveDownload for CountingDownload {
        fn download_to(
            &self,
            _url: &str,
            _expected_size: u64,
            _destination: &Path,
            _on_progress: &mut dyn FnMut(u64, u64),
        ) -> Result<(), ProvisionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ProvisionError::new("download should not run"))
        }
    }

    #[test]
    fn listener_observes_the_current_snapshot_after_subscription() {
        let preparation = AppPreparation::with_mode(true);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        *preparation.inner.listener.lock().unwrap() = Some(Arc::new(move |snapshot| {
            recorded.lock().unwrap().push(snapshot);
        }));
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"runtime")]);
        let plan = plan_for(&body);
        let downloader = Arc::new(FlakyDownload {
            body,
            calls: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(0),
        });
        let temp = tempfile::tempdir().unwrap();
        preparation.configure(
            temp.path().to_path_buf(),
            plan,
            downloader,
            Arc::new(|_| Ok(())),
        );
        preparation.bootstrap().unwrap();
        let events = seen.lock().unwrap().clone();
        assert!(events
            .iter()
            .any(|snapshot| snapshot.state == AppPreparationState::Ready));
        assert_eq!(preparation.snapshot().state, AppPreparationState::Ready);
    }

    struct ReleaseOnDrop(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let (lock, cvar) = &*self.0;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
    }

    struct StalledDownload {
        entered: Arc<(Mutex<bool>, Condvar)>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ArchiveDownload for StalledDownload {
        fn download_to(
            &self,
            _url: &str,
            _expected_size: u64,
            _destination: &Path,
            _on_progress: &mut dyn FnMut(u64, u64),
        ) -> Result<(), ProvisionError> {
            {
                let (lock, cvar) = &*self.entered;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
            Err(ProvisionError::new("stalled before the first byte"))
        }
    }

    #[test]
    fn missing_runtime_blocks_at_zero_before_the_first_download_byte() {
        let temp = tempfile::tempdir().unwrap();
        let executable_name = deno_executable_file_name();
        let body = zip_archive(&[(executable_name, b"runtime")]);
        let plan = plan_for(&body);
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let downloader = Arc::new(StalledDownload {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let preparation = AppPreparation::with_mode(false);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        *preparation.inner.listener.lock().unwrap() = Some(Arc::new(move |snapshot| {
            recorded.lock().unwrap().push(snapshot);
        }));
        preparation.configure(
            temp.path().to_path_buf(),
            plan.clone(),
            downloader,
            Arc::new(|_| Ok(())),
        );
        let _release_on_drop = ReleaseOnDrop(Arc::clone(&release));
        preparation.bootstrap().unwrap();

        match preparation.snapshot().state {
            AppPreparationState::Downloading {
                downloaded_bytes,
                total_bytes,
                progress_percent,
            } => {
                assert_eq!(downloaded_bytes, 0);
                assert_eq!(total_bytes, plan.archive_size_bytes);
                assert_eq!(progress_percent, 0);
            }
            other => panic!("expected a blocking 0% download, got {other:?}"),
        }

        let (lock, cvar) = &*entered;
        let mut has_entered = lock.lock().unwrap();
        let started = Instant::now();
        while !*has_entered {
            let (guard, timeout) = cvar
                .wait_timeout(has_entered, Duration::from_secs(5))
                .unwrap();
            has_entered = guard;
            if timeout.timed_out() && !*has_entered && started.elapsed() > Duration::from_secs(5) {
                panic!("downloader did not start");
            }
        }
        drop(has_entered);

        match preparation.snapshot().state {
            AppPreparationState::Downloading {
                downloaded_bytes,
                progress_percent,
                ..
            } => {
                assert_eq!(downloaded_bytes, 0);
                assert_eq!(progress_percent, 0);
            }
            other => panic!("download progressed before the first byte: {other:?}"),
        }
        let events = seen.lock().unwrap().clone();
        assert!(matches!(
            events.first().map(|snapshot| &snapshot.state),
            Some(AppPreparationState::Downloading {
                downloaded_bytes: 0,
                progress_percent: 0,
                ..
            })
        ));
        assert!(events
            .iter()
            .all(|snapshot| !matches!(snapshot.state, AppPreparationState::Checking)));

        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        let finished = Instant::now();
        while !matches!(
            preparation.snapshot().state,
            AppPreparationState::Failed | AppPreparationState::Ready
        ) && finished.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            preparation.snapshot().state,
            AppPreparationState::Failed
        ));
    }
}
