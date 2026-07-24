use crate::pedelec_core::{error_codes, PedelecError, ProviderCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenProviderTerminalInput {
    pub provider: ProviderCode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenProviderTerminalOutput {
    pub provider: ProviderCode,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalPlan {
    pub program: String,
    pub args: Vec<String>,
    pub method: String,
}

pub fn open(
    provider: ProviderCode,
    executable: PathBuf,
) -> Result<OpenProviderTerminalOutput, PedelecError> {
    let workdir = provider_terminal_directory()?;
    let plan = terminal_plan(&provider, &executable, &workdir)?;
    spawn_plan(&plan).map_err(|error| launch_error(&provider, &executable, error))?;
    Ok(OpenProviderTerminalOutput {
        provider,
        method: plan.method,
    })
}

fn provider_terminal_directory() -> Result<PathBuf, PedelecError> {
    let home = dirs::home_dir().ok_or_else(|| {
        PedelecError::with_details(
            error_codes::PROVIDER_TERMINAL_WORKDIR_FAILED,
            "Could not determine the user home directory for the provider Terminal.",
            json!({"platform": std::env::consts::OS}),
        )
    })?;
    let path = provider_terminal_directory_from(&home);
    std::fs::create_dir_all(&path).map_err(|error| PedelecError::with_details(
        error_codes::PROVIDER_TERMINAL_WORKDIR_FAILED,
        "Could not create the provider Terminal working directory.",
        json!({"platform": std::env::consts::OS, "workingDirectory": path, "error": error.to_string()}),
    ))?;
    Ok(path)
}

pub(crate) fn provider_terminal_directory_from(home: &Path) -> PathBuf {
    home.join(".pedelec").join("provider-terminal")
}

pub(crate) fn terminal_plan(
    provider: &ProviderCode,
    executable: &Path,
    workdir: &Path,
) -> Result<TerminalPlan, PedelecError> {
    if *provider == ProviderCode::Ollama {
        return Err(unsupported(provider));
    }
    if !executable.is_absolute() {
        return Err(PedelecError::with_details(
            error_codes::PROVIDER_TERMINAL_UNAVAILABLE,
            "The scanned provider executable path is invalid.",
            json!({"provider": provider_name(provider), "executablePath": executable}),
        ));
    }
    #[cfg(windows)]
    {
        Ok(windows_plan(executable, workdir))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(macos_plan(executable, workdir))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux_plan("x-terminal-emulator", executable, workdir))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(unsupported(provider))
    }
}

fn spawn_plan(plan: &TerminalPlan) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = plan.args.last().expect("macOS plan has script");
        let path = std::env::temp_dir().join(format!(
            "pedelec-provider-terminal-{}.command",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", script))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        return Command::new("open").arg(path).spawn().map(|_| ());
    }
    #[cfg(target_os = "linux")]
    {
        let executable = plan.args.last().expect("linux plan has script");
        let mut last_error = None;
        for terminal in [
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "xterm",
        ] {
            let candidate = linux_plan(terminal, Path::new("/unused"), Path::new("/unused"));
            let mut candidate = candidate;
            // Preserve the tested terminal-specific invocation while reusing the generated script.
            *candidate.args.last_mut().expect("script") = executable.clone();
            match Command::new(&candidate.program)
                .args(&candidate.args)
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        return Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no supported terminal emulator found",
            )
        }));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let mut command = Command::new(&plan.program);
        command.args(&plan.args).creation_flags(CREATE_NEW_CONSOLE);
        return command.spawn().map(|_| ());
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(windows)]
fn windows_plan(executable: &Path, workdir: &Path) -> TerminalPlan {
    let script = format!(
        "Set-Location -LiteralPath '{}'; & '{}'; Write-Host ''; Write-Host 'Provider CLI exited. This Terminal will remain open.'",
        powershell_quote(workdir), powershell_quote(executable),
    );
    TerminalPlan {
        program: "powershell.exe".into(),
        args: vec![
            "-NoLogo".into(),
            "-NoExit".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-Command".into(),
            script,
        ],
        method: "windows-powershell".into(),
    }
}

#[cfg(windows)]
fn powershell_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

#[cfg(target_os = "macos")]
fn macos_plan(executable: &Path, workdir: &Path) -> TerminalPlan {
    TerminalPlan {
        program: "open".into(),
        args: vec![shell_script(executable, workdir)],
        method: "macos-command".into(),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_script(executable: &Path, workdir: &Path) -> String {
    format!("cd {} || exit 1\n{}\nprintf '%s\\n' 'Provider CLI exited. This Terminal will remain open.'\nexec \"${{SHELL:-/bin/sh}}\" -l", shell_quote(workdir), shell_quote(executable))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\\"'\\\"'"))
}

#[cfg(target_os = "linux")]
fn linux_plan(terminal: &str, executable: &Path, workdir: &Path) -> TerminalPlan {
    let script = shell_script(executable, workdir);
    let args = match terminal {
        "gnome-terminal" => vec!["--".into(), "sh".into(), "-lc".into(), script],
        "konsole" => vec!["-e".into(), "sh".into(), "-lc".into(), script],
        "xfce4-terminal" => vec![
            "--command".into(),
            format!("sh -lc {}", shell_quote(Path::new(&script))),
        ],
        "xterm" => vec!["-e".into(), "sh".into(), "-lc".into(), script],
        _ => vec!["--".into(), "sh".into(), "-lc".into(), script],
    };
    TerminalPlan {
        program: terminal.into(),
        args,
        method: format!("linux-{}", terminal),
    }
}

fn provider_name(provider: &ProviderCode) -> &'static str {
    match provider {
        ProviderCode::Codex => "codex",
        ProviderCode::Antigravity => "antigravity",
        ProviderCode::OpenCode => "opencode",
        ProviderCode::Cursor => "cursor",
        ProviderCode::Claude => "claude",
        ProviderCode::Ollama => "ollama",
    }
}

fn unsupported(provider: &ProviderCode) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_TERMINAL_UNSUPPORTED,
        "This provider does not support opening a provider CLI Terminal.",
        json!({"provider": provider_name(provider), "platform": std::env::consts::OS}),
    )
}
fn launch_error(provider: &ProviderCode, executable: &Path, error: std::io::Error) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_TERMINAL_LAUNCH_FAILED,
        "Could not open the provider CLI in Terminal.",
        json!({"provider": provider_name(provider), "platform": std::env::consts::OS, "executablePath": executable, "error": error.to_string()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn all_external_providers_build_terminal_plans() {
        let executable = std::env::current_exe().unwrap();
        for provider in [
            ProviderCode::Codex,
            ProviderCode::Antigravity,
            ProviderCode::OpenCode,
            ProviderCode::Cursor,
            ProviderCode::Claude,
        ] {
            assert!(terminal_plan(&provider, &executable, Path::new("/tmp/provider-terminal")).is_ok());
        }
    }
    #[test]
    fn ollama_is_rejected() {
        assert_eq!(
            terminal_plan(
                &ProviderCode::Ollama,
                Path::new("/tmp/ollama"),
                Path::new("/tmp/work")
            )
            .unwrap_err()
            .code,
            error_codes::PROVIDER_TERMINAL_UNSUPPORTED
        );
    }
    #[test]
    fn working_directory_is_fixed_and_created() {
        let temp = tempdir().unwrap();
        let path = provider_terminal_directory_from(temp.path());
        assert_eq!(path, temp.path().join(".pedelec/provider-terminal"));
        std::fs::create_dir_all(&path).unwrap();
        assert!(path.is_dir());
    }
    #[cfg(windows)]
    #[test]
    fn windows_plan_uses_visible_console_and_escapes_paths() {
        let plan = windows_plan(
            Path::new("C:/A B/cli'cmd.cmd"),
            Path::new("C:/A B/work'place"),
        );
        assert_eq!(plan.method, "windows-powershell");
        assert!(plan.args.contains(&"-NoExit".into()));
        assert!(plan.args.last().unwrap().contains("''"));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_plan_uses_absolute_executable_cwd_and_login_shell() {
        let plan = macos_plan(Path::new("/Applications/My CLI"), Path::new("/tmp/work"));
        let script = plan.args.last().unwrap();
        assert!(script.contains("'/Applications/My CLI'"));
        assert!(script.contains("cd '/tmp/work'"));
        assert!(script.contains("exec \"${SHELL:-/bin/sh}\" -l"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_plans_use_terminal_specific_arguments() {
        assert_eq!(
            linux_plan("konsole", Path::new("/tmp/cli"), Path::new("/tmp/work")).args[0],
            "-e"
        );
        assert_eq!(
            linux_plan("xterm", Path::new("/tmp/cli"), Path::new("/tmp/work")).args[0],
            "-e"
        );
        assert_eq!(
            linux_plan(
                "gnome-terminal",
                Path::new("/tmp/cli"),
                Path::new("/tmp/work")
            )
            .args[0],
            "--"
        );
    }
}
