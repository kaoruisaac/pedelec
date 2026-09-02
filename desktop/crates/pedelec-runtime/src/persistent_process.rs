use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

static NEXT_PROCESS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Identity for one lifetime of a persistent child. It is intentionally not
/// derived from the operating-system process id.
pub type ProcessGeneration = u64;

/// Shared guard for callbacks that outlive a process replacement. A callback
/// is safe to apply only when its generation is still current.
#[derive(Debug, Clone, Default)]
pub struct ProcessGenerationGuard {
    current: Arc<AtomicU64>,
}

impl ProcessGenerationGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, generation: ProcessGeneration) -> bool {
        let mut current = self.current.load(Ordering::Acquire);
        loop {
            if generation <= current {
                return false;
            }
            match self.current.compare_exchange(
                current,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }

    pub fn is_current(&self, generation: ProcessGeneration) -> bool {
        self.current.load(Ordering::Acquire) == generation
    }

    pub fn accepts(&self, exit: &ProcessExit) -> bool {
        self.is_current(exit.generation)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PersistentProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_remove: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl PersistentProcessSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, T>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistentProcessError {
    EmptyProgram,
    Spawn {
        program: String,
        error: String,
    },
    MissingStdin,
    MissingStdout,
    MissingStderr,
    Io {
        operation: &'static str,
        error: String,
    },
    ShutdownTimeout {
        generation: ProcessGeneration,
        pid: u32,
    },
}

impl fmt::Display for PersistentProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProgram => write!(f, "persistent process program is empty"),
            Self::Spawn { program, error } => {
                write!(f, "could not spawn persistent process {program}: {error}")
            }
            Self::MissingStdin => write!(f, "persistent process stdin was not piped"),
            Self::MissingStdout => write!(f, "persistent process stdout was not piped"),
            Self::MissingStderr => write!(f, "persistent process stderr was not piped"),
            Self::Io { operation, error } => write!(f, "persistent process {operation} failed: {error}"),
            Self::ShutdownTimeout { generation, pid } => write!(
                f,
                "persistent process generation {generation} (pid {pid}) did not exit after shutdown",
            ),
        }
    }
}

impl std::error::Error for PersistentProcessError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessExitKind {
    ExpectedShutdown,
    UnexpectedExit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub generation: ProcessGeneration,
    pub pid: u32,
    pub status: Option<ExitStatusSnapshot>,
    pub kind: ProcessExitKind,
}

/// A portable, serializable representation of `ExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatusSnapshot {
    pub success: bool,
    pub code: Option<i32>,
}

impl From<ExitStatus> for ExitStatusSnapshot {
    fn from(status: ExitStatus) -> Self {
        Self {
            success: status.success(),
            code: status.code(),
        }
    }
}

#[derive(Debug)]
struct ProcessLifecycle {
    exit: Option<ProcessExit>,
    shutdown_requested: bool,
}

#[derive(Debug)]
struct PersistentProcessInner {
    generation: ProcessGeneration,
    pid: u32,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    lifecycle: (Mutex<ProcessLifecycle>, Condvar),
    exit_subscribers: Mutex<Vec<mpsc::Sender<ProcessExit>>>,
}

/// A persistent child process with independently owned protocol and
/// diagnostic streams.
#[derive(Debug)]
pub struct PersistentProcess {
    inner: Arc<PersistentProcessInner>,
    stdout: Mutex<Option<ChildStdout>>,
    stderr: Mutex<Option<ChildStderr>>,
}

impl PersistentProcess {
    pub fn spawn(spec: PersistentProcessSpec) -> Result<Self, PersistentProcessError> {
        if spec.program.as_os_str().is_empty() {
            return Err(PersistentProcessError::EmptyProgram);
        }

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for key in &spec.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command
            .spawn()
            .map_err(|error| PersistentProcessError::Spawn {
                program: spec.program.to_string_lossy().into_owned(),
                error: error.to_string(),
            })?;
        let pid = child.id();
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_child_after_setup_failure(&mut child);
                return Err(PersistentProcessError::MissingStdin);
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_child_after_setup_failure(&mut child);
                return Err(PersistentProcessError::MissingStdout);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_child_after_setup_failure(&mut child);
                return Err(PersistentProcessError::MissingStderr);
            }
        };
        let generation = NEXT_PROCESS_GENERATION.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::new(PersistentProcessInner {
            generation,
            pid,
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            lifecycle: (
                Mutex::new(ProcessLifecycle {
                    exit: None,
                    shutdown_requested: false,
                }),
                Condvar::new(),
            ),
            exit_subscribers: Mutex::new(Vec::new()),
        });

        spawn_exit_watcher(Arc::clone(&inner));

        Ok(Self {
            inner,
            stdout: Mutex::new(Some(stdout)),
            stderr: Mutex::new(Some(stderr)),
        })
    }

    pub fn generation(&self) -> ProcessGeneration {
        self.inner.generation
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    pub fn writer(&self) -> PersistentWriter {
        PersistentWriter {
            stdin: Arc::clone(&self.inner),
        }
    }

    pub fn take_stdout(&self) -> Result<ChildStdout, PersistentProcessError> {
        self.stdout
            .lock()
            .expect("persistent stdout mutex poisoned")
            .take()
            .ok_or(PersistentProcessError::MissingStdout)
    }

    pub fn take_stderr(&self) -> Result<ChildStderr, PersistentProcessError> {
        self.stderr
            .lock()
            .expect("persistent stderr mutex poisoned")
            .take()
            .ok_or(PersistentProcessError::MissingStderr)
    }

    pub fn is_running(&self) -> bool {
        self.exit_event().is_none()
    }

    pub fn exit_event(&self) -> Option<ProcessExit> {
        self.inner
            .lifecycle
            .0
            .lock()
            .expect("persistent lifecycle mutex poisoned")
            .exit
            .clone()
    }

    /// Subscribe to the one process-lifetime exit event. Subscribers created
    /// after exit receive the already-known event immediately.
    pub fn subscribe_exit(&self) -> mpsc::Receiver<ProcessExit> {
        let (tx, rx) = mpsc::channel();
        if let Some(exit) = self.exit_event() {
            let _ = tx.send(exit);
            return rx;
        }
        let lifecycle = self
            .inner
            .lifecycle
            .0
            .lock()
            .expect("persistent lifecycle mutex poisoned");
        let mut subscribers = self
            .inner
            .exit_subscribers
            .lock()
            .expect("persistent exit subscribers mutex poisoned");
        if let Some(exit) = lifecycle.exit.clone() {
            let _ = tx.send(exit);
        } else {
            subscribers.push(tx);
        }
        rx
    }

    pub fn wait_for_exit(&self, timeout: Duration) -> Option<ProcessExit> {
        let (lifecycle, changed) = &self.inner.lifecycle;
        let mut state = lifecycle
            .lock()
            .expect("persistent lifecycle mutex poisoned");
        if state.exit.is_none() {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next_state, result) = changed
                    .wait_timeout(state, remaining)
                    .expect("persistent lifecycle mutex poisoned");
                state = next_state;
                if state.exit.is_some() || result.timed_out() {
                    break;
                }
            }
        }
        state.exit.clone()
    }

    /// Close stdin and wait for a normal child exit, falling back to a force
    /// kill if the child does not honor EOF within `grace_period`.
    pub fn shutdown(&self, grace_period: Duration) -> Result<ProcessExit, PersistentProcessError> {
        self.mark_shutdown_requested();
        self.writer().close();

        if let Some(exit) = self.wait_for_exit(grace_period) {
            return Ok(exit);
        }

        self.force_kill()?;
        self.wait_for_exit(grace_period.max(Duration::from_millis(100)))
            .ok_or(PersistentProcessError::ShutdownTimeout {
                generation: self.generation(),
                pid: self.pid(),
            })
    }

    /// Mark this process as intentionally stopping and terminate it. The
    /// resulting exit event is therefore classified as expected shutdown.
    pub fn force_kill(&self) -> Result<(), PersistentProcessError> {
        self.mark_shutdown_requested();
        let mut child = self
            .inner
            .child
            .lock()
            .expect("persistent child mutex poisoned");
        if let Some(child) = child.as_mut() {
            child.kill().map_err(|error| PersistentProcessError::Io {
                operation: "force-kill",
                error: error.to_string(),
            })?;
        }
        Ok(())
    }

    /// Retire a process generation because its transport is no longer safe.
    ///
    /// This is deliberately distinct from [`Self::force_kill`]. A forced
    /// retirement must produce an unexpected exit notification so the owning
    /// runtime can fan the failure out to every attached session. Treating it
    /// as an expected shutdown would make a kill race capable of silently
    /// stranding active Pedelec threads.
    pub fn retire(&self) -> Result<(), PersistentProcessError> {
        let mut child = self
            .inner
            .child
            .lock()
            .expect("persistent child mutex poisoned");
        if let Some(child) = child.as_mut() {
            child.kill().map_err(|error| PersistentProcessError::Io {
                operation: "retire",
                error: error.to_string(),
            })?;
        }
        Ok(())
    }

    fn mark_shutdown_requested(&self) {
        let mut lifecycle = self
            .inner
            .lifecycle
            .0
            .lock()
            .expect("persistent lifecycle mutex poisoned");
        lifecycle.shutdown_requested = true;
    }
}

fn terminate_child_after_setup_failure(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for PersistentProcess {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.shutdown(Duration::from_millis(100));
        }
    }
}

#[derive(Debug, Clone)]
pub struct PersistentWriter {
    stdin: Arc<PersistentProcessInner>,
}

impl PersistentWriter {
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stdin = self
            .stdin
            .stdin
            .lock()
            .expect("persistent stdin mutex poisoned");
        let stdin = stdin.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "persistent process stdin is closed",
            )
        })?;
        stdin.write_all(bytes)
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut stdin = self
            .stdin
            .stdin
            .lock()
            .expect("persistent stdin mutex poisoned");
        let stdin = stdin.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "persistent process stdin is closed",
            )
        })?;
        stdin.flush()
    }

    pub fn close(&self) {
        self.stdin
            .stdin
            .lock()
            .expect("persistent stdin mutex poisoned")
            .take();
    }

    pub fn generation(&self) -> ProcessGeneration {
        self.stdin.generation
    }
}

fn spawn_exit_watcher(inner: Arc<PersistentProcessInner>) {
    thread::Builder::new()
        .name(format!("pedelec-runtime-exit-{}", inner.generation))
        .spawn(move || loop {
            let result = {
                let mut child = inner.child.lock().expect("persistent child mutex poisoned");
                child.as_mut().map(Child::try_wait)
            };

            match result {
                Some(Ok(Some(status))) => {
                    let kind = {
                        let lifecycle = inner
                            .lifecycle
                            .0
                            .lock()
                            .expect("persistent lifecycle mutex poisoned");
                        if lifecycle.shutdown_requested {
                            ProcessExitKind::ExpectedShutdown
                        } else {
                            ProcessExitKind::UnexpectedExit
                        }
                    };
                    let exit = ProcessExit {
                        generation: inner.generation,
                        pid: inner.pid,
                        status: Some(status.into()),
                        kind,
                    };
                    let subscribers = {
                        let mut lifecycle = inner
                            .lifecycle
                            .0
                            .lock()
                            .expect("persistent lifecycle mutex poisoned");
                        lifecycle.exit = Some(exit.clone());
                        inner.lifecycle.1.notify_all();
                        std::mem::take(
                            &mut *inner
                                .exit_subscribers
                                .lock()
                                .expect("persistent exit subscribers mutex poisoned"),
                        )
                    };
                    for subscriber in subscribers {
                        let _ = subscriber.send(exit.clone());
                    }
                    break;
                }
                Some(Ok(None)) => thread::sleep(Duration::from_millis(10)),
                Some(Err(_)) => {
                    // A failed status probe cannot safely produce an exit
                    // status. Keep polling; a later shutdown can still kill
                    // and reap the child.
                    thread::sleep(Duration::from_millis(10));
                }
                None => break,
            }
        })
        .expect("could not start persistent process exit watcher");
}
