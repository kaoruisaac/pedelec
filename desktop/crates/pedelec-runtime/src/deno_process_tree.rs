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
    #[cfg(unix)]
    leader_reaped: bool,
    #[cfg(unix)]
    cleanup_failure: Option<String>,
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
            #[cfg(unix)]
            leader_reaped: false,
            #[cfg(unix)]
            cleanup_failure: None,
        })
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // On Unix, keep the group leader waitable until group cleanup. Reaping
        // it here could allow its numeric PGID to be reused by an unrelated run.
        #[cfg(unix)]
        if !self.cleaned && !self.leader_reaped {
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
        // After reaping the leader, its numeric PGID is no longer reserved.
        // In particular, a failed cleanup must never be retried by Drop.
        #[cfg(unix)]
        if self.leader_reaped {
            if let Some(error) = &self.cleanup_failure {
                return Err(io::Error::other(error.clone()));
            }
            return self
                .child
                .try_wait()?
                .ok_or_else(|| io::Error::other("reaped Deno leader has no cached exit status"));
        }

        let tree_result = if self.cleaned {
            Ok(())
        } else {
            #[cfg(target_os = "macos")]
            {
                match self.tree.terminate() {
                    Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                        match self.tree.exited_leader_only(self.child.id()) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err(error),
                            Err(check) => Err(io::Error::new(
                                error.kind(),
                                format!("{error}; macOS group membership check failed: {check}"),
                            )),
                        }
                    }
                    result => result,
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                self.tree.terminate()
            }
        };
        if tree_result.is_ok() {
            self.cleaned = true;
        }
        // Reap even when cleanup fails, but never signal the old group again.
        let parent_result = match self.child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => self.child.kill().and_then(|_| self.child.wait()),
            Err(error) => Err(error),
        };
        #[cfg(unix)]
        if parent_result.is_ok() {
            self.leader_reaped = true;
        }
        let tree_result = tree_result.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("process-tree cleanup failed (descendants may remain): {error}"),
            )
        });
        #[cfg(unix)]
        if let Err(error) = &tree_result {
            self.cleanup_failure
                .get_or_insert_with(|| error.to_string());
        }
        tree_result?;
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
        #[cfg(unix)]
        if self.leader_reaped {
            return;
        }
        let _ = self.terminate();
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::process::CommandExt;

    // Public macOS libproc.h API (available since macOS 10.7).
    // Unlike sysctl(KERN_PROC_PGRP), this does not require duplicating the
    // SDK-specific kinfo_proc ABI in Rust.
    #[cfg(target_os = "macos")]
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_listpgrppids(
            pgrpid: libc::pid_t,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

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
        #[cfg(all(test, target_os = "macos"))]
        pub(super) fn test_set_pgid(&mut self, pgid: libc::pid_t) {
            self.pgid = pgid;
        }
        #[cfg(target_os = "macos")]
        pub(super) fn exited_leader_only(&self, child_pid: u32) -> io::Result<bool> {
            // Only the original direct child can hold this PGID reserved.
            if self.pgid <= 0 || self.pgid as u32 != child_pid {
                return Ok(false);
            }
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // A positive result proves the leader exited and remains waitable;
            // its PID has not yet been released for reuse.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child_pid,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { info.si_pid() } != self.pgid
                || !matches!(
                    info.si_code,
                    libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
                )
            {
                return Ok(false);
            }

            // proc_listpgrppids returns the *number of PIDs*, not bytes.
            // A full buffer, empty result, or any other member fails closed.
            const CAPACITY: usize = 128;
            let mut pids = [0 as libc::pid_t; CAPACITY];
            let count = unsafe {
                proc_listpgrppids(
                    self.pgid,
                    pids.as_mut_ptr().cast(),
                    std::mem::size_of_val(&pids) as libc::c_int,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(count == 1 && pids[0] == self.pgid)
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

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn wait_exited(child: &mut ManagedChild) {
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn exited_leader_only_is_safe_to_reap_after_group_cleanup() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf hello"]).stdout(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        wait_exited(&mut child);
        assert!(child.tree.exited_leader_only(child.child.id()).unwrap());
        assert_eq!(child.terminate().unwrap().code(), Some(0));
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        assert_eq!(stdout, "hello");
        assert_eq!(child.terminate().unwrap().code(), Some(0));
    }

    #[test]
    fn failed_cleanup_then_reap_never_signals_reused_group() {
        let mut unrelated_command = Command::new("/bin/sh");
        unrelated_command.args(["-c", "sleep 20"]);
        let mut unrelated = ManagedChild::spawn(&mut unrelated_command).unwrap();

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut failed = ManagedChild::spawn(&mut command).unwrap();
        wait_exited(&mut failed);
        // A test-only invalid PGID forces a real cleanup error before reap.
        failed.tree.test_set_pgid(0);
        let first = failed.terminate().unwrap_err().to_string();
        assert!(first.contains("invalid Deno process group"));
        assert!(failed.leader_reaped);
        // If Drop or a repeated cleanup signals this group, it will kill the
        // unrelated live process. The leader's old PID is no longer protected.
        failed
            .tree
            .test_set_pgid(unrelated.child.id() as libc::pid_t);
        assert_eq!(failed.terminate().unwrap_err().to_string(), first);
        drop(failed);
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.terminate().unwrap();
    }

    #[test]
    fn exited_leader_with_live_descendant_is_not_zombie_only() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("descendant.pid");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 20 & echo $! > \"$PID_MARKER\""])
            .env("PID_MARKER", &marker);
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        wait_exited(&mut child);
        let pid: libc::pid_t = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::getpgid(pid) },
            child.child.id() as libc::pid_t
        );
        assert!(!child.tree.exited_leader_only(child.child.id()).unwrap());
        child.terminate().unwrap();
        // Zombies are already stopped, even if the orphan reaper is delayed.
        let start = Instant::now();
        loop {
            let output = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&output.stdout);
            if !output.status.success() || stat.trim_start().starts_with('Z') {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
