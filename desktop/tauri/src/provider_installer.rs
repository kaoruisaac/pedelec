use pedelec_core::{error_codes, PedelecError, ProviderCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenProviderInstallerInput {
    pub provider: ProviderCode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenProviderInstallerOutput {
    pub provider: ProviderCode,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallerPlan {
    pub program: String,
    pub args: Vec<String>,
    pub method: String,
}

const FINISHED: &str =
    "Installation command finished. Return to Pedelec and click Restart Pedelec when ready.";
const ANTIGRAVITY_FINISHED: &str =
    "Antigravity installation and sign-in flow has finished. Return to Pedelec and click Restart Pedelec when ready.";
const CURSOR_FINISHED: &str =
    "Cursor installation and sign-in flow has finished. Return to Pedelec and click Restart Pedelec when ready.";
const CLAUDE_FINISHED: &str =
    "Claude installation and sign-in flow has finished. Return to Pedelec and click Restart Pedelec when ready.";
#[cfg(any(target_os = "macos", target_os = "linux"))]
const CODEX_FINISHED: &str = "Codex installation and sign-in flow has finished. Return to Pedelec and click Restart Pedelec when ready.";
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

pub fn open(
    input: OpenProviderInstallerInput,
) -> Result<OpenProviderInstallerOutput, PedelecError> {
    let plan = installer_plan(&input.provider)?;
    #[cfg(target_os = "macos")]
    {
        open_macos_terminal(&plan).map_err(|error| launch_error(&input.provider, error))?;
    }
    #[cfg(target_os = "linux")]
    {
        open_linux_terminal(&plan).map_err(|error| launch_error(&input.provider, error))?;
    }
    #[cfg(windows)]
    {
        let mut command = Command::new(&plan.program);
        command.args(&plan.args);
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_CONSOLE); // Deliberately not CREATE_NO_WINDOW.
        command
            .spawn()
            .map_err(|error| launch_error(&input.provider, error))?;
    }
    Ok(OpenProviderInstallerOutput {
        provider: input.provider,
        method: plan.method,
    })
}

#[cfg(target_os = "macos")]
fn open_macos_terminal(plan: &InstallerPlan) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let script = plan.args.last().expect("shell plan has script");
    let path = std::env::temp_dir().join(format!(
        "pedelec-provider-install-{}.command",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, format!("#!/bin/sh\n{}\n", script))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Command::new("open").arg(path).spawn().map(|_| ())
}

#[cfg(target_os = "linux")]
fn open_linux_terminal(plan: &InstallerPlan) -> std::io::Result<()> {
    let script = plan.args.last().expect("shell plan has script");
    let mut last_error = None;
    for terminal in [
        "x-terminal-emulator",
        "gnome-terminal",
        "konsole",
        "xfce4-terminal",
        "xterm",
    ] {
        let result = if terminal == "xterm" {
            Command::new(terminal)
                .args(["-e", "sh", "-lc", script])
                .spawn()
        } else {
            Command::new(terminal)
                .args(["--", "sh", "-lc", script])
                .spawn()
        };
        match result {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal emulator found")
    }))
}

pub(crate) fn installer_plan(provider: &ProviderCode) -> Result<InstallerPlan, PedelecError> {
    if !matches!(
        provider,
        ProviderCode::Codex
            | ProviderCode::Antigravity
            | ProviderCode::OpenCode
            | ProviderCode::Cursor
            | ProviderCode::Claude
    ) {
        return Err(unsupported(provider));
    }
    #[cfg(windows)]
    {
        return Ok(windows_plan(provider));
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Ok(shell_plan(provider))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(unsupported(provider))
    }
}

#[cfg(windows)]
fn windows_plan(provider: &ProviderCode) -> InstallerPlan {
    let script = match provider {
        ProviderCode::Codex => windows_codex_script(),
        ProviderCode::Antigravity => windows_antigravity_script(),
        ProviderCode::Cursor => windows_cursor_script(),
        ProviderCode::Claude => windows_claude_script(),
        ProviderCode::OpenCode => format!("$bash=Get-Command bash.exe -ErrorAction SilentlyContinue; $native=$false; if($bash){{$u=& $bash.Source -lc 'uname -s' 2>$null; if($u -match '^(MINGW|MSYS|CYGWIN)'){{$native=$true}}}}; if($native){{Write-Host 'Installing OpenCode with native Windows Bash and curl...'; & $bash.Source -lc 'curl -fsSL https://opencode.ai/install | bash'}} else {{$npm=Get-Command npm.cmd -ErrorAction SilentlyContinue; if(-not $npm){{$npm=Get-Command npm -ErrorAction SilentlyContinue}}; if($npm){{Write-Host 'Installing OpenCode with npm...'; & $npm.Source install -g opencode-ai}} else {{if($bash){{Write-Host 'WSL Bash was found, but it installs the Linux OpenCode binary and cannot be used for this Windows installation.'}}; Write-Host 'OpenCode installation requires Node.js/npm or a native Windows Bash (Git Bash, MSYS2, or Cygwin).'; Write-Host 'Install Node.js or Git Bash, then return to Pedelec and try again.'}}}}; Write-Host ''; Write-Host '{}'", FINISHED),
        _ => unreachable!(),
    };
    InstallerPlan {
        program: "powershell.exe".into(),
        args: vec![
            "-NoLogo".into(),
            "-NoExit".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-Command".into(),
            script,
        ],
        method: match provider {
            ProviderCode::Codex => "codex-windows-auto",
            ProviderCode::Antigravity => "antigravity-windows-auto",
            ProviderCode::Cursor => "cursor-windows-auto",
            ProviderCode::Claude => "claude-windows-auto",
            ProviderCode::OpenCode => "opencode-windows-auto",
            _ => unreachable!(),
        }
        .into(),
    }
}

#[cfg(windows)]
fn windows_cursor_script() -> String {
    format!(
        r#"
$cursorInstallDir = Join-Path $env:LOCALAPPDATA 'cursor-agent'
$cursorCommand = $null
Write-Host 'Installing Cursor CLI...'
try {{
    $ErrorActionPreference = 'Stop'
    irm 'https://cursor.com/install?win32=true' | iex
}} catch {{
    Write-Host "The Cursor installer failed: $($_.Exception.Message)"
}}

if (Test-Path -LiteralPath $cursorInstallDir -PathType Container) {{
    foreach ($cursorCandidate in @(
        'agent.exe', 'agent.cmd', 'agent.ps1',
        'cursor-agent.exe', 'cursor-agent.cmd', 'cursor-agent.ps1'
    )) {{
        $candidatePath = Join-Path $cursorInstallDir $cursorCandidate
        if (Test-Path -LiteralPath $candidatePath -PathType Leaf) {{
            $cursorCommand = $candidatePath
            break
        }}
    }}
    if ($cursorCommand) {{
        Write-Host 'Starting Cursor sign-in...'
        try {{
            & $cursorCommand login
            $loginExitCode = $LASTEXITCODE
            if ($loginExitCode -eq 0) {{
                Write-Host 'Cursor sign-in completed.'
            }} else {{
                Write-Host "Cursor is installed, but sign-in was not completed (exit code $loginExitCode)."
                Write-Host 'You can run agent login again later.'
            }}
        }} catch {{
            Write-Host "Cursor is installed, but sign-in could not be started: $($_.Exception.Message)"
            Write-Host 'You can run agent login again later.'
        }}
    }} else {{
        Write-Host 'Cursor is installed, but this installer could not locate its launcher to start sign-in.'
        Write-Host 'Open a new Terminal and run agent login.'
    }}
    Write-Host '{}'
}} else {{
    Write-Host 'Cursor installation failed: the Cursor CLI installation directory was not found.'
    Write-Host 'Review the errors above, then try again.'
}}
"#,
        CURSOR_FINISHED
    )
}

#[cfg(windows)]
fn windows_claude_script() -> String {
    format!(
        r#"
$claudeCommand = $null
$installerFailed = $false
Write-Host 'Installing Claude Code...'
try {{
    $ErrorActionPreference = 'Stop'
    $global:LASTEXITCODE = 0
    irm https://claude.ai/install.ps1 | iex
    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {{
        throw "Claude installer exited with code $LASTEXITCODE."
    }}
}} catch {{
    $installerFailed = $true
    Write-Host "The Claude Code installer failed: $($_.Exception.Message)"
}}

if ($installerFailed) {{
    Write-Host 'Claude installation failed.'
    Write-Host 'Review the errors above, then try again.'
}} else {{
    $claudeCandidate = Join-Path $env:USERPROFILE '.local\bin\claude.exe'
    if (Test-Path -LiteralPath $claudeCandidate -PathType Leaf) {{
        $claudeCommand = $claudeCandidate
    }} else {{
        $claudeLookup = Get-Command claude -ErrorAction SilentlyContinue
        if ($claudeLookup -and $claudeLookup.CommandType -eq 'Application' -and (Test-Path -LiteralPath $claudeLookup.Source -PathType Leaf)) {{
            $claudeCommand = $claudeLookup.Source
        }}
    }}

    if ($claudeCommand -and (Test-Path -LiteralPath $claudeCommand -PathType Leaf)) {{
        Write-Host 'Claude Code installation completed.'
        Write-Host 'Starting Claude sign-in...'
        try {{
            & $claudeCommand auth login
            $loginExitCode = $LASTEXITCODE
            if ($loginExitCode -eq 0) {{
                Write-Host 'Claude sign-in completed.'
            }} else {{
                Write-Host "Claude is installed, but sign-in was not completed (exit code $loginExitCode)."
                Write-Host 'You can run claude auth login again later.'
            }}
        }} catch {{
            Write-Host "Claude is installed, but sign-in could not be started: $($_.Exception.Message)"
            Write-Host 'You can run claude auth login again later.'
        }}
        Write-Host '{}'
    }} else {{
        Write-Host 'Claude installer ran, but this installer could not locate the Claude executable.'
        Write-Host 'Open a new Terminal and run claude auth login after checking the installation.'
    }}
}}
"#,
        CLAUDE_FINISHED
    )
}

#[cfg(windows)]
fn windows_antigravity_script() -> String {
    format!(
        r#"
$agyCommand = Join-Path $env:LOCALAPPDATA 'agy\bin\agy.exe'
Write-Host 'Installing Antigravity CLI...'
try {{
    $ErrorActionPreference = 'Stop'
    irm https://antigravity.google/cli/install.ps1 | iex
}} catch {{
    Write-Host "The Antigravity installer failed: $($_.Exception.Message)"
}}

if (Test-Path -LiteralPath $agyCommand -PathType Leaf) {{
    Write-Host 'Starting Antigravity sign-in and onboarding...'
    try {{
        & $agyCommand
        $agyExitCode = $LASTEXITCODE
        if ($agyExitCode -eq 0) {{
            Write-Host 'Antigravity sign-in and onboarding flow completed.'
        }} else {{
            Write-Host "Antigravity is installed, but sign-in or onboarding was not completed (exit code $agyExitCode)."
            Write-Host 'You can run agy again later.'
        }}
    }} catch {{
        Write-Host "Antigravity is installed, but could not be started: $($_.Exception.Message)"
        Write-Host 'You can run agy again later.'
    }}
    Write-Host '{}'
}} else {{
    Write-Host 'Antigravity installation failed: agy.exe was not found at the expected location.'
    Write-Host 'Review the errors above, then install Antigravity manually and run agy in a new Terminal.'
}}
"#,
        ANTIGRAVITY_FINISHED
    )
}

#[cfg(windows)]
fn windows_codex_script() -> String {
    r#"
$pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
if (-not $pwsh) {
    $pwsh = Get-Command pwsh -ErrorAction SilentlyContinue
}

$installed = $false
$codexCommand = $null
if ($pwsh) {
    Write-Host 'Installing Codex with PowerShell 7...'
    try {
        & $pwsh.Source -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "`$ErrorActionPreference = 'Stop'; irm https://chatgpt.com/codex/install.ps1 | iex"
        $installerExitCode = $LASTEXITCODE
        if ($installerExitCode -eq 0) {
            $installed = $true
            $codexInstallDir = $env:CODEX_INSTALL_DIR
            if ([string]::IsNullOrWhiteSpace($codexInstallDir)) {
                $codexInstallDir = Join-Path $env:LOCALAPPDATA 'Programs\OpenAI\Codex\bin'
            }
            $codexCandidate = Join-Path $codexInstallDir 'codex.exe'
            if (Test-Path -LiteralPath $codexCandidate -PathType Leaf) {
                $codexCommand = $codexCandidate
            }
        } else {
            Write-Host "The Codex PowerShell installer exited with code $installerExitCode."
        }
    } catch {
        Write-Host "The Codex PowerShell installer could not be started: $($_.Exception.Message)"
    }

    if (-not $installed) {
        Write-Host 'The Codex PowerShell installer failed. Trying npm instead...'
    }
}

if (-not $installed) {
    $npm = Get-Command npm.cmd -ErrorAction SilentlyContinue
    if (-not $npm) {
        $npm = Get-Command npm -ErrorAction SilentlyContinue
    }

    if ($npm) {
        Write-Host 'Installing Codex with npm...'
        try {
            & $npm.Source install -g @openai/codex
            $npmExitCode = $LASTEXITCODE
            if ($npmExitCode -eq 0) {
                $installed = $true
                try {
                    $npmPrefixOutput = & $npm.Source prefix -g
                    $npmPrefixExitCode = $LASTEXITCODE
                    if ($npmPrefixExitCode -eq 0 -and $npmPrefixOutput) {
                        $npmPrefix = ($npmPrefixOutput | Select-Object -First 1).ToString().Trim()
                        if (-not [string]::IsNullOrWhiteSpace($npmPrefix)) {
                            foreach ($codexCandidate in @((Join-Path $npmPrefix 'codex.cmd'), (Join-Path $npmPrefix 'codex.exe'))) {
                                if (Test-Path -LiteralPath $codexCandidate -PathType Leaf) {
                                    $codexCommand = $codexCandidate
                                    break
                                }
                            }
                        }
                    }
                } catch {
                    Write-Host "Could not locate the npm Codex executable: $($_.Exception.Message)"
                }
            } else {
                Write-Host "npm installation failed with exit code $npmExitCode."
            }
        } catch {
            Write-Host "npm could not be started: $($_.Exception.Message)"
        }
    } elseif (-not $pwsh) {
        Write-Host 'Codex could not be installed automatically.'
        Write-Host 'PowerShell 7 and Node.js/npm were not found.'
        Write-Host 'Install PowerShell 7 or Node.js, then return to Pedelec and try again.'
    }
}

if ($installed) {
    Write-Host 'Codex installation command completed.'
    if ($codexCommand -and (Test-Path -LiteralPath $codexCommand -PathType Leaf)) {
        Write-Host 'Starting Codex sign-in...'
        & $codexCommand login
        $loginExitCode = $LASTEXITCODE
        if ($loginExitCode -eq 0) {
            Write-Host 'Codex sign-in completed.'
        } else {
            Write-Host "Codex is installed, but sign-in was not completed (exit code $loginExitCode)."
            Write-Host 'You can run codex login again later.'
        }
    } else {
        Write-Host 'Codex is installed, but this installer could not locate its executable to start sign-in.'
        Write-Host 'Open a new Terminal and run codex login.'
    }
    Write-Host 'Codex installation and sign-in flow has finished. Return to Pedelec and click Restart Pedelec when ready.'
} elseif ($pwsh -or $npm) {
    Write-Host 'Codex installation failed.'
    Write-Host 'Review the errors above, then install PowerShell 7 or Node.js/npm and try again.'
}
"#
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_plan(provider: &ProviderCode) -> InstallerPlan {
    let script = match provider {
        ProviderCode::Codex => format!(r#"if curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh; then
  codex_command=""
  if [ -n "${{CODEX_INSTALL_DIR:-}}" ] && [ -x "$CODEX_INSTALL_DIR/codex" ]; then
    codex_command="$CODEX_INSTALL_DIR/codex"
  elif [ -x "$HOME/.local/bin/codex" ]; then
    codex_command="$HOME/.local/bin/codex"
  else
    codex_command="$(command -v codex 2>/dev/null || true)"
  fi

  if [ -n "$codex_command" ] && [ -x "$codex_command" ]; then
    printf '%s\n' 'Starting Codex sign-in...'
    if "$codex_command" login; then
      printf '%s\n' 'Codex sign-in completed.'
    else
      login_exit_code=$?
      printf '%s\n' "Codex is installed, but sign-in was not completed (exit code $login_exit_code)."
      printf '%s\n' 'You can run codex login again later.'
    fi
  else
    printf '%s\n' 'Codex is installed, but this installer could not locate its executable to start sign-in.'
    printf '%s\n' 'Open a new Terminal and run codex login.'
  fi
  printf '\n%s\n' '{}'
else
  printf '%s\n' 'Codex installation failed. Review the errors above, then try again.'
fi
exec "${{SHELL:-/bin/sh}}" -l"#, CODEX_FINISHED),
        ProviderCode::OpenCode => format!("curl -fsSL https://opencode.ai/install | bash\nprintf '\\n{}\\n'\nexec \"${{SHELL:-/bin/sh}}\" -l", FINISHED),
        ProviderCode::Claude => format!(r#"claude_install_script="$(mktemp "${{TMPDIR:-/tmp}}/pedelec-claude-install.XXXXXX")"
claude_install_success=false
if [ -n "$claude_install_script" ] && curl -fsSL https://claude.ai/install.sh -o "$claude_install_script"; then
  if bash "$claude_install_script"; then
    claude_install_success=true
  fi
fi
if [ -n "$claude_install_script" ]; then
  rm -f "$claude_install_script"
fi

if [ "$claude_install_success" = true ]; then
  claude_command=""
  if [ -x "$HOME/.local/bin/claude" ]; then
    claude_command="$HOME/.local/bin/claude"
  else
    claude_command="$(command -v claude 2>/dev/null || true)"
  fi

  if [ -n "$claude_command" ] && [ -x "$claude_command" ]; then
    printf '%s\n' 'Starting Claude sign-in...'
    if "$claude_command" auth login; then
      printf '%s\n' 'Claude sign-in completed.'
    else
      login_exit_code=$?
      printf '%s\n' "Claude is installed, but sign-in was not completed (exit code $login_exit_code)."
      printf '%s\n' 'You can run claude auth login again later.'
    fi
  else
    printf '%s\n' 'Claude installer ran, but this installer could not locate the Claude executable.'
    printf '%s\n' 'Open a new Terminal and run claude auth login after checking the installation.'
  fi
  printf '\n%s\n' '{}'
else
  printf '%s\n' 'Claude installation failed. Review the errors above, then try again.'
fi
exec "${{SHELL:-/bin/sh}}" -l"#, CLAUDE_FINISHED),
        ProviderCode::Cursor => format!(r#"if curl -fsSL https://cursor.com/install | bash; then
  cursor_command="${{CURSOR_INSTALL_DIR:-$HOME/.local/bin}}/cursor-agent"
  if [ -x "$cursor_command" ]; then
    printf '%s\n' 'Starting Cursor sign-in...'
    if "$cursor_command" login; then
      printf '%s\n' 'Cursor sign-in completed.'
    else
      login_exit_code=$?
      printf '%s\n' "Cursor is installed, but sign-in was not completed (exit code $login_exit_code)."
      printf '%s\n' 'You can run cursor-agent login again later.'
    fi
  else
    printf '%s\n' 'Cursor is installed, but this installer could not locate its executable to start sign-in.'
    printf '%s\n' 'Open a new Terminal and run cursor-agent login.'
  fi
  printf '\n%s\n' '{}'
else
  printf '%s\n' 'Cursor installation failed. Review the errors above, then try again.'
fi
exec "${{SHELL:-/bin/sh}}" -l"#, CURSOR_FINISHED),
        ProviderCode::Antigravity => format!(r#"if curl -fsSL https://antigravity.google/cli/install.sh | bash; then
  agy_command="$HOME/.local/bin/agy"
  if [ -x "$agy_command" ]; then
    printf '%s\n' 'Starting Antigravity sign-in and onboarding...'
    if "$agy_command"; then
      printf '%s\n' 'Antigravity sign-in and onboarding flow completed.'
    else
      agy_exit_code=$?
      printf '%s\n' "Antigravity is installed, but sign-in or onboarding was not completed (exit code $agy_exit_code)."
      printf '%s\n' 'You can run agy again later.'
    fi
    printf '\n%s\n' '{}'
  else
    printf '%s\n' 'Antigravity installation failed: agy was not found at the expected location.'
    printf '%s\n' 'Review the errors above, then install Antigravity manually and run agy in a new Terminal.'
  fi
else
  printf '%s\n' 'Antigravity installation failed. Review the errors above, then try again.'
fi
exec "${{SHELL:-/bin/sh}}" -l"#, ANTIGRAVITY_FINISHED),
        _ => unreachable!(),
    };
    InstallerPlan {
        program: "sh".into(),
        args: vec!["-lc".into(), script],
        method: match provider {
            ProviderCode::Codex => "codex-shell",
            ProviderCode::Antigravity => "antigravity-shell",
            ProviderCode::Cursor => "cursor-shell",
            ProviderCode::OpenCode => "opencode-shell",
            ProviderCode::Claude => "claude-shell",
            _ => unreachable!(),
        }
        .into(),
    }
}

fn unsupported(provider: &ProviderCode) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_INSTALL_UNSUPPORTED,
        "This provider does not support guided installation.",
        json!({"provider": format!("{:?}", provider).to_lowercase(), "platform": std::env::consts::OS}),
    )
}
fn launch_error(provider: &ProviderCode, error: std::io::Error) -> PedelecError {
    PedelecError::with_details(
        error_codes::PROVIDER_INSTALLER_LAUNCH_FAILED,
        "Could not open the provider installer in Terminal.",
        json!({"provider": format!("{:?}", provider).to_lowercase(), "platform": std::env::consts::OS, "error": error.to_string()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_installable_providers() {
        for provider in [ProviderCode::Ollama] {
            assert_eq!(
                installer_plan(&provider).unwrap_err().code,
                error_codes::PROVIDER_INSTALL_UNSUPPORTED
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_plans_are_visible_powershell_installers() {
        let codex = installer_plan(&ProviderCode::Codex).unwrap();
        assert_eq!(codex.program, "powershell.exe");
        assert_eq!(codex.method, "codex-windows-auto");
        assert_eq!(CREATE_NEW_CONSOLE, 0x0000_0010);
        assert!(codex.args.contains(&"-NoExit".to_string()));
        let codex_script = codex.args.last().unwrap();
        assert!(codex_script.contains("Get-Command pwsh.exe"));
        assert!(codex_script.contains("Get-Command pwsh -ErrorAction SilentlyContinue"));
        assert!(codex_script
            .contains("& $pwsh.Source -NoLogo -NoProfile -ExecutionPolicy Bypass -Command"));
        assert!(codex_script.contains("chatgpt.com/codex/install.ps1 | iex"));
        assert!(codex_script.contains("$codexCommand = $null"));
        assert!(codex_script.contains("Programs\\OpenAI\\Codex\\bin"));
        assert!(codex_script.contains("codex.exe"));
        assert!(codex_script.contains("Get-Command npm.cmd"));
        assert!(codex_script.contains("Get-Command npm -ErrorAction SilentlyContinue"));
        assert!(codex_script.contains("& $npm.Source install -g @openai/codex"));
        assert!(codex_script.contains("& $npm.Source prefix -g"));
        assert!(codex_script.contains("codex.cmd"));
        assert!(codex_script.contains("& $codexCommand login"));
        assert!(codex_script.contains("Codex sign-in completed."));
        assert!(codex_script.contains("sign-in was not completed"));
        assert!(codex_script.contains("could not locate its executable to start sign-in"));
        assert!(
            codex_script.contains("The Codex PowerShell installer failed. Trying npm instead...")
        );
        assert!(codex_script.contains("PowerShell 7 and Node.js/npm were not found."));
        assert!(codex_script.contains("Codex installation failed."));
        assert!(codex_script
            .contains("if ($installed) {\n    Write-Host 'Codex installation command completed.'"));
        assert!(
            codex_script.find("if ($installed) {").unwrap()
                < codex_script.find("& $codexCommand login").unwrap()
        );
        let opencode = installer_plan(&ProviderCode::OpenCode).unwrap();
        let opencode_script = opencode.args.last().unwrap();
        assert!(opencode_script.contains("MINGW|MSYS|CYGWIN"));
        assert!(!opencode_script.contains("command -v curl"));
        assert!(opencode_script.contains("Installing OpenCode with native Windows Bash and curl"));
        assert!(opencode_script.contains("WSL Bash was found"));
        assert!(opencode_script.contains("install -g opencode-ai"));
        assert!(!opencode_script.contains("codex login"));

        let cursor = installer_plan(&ProviderCode::Cursor).unwrap();
        assert_eq!(cursor.program, "powershell.exe");
        assert_eq!(cursor.method, "cursor-windows-auto");
        let cursor_script = cursor.args.last().unwrap();
        assert!(cursor_script.contains("irm 'https://cursor.com/install?win32=true' | iex"));
        assert!(cursor_script.contains("Join-Path $env:LOCALAPPDATA 'cursor-agent'"));
        assert!(cursor_script.contains("'agent.exe', 'agent.cmd', 'agent.ps1'"));
        assert!(
            cursor_script.contains("'cursor-agent.exe', 'cursor-agent.cmd', 'cursor-agent.ps1'")
        );
        assert!(cursor_script.contains("& $cursorCommand login"));
        assert!(cursor_script.contains("Starting Cursor sign-in"));
        assert!(cursor_script.contains("sign-in was not completed"));
        assert!(cursor_script.contains("could not locate its launcher to start sign-in"));
        assert!(cursor_script.contains(CURSOR_FINISHED));
        assert!(!cursor_script.contains("bash.exe"));
        assert!(!cursor_script.contains("WSL Bash"));
        assert!(!cursor_script.contains("curl -fsSL"));

        let antigravity = installer_plan(&ProviderCode::Antigravity).unwrap();
        assert_eq!(antigravity.program, "powershell.exe");
        assert_eq!(antigravity.method, "antigravity-windows-auto");
        assert!(antigravity.args.contains(&"-NoExit".to_string()));
        let antigravity_script = antigravity.args.last().unwrap();
        assert!(antigravity_script.contains("https://antigravity.google/cli/install.ps1 | iex"));
        assert!(antigravity_script.contains("$env:LOCALAPPDATA"));
        assert!(antigravity_script.contains("agy\\bin\\agy.exe"));
        assert!(antigravity_script.contains("Test-Path -LiteralPath $agyCommand -PathType Leaf"));
        assert!(antigravity_script.contains("& $agyCommand"));
        assert!(antigravity_script.contains(ANTIGRAVITY_FINISHED));
        assert!(!antigravity_script.contains("codex login"));
        assert!(!antigravity_script.contains("install -g opencode-ai"));

        let claude = installer_plan(&ProviderCode::Claude).unwrap();
        assert_eq!(claude.program, "powershell.exe");
        assert_eq!(claude.method, "claude-windows-auto");
        assert!(claude.args.contains(&"-NoExit".to_string()));
        let claude_script = claude.args.last().unwrap();
        assert!(claude_script.contains("irm https://claude.ai/install.ps1 | iex"));
        assert!(claude_script.contains("$env:USERPROFILE"));
        assert!(claude_script.contains(".local\\bin\\claude.exe"));
        assert!(claude_script.contains("Get-Command claude -ErrorAction SilentlyContinue"));
        assert!(claude_script.contains("claude auth login"));
        assert!(claude_script.contains("Claude sign-in completed."));
        assert!(claude_script.contains("Claude is installed, but sign-in was not completed"));
        assert!(claude_script.contains("could not locate the Claude executable"));
        assert!(claude_script.contains("Claude installation failed."));
        assert!(claude_script.contains("$installerFailed = $false"));
        assert!(claude_script.contains("$global:LASTEXITCODE = 0"));
        assert!(claude_script.contains("if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0)"));
        assert!(claude_script
            .contains("if ($installerFailed) {\n    Write-Host 'Claude installation failed.'"));
        assert!(claude_script.contains("} else {\n    $claudeCandidate ="));
        assert!(
            claude_script
                .find("irm https://claude.ai/install.ps1 | iex")
                .unwrap()
                < claude_script.find("claude auth login").unwrap()
        );
        assert!(!claude_script.contains("install -g @anthropic-ai/claude-code"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn shell_plans_run_codex_sign_in_only_after_a_successful_install() {
        let codex = installer_plan(&ProviderCode::Codex).unwrap();
        assert_eq!(codex.method, "codex-shell");
        let codex_script = codex.args.last().unwrap();
        assert!(codex_script
            .contains("https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh"));
        assert!(!codex_script
            .contains("CODEX_NON_INTERACTIVE=1 curl -fsSL https://chatgpt.com/codex/install.sh"));
        assert!(codex_script.contains("[ -n \"${CODEX_INSTALL_DIR:-}\" ]"));
        assert!(codex_script.contains("[ -x \"$CODEX_INSTALL_DIR/codex\" ]"));
        assert!(codex_script.contains("[ -x \"$HOME/.local/bin/codex\" ]"));
        assert!(codex_script.contains("command -v codex 2>/dev/null"));
        assert!(codex_script.contains("[ -n \"$codex_command\" ] && [ -x \"$codex_command\" ]"));
        assert!(codex_script.contains("if \"$codex_command\" login; then"));
        assert!(codex_script.contains("Codex sign-in completed."));
        assert!(codex_script.contains("sign-in was not completed"));
        assert!(codex_script.contains("if curl -fsSL"));
        assert!(codex_script.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        assert!(!codex_script.contains("https://chatgpt.com/codex/install.sh | sh"));
        assert!(
            codex_script
                .find("if curl -fsSL https://chatgpt.com/codex/install.sh")
                .unwrap()
                < codex_script
                    .find("if \"$codex_command\" login; then")
                    .unwrap()
        );
        assert!(
            codex_script.find("CODEX_NON_INTERACTIVE=1 sh").unwrap()
                < codex_script
                    .find("if [ -n \"${CODEX_INSTALL_DIR:-}\" ]")
                    .unwrap()
        );
        assert!(
            codex_script
                .find("[ -n \"${CODEX_INSTALL_DIR:-}\" ]")
                .unwrap()
                < codex_script
                    .find("[ -x \"$HOME/.local/bin/codex\" ]")
                    .unwrap()
        );
        assert!(
            codex_script
                .find("[ -x \"$HOME/.local/bin/codex\" ]")
                .unwrap()
                < codex_script.find("command -v codex 2>/dev/null").unwrap()
        );
        assert!(
            codex_script.find("command -v codex 2>/dev/null").unwrap()
                < codex_script
                    .find("if \"$codex_command\" login; then")
                    .unwrap()
        );

        let opencode = installer_plan(&ProviderCode::OpenCode).unwrap();
        assert_eq!(opencode.method, "opencode-shell");
        let opencode_script = opencode.args.last().unwrap();
        assert!(opencode_script.contains("https://opencode.ai/install | bash"));
        assert!(!opencode_script.contains("codex login"));
        assert!(!opencode_script.contains("CODEX_INSTALL_DIR"));
        assert!(opencode_script.contains("exec \"${SHELL:-/bin/sh}\" -l"));

        let cursor = installer_plan(&ProviderCode::Cursor).unwrap();
        assert_eq!(cursor.method, "cursor-shell");
        let cursor_script = cursor.args.last().unwrap();
        assert!(cursor_script.contains("https://cursor.com/install | bash"));
        assert!(cursor_script.contains("${CURSOR_INSTALL_DIR:-$HOME/.local/bin}/cursor-agent"));
        assert!(cursor_script.contains("if \"$cursor_command\" login; then"));
        assert!(cursor_script.contains("Cursor sign-in completed."));
        assert!(cursor_script.contains("sign-in was not completed"));
        assert!(cursor_script.contains(CURSOR_FINISHED));
        assert!(cursor_script.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        assert!(!cursor_script.contains("codex login"));
        assert!(!cursor_script.contains("/agent\""));
        assert!(!cursor_script.contains(" run agent login"));

        let antigravity = installer_plan(&ProviderCode::Antigravity).unwrap();
        assert_eq!(antigravity.method, "antigravity-shell");
        let antigravity_script = antigravity.args.last().unwrap();
        assert!(antigravity_script.contains("https://antigravity.google/cli/install.sh | bash"));
        assert!(antigravity_script.contains("$HOME/.local/bin/agy"));
        assert!(antigravity_script.contains("[ -x \"$agy_command\" ]"));
        assert!(antigravity_script.contains("\"$agy_command\""));
        assert!(antigravity_script.contains(ANTIGRAVITY_FINISHED));
        assert!(antigravity_script.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        assert!(!antigravity_script.contains("codex login"));
        assert!(!antigravity_script.contains("https://opencode.ai/install"));

        let claude = installer_plan(&ProviderCode::Claude).unwrap();
        assert_eq!(claude.method, "claude-shell");
        let claude_script = claude.args.last().unwrap();
        assert!(claude_script.contains(
            "claude_install_script=\"$(mktemp \"${TMPDIR:-/tmp}/pedelec-claude-install.XXXXXX\")\""
        ));
        assert!(claude_script
            .contains("curl -fsSL https://claude.ai/install.sh -o \"$claude_install_script\""));
        assert!(claude_script.contains("bash \"$claude_install_script\""));
        assert!(claude_script.contains("rm -f \"$claude_install_script\""));
        assert!(!claude_script.contains("if curl -fsSL https://claude.ai/install.sh | bash; then"));
        assert!(claude_script.contains("$HOME/.local/bin/claude"));
        assert!(claude_script.contains("command -v claude 2>/dev/null"));
        assert!(claude_script.contains("if \"$claude_command\" auth login; then"));
        assert!(claude_script.contains("Claude sign-in completed."));
        assert!(claude_script.contains("Claude is installed, but sign-in was not completed"));
        assert!(claude_script.contains("could not locate the Claude executable"));
        assert!(claude_script.contains("Claude installation failed."));
        assert!(claude_script.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        assert!(
            claude_script
                .find("if [ \"$claude_install_success\" = true ]; then")
                .unwrap()
                < claude_script
                    .find("if \"$claude_command\" auth login; then")
                    .unwrap()
        );
        assert!(
            claude_script
                .find("curl -fsSL https://claude.ai/install.sh -o \"$claude_install_script\"")
                .unwrap()
                < claude_script
                    .find("bash \"$claude_install_script\"")
                    .unwrap()
        );
        assert!(
            claude_script
                .find("bash \"$claude_install_script\"")
                .unwrap()
                < claude_script
                    .find("if [ \"$claude_install_success\" = true ]; then")
                    .unwrap()
        );
        assert!(!claude_script.contains("npm install"));
    }
}
