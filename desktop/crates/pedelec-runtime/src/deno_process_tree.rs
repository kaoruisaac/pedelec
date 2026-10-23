//! One OS lifetime per invocation, shared by Sandbox and Full Access.
//! This manages normally inherited descendants; it is not an OS sandbox.
use std::io;
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, ExitStatus};

#[derive(Debug)]
pub(super) struct ManagedChild {
    child: Child,
    tree: platform::ProcessTree,
    cleaned: bool,
}

impl ManagedChild {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let tree = platform::ProcessTree::prepare(command)?;
        let mut child = command.spawn()?;
        let tree = match tree.attach(&child) {
            Ok(tree) => tree,
            Err(error) => {
                // Windows is still suspended if assignment/resume failed.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            tree,
            cleaned: false,
        })
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // On Unix, keep the group leader waitable until group cleanup. Reaping
        // it here could allow its numeric PGID to be reused by an unrelated run.
        #[cfg(unix)]
        if !self.cleaned {
            use std::os::unix::process::ExitStatusExt;
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            let status = unsafe { info.si_status() };
            return Ok(Some(ExitStatus::from_raw(
                if info.si_code == libc::CLD_EXITED {
                    status << 8
                } else {
                    status
                },
            )));
        }
        self.child.try_wait()
    }

    pub(super) fn terminate(&mut self) -> io::Result<ExitStatus> {
        let tree_result = if self.cleaned {
            Ok(())
        } else {
            self.tree.terminate()
        };
        if tree_result.is_ok() {
            self.cleaned = true;
        }
        // Always reap the immediate child, including when group cleanup fails.
        let parent_result = match self.child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => self.child.kill().and_then(|_| self.child.wait()),
            Err(error) => Err(error),
        };
        tree_result.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("process-tree cleanup failed (descendants may remain): {error}"),
            )
        })?;
        parent_result
    }
}

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[derive(Debug)]
    pub(super) struct ProcessTree {
        pgid: libc::pid_t,
    }
    impl ProcessTree {
        pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
            // Applied in the child before exec, so script code cannot spawn
            // descendants before it belongs to its own group.
            command.process_group(0);
            Ok(Self { pgid: 0 })
        }
        pub(super) fn attach(mut self, child: &Child) -> io::Result<Self> {
            self.pgid = child.id() as libc::pid_t;
            Ok(self)
        }
        pub(super) fn terminate(&self) -> io::Result<()> {
            if self.pgid <= 0 {
                return Err(io::Error::other("invalid Deno process group"));
            }
            // SIGKILL also ends descendants that ignore SIGTERM, releasing
            // inherited stdout/stderr handles before captures are collected.
            if unsafe { libc::kill(-self.pgid, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;

    #[derive(Debug)]
    pub(super) struct ProcessTree {
        job: OwnedHandle,
    }
    impl ProcessTree {
        pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
            // No user code runs until assignment succeeds. Preserve hidden
            // console behavior and never enable job breakaway/inheritance.
            command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let tree = Self {
                job: unsafe { OwnedHandle::from_raw_handle(raw) },
            };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    raw,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(tree)
        }
        pub(super) fn attach(self, child: &Child) -> io::Result<Self> {
            if unsafe { AssignProcessToJobObject(self.job.as_raw_handle(), child.as_raw_handle()) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            // std's primary-thread handle API is unstable. A suspended fresh
            // process has one thread: find it with documented ToolHelp APIs.
            let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
            entry.dwSize = std::mem::size_of_val(&entry) as u32;
            let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
            while found != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if raw.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
                    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(self);
                }
                found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
            }
            Err(io::Error::other(
                "suspended Deno primary thread was not found",
            ))
        }
        pub(super) fn terminate(&self) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // TerminateJobObject initiates termination; confirm the job is
            // empty within a bounded grace rather than claiming success from
            // the parent's exit alone.
            let start = std::time::Instant::now();
            loop {
                let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                    unsafe { std::mem::zeroed() };
                if unsafe {
                    QueryInformationJobObject(
                        self.job.as_raw_handle(),
                        JobObjectBasicAccountingInformation,
                        &mut accounting as *mut _ as _,
                        std::mem::size_of_val(&accounting) as u32,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if accounting.ActiveProcesses == 0 {
                    return Ok(());
                }
                if start.elapsed() >= std::time::Duration::from_secs(1) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Deno job still contains active processes after termination",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}
