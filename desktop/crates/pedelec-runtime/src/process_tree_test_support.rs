//! Shared real-engine fixture for Runtime and IPC lifecycle tests.
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub(crate) fn source(marker: &Path, exit_parent: bool) -> String {
    let marker = serde_json::to_string(marker).unwrap();
    let child_source = format!(
        r#"
const grandchild = new Deno.Command(Deno.execPath(), {{
  args: ['eval', 'setInterval(() => {{}}, 1000)'],
  stdin: 'null', stdout: 'inherit', stderr: 'inherit'
}}).spawn();
await Deno.writeTextFile({marker}, JSON.stringify([Deno.ppid, Deno.pid, grandchild.pid]));
await grandchild.status;
"#
    );
    let child_source = serde_json::to_string(&child_source).unwrap();
    format!(
        r#"
const child = new Deno.Command(Deno.execPath(), {{
  args: ['eval', {child_source}],
  stdin: 'null', stdout: 'inherit', stderr: 'inherit'
}}).spawn();
while (true) {{
  try {{ await Deno.stat({marker}); break; }} catch {{ await new Promise(r => setTimeout(r, 10)); }}
}}
if ({exit_parent}) Deno.exit(0);
await child.status;
"#
    )
}

pub(crate) fn wait_pids(marker: &Path) -> Vec<u32> {
    let start = Instant::now();
    loop {
        if let Ok(data) = fs::read(marker) {
            if let Ok(pids) = serde_json::from_slice::<Vec<u32>>(&data) {
                assert_eq!(pids.len(), 3);
                return pids;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "descendant PID fixture did not become ready: {}",
            marker.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE},
        };
        let handle = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let result = unsafe { WaitForSingleObject(handle, 0) };
        unsafe {
            CloseHandle(handle);
        }
        result == WAIT_TIMEOUT
    }
    #[cfg(unix)]
    {
        // Zombies have exited but may await the host's orphan reaper.
        #[cfg(target_os = "linux")]
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            if stat
                .rsplit_once(')')
                .is_some_and(|(_, tail)| tail.trim_start().starts_with('Z'))
            {
                return false;
            }
        }
        #[cfg(target_os = "macos")]
        if let Ok(output) = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
        {
            if String::from_utf8_lossy(&output.stdout)
                .trim_start()
                .starts_with('Z')
            {
                return false;
            }
        }
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

pub(crate) fn assert_stopped(pids: &[u32]) {
    let start = Instant::now();
    while pids.iter().any(|pid| alive(*pid)) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "process tree still alive: {pids:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

// Keep a failed assertion from leaving the harmless long-lived fixture behind.
pub(crate) struct Cleanup(pub Vec<u32>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in self.0.iter().rev().copied().filter(|pid| alive(*pid)) {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::{
                    Foundation::CloseHandle,
                    System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
                };
                let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !handle.is_null() {
                    TerminateProcess(handle, 1);
                    CloseHandle(handle);
                }
            }
        }
    }
}
