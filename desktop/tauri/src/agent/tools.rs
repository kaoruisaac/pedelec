use super::config::AgentConfig;
use super::error::AgentError;
use super::model::ModelAttachment;
use super::sandbox::Sandbox;
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn tool_definitions(vision: bool) -> Value {
    let mut definitions = serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "fs.list_text_files",
                "description": "List readable UTF-8 text files inside the sandbox.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "dir": { "type": "string", "default": "." },
                        "maxDepth": { "type": "integer", "default": 3 }
                    },
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a restricted Pedelec CLI command. This is not a full shell; only pedelec-cli tool-spec and pedelec-cli tool-call commands are allowed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeoutMs": { "type": "integer" }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "fs.read_text_file",
                "description": "Read one UTF-8 text file inside the sandbox.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
    ]);
    if vision {
        definitions.as_array_mut().unwrap().extend(serde_json::json!([
        {"type":"function","function":{"name":"fs.list_image_files","description":"List supported PNG, JPEG, and WebP images inside the sandbox.","parameters":{"type":"object","properties":{"dir":{"type":"string","default":"."},"maxDepth":{"type":"integer","default":3}},"additionalProperties":false}}},
        {"type":"function","function":{"name":"fs.read_image","description":"Read one sandbox image so it can be viewed.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}
    ]).as_array().unwrap().iter().cloned());
    }
    definitions
}

#[derive(Debug)]
pub struct ToolExecutionResult {
    pub content: Value,
    pub attachments: Vec<ModelAttachment>,
}

pub fn execute_tool(
    tool: &str,
    args: &Value,
    _session_id: &str,
    sandbox: &Sandbox,
    config: &AgentConfig,
) -> Result<ToolExecutionResult, AgentError> {
    match tool {
        "fs.list_text_files" => {
            let dir = args.get("dir").and_then(Value::as_str).unwrap_or(".");
            let max_depth = args
                .get("maxDepth")
                .and_then(Value::as_u64)
                .unwrap_or(3)
                .min(16) as usize;
            let files = sandbox.list_text_files(dir, max_depth)?;
            Ok(ToolExecutionResult {
                content: serde_json::json!({ "files": files }),
                attachments: vec![],
            })
        }
        "fs.read_text_file" => {
            let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
                AgentError::new("INVALID_ARGUMENT", "fs.read_text_file requires path")
            })?;
            let (text, truncated) = sandbox.read_text_file(path)?;
            Ok(ToolExecutionResult {
                content: serde_json::json!({ "path": path, "text": text, "truncated": truncated }),
                attachments: vec![],
            })
        }
        "fs.list_image_files" => {
            let dir = args.get("dir").and_then(Value::as_str).unwrap_or(".");
            let depth = args
                .get("maxDepth")
                .and_then(Value::as_u64)
                .unwrap_or(3)
                .min(16) as usize;
            let (files, truncated) = sandbox.list_image_files(dir, depth)?;
            Ok(ToolExecutionResult {
                content: serde_json::json!({"files":files,"truncated":truncated}),
                attachments: vec![],
            })
        }
        "fs.read_image" => {
            let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
                AgentError::new("INVALID_ARGUMENT", "fs.read_image requires path")
            })?;
            let (info, bytes) = sandbox.read_image(path)?;
            Ok(ToolExecutionResult {
                content: serde_json::to_value(&info).unwrap(),
                attachments: vec![ModelAttachment::Image {
                    media_type: info.media_type,
                    bytes,
                }],
            })
        }
        "bash" => Ok(ToolExecutionResult {
            content: bash_tool(args, config)?,
            attachments: vec![],
        }),
        _ => Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Unknown tool",
            serde_json::json!({ "tool": tool }),
        )),
    }
}

fn bash_tool(args: &Value, config: &AgentConfig) -> Result<Value, AgentError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| AgentError::new("INVALID_ARGUMENT", "bash requires command"))?;
    let timeout_ms = args
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(config.pedelec_cli_timeout_ms);
    let argv = parse_restricted_bash_command(command)?;
    validate_pedelec_cli_command(&argv)?;
    let cli_path = resolve_pedelec_cli(config)?;
    let mut process = Command::new(cli_path);
    process
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(runtime_file) = &config.core_runtime_file {
        process.env("PEDELEC_CORE_IPC_RUNTIME_FILE", runtime_file);
    }
    run_pedelec_cli_command(process, timeout_ms)
}

fn parse_restricted_bash_command(command: &str) -> Result<Vec<String>, AgentError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "command must be a non-empty pedelec-cli command.",
        ));
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    let mut quote: Option<char> = None;
    let mut token_started = false;

    while let Some(ch) = chars.next() {
        match quote {
            Some(quote_char) => {
                if ch == quote_char {
                    quote = None;
                } else if quote_char == '"' && ch == '$' {
                    return Err(unsupported_shell_syntax(
                        "environment variable expansion is not supported.",
                    ));
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    token_started = true;
                }
                ch if ch.is_whitespace() => {
                    if token_started {
                        args.push(std::mem::take(&mut current));
                        token_started = false;
                    }
                    while chars.peek().is_some_and(|value| value.is_whitespace()) {
                        chars.next();
                    }
                }
                '|' | '>' | '<' | ';' | '&' => {
                    return Err(unsupported_shell_syntax(
                        "pipes, redirects, command chaining, and background commands are not supported.",
                    ));
                }
                '$' => {
                    return Err(unsupported_shell_syntax(
                        "environment variable expansion and command substitution are not supported.",
                    ));
                }
                _ => {
                    token_started = true;
                    current.push(ch);
                }
            },
        }
    }

    if let Some(quote_char) = quote {
        return Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "command contains an unterminated quote.",
            serde_json::json!({ "quote": quote_char }),
        ));
    }
    if token_started {
        args.push(current);
    }
    if args.is_empty() {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "command must be a non-empty pedelec-cli command.",
        ));
    }
    Ok(args)
}

fn unsupported_shell_syntax(message: &str) -> AgentError {
    AgentError::new(
        "UNSUPPORTED_SHELL_SYNTAX",
        format!("{message} Use only: pedelec-cli tool-spec <tool_name> or pedelec-cli tool-call <tool_name> ..."),
    )
}

fn validate_pedelec_cli_command(argv: &[String]) -> Result<(), AgentError> {
    if argv.first().map(String::as_str) != Some("pedelec-cli") {
        return Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only pedelec-cli tool commands are allowed.",
            serde_json::json!({ "allowed": [
                "pedelec-cli tool-spec <tool_name>",
                "pedelec-cli tool-call <tool_name> ..."
            ] }),
        ));
    }

    match argv.get(1).map(String::as_str) {
        Some("tool-spec") if argv.len() == 3 && !argv[2].trim().is_empty() => Ok(()),
        Some("tool-spec") => Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-cli tool-spec <tool_name>",
        )),
        Some("tool-call") if argv.len() >= 3 && !argv[2].trim().is_empty() => Ok(()),
        Some("tool-call") => Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-cli tool-call <tool_name> ...",
        )),
        _ => Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only pedelec-cli tool-spec and pedelec-cli tool-call commands are allowed.",
            serde_json::json!({ "command": argv }),
        )),
    }
}

fn run_pedelec_cli_command(mut command: Command, timeout_ms: u64) -> Result<Value, AgentError> {
    let timed_output = run_command_with_timeout(&mut command, timeout_ms).map_err(|err| {
        AgentError::with_details(
            "PEDELEC_CLI_FAILED",
            "Failed to execute pedelec-cli",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let output = timed_output.output;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if timed_output.timed_out {
        return Err(AgentError::with_details(
            "PEDELEC_CLI_TIMEOUT",
            "pedelec-cli timed out.",
            serde_json::json!({
                "timeoutMs": timeout_ms,
                "stdout": stdout,
                "stderr": stderr
            }),
        ));
    }
    if !output.status.success() {
        return Err(AgentError::with_details(
            "PEDELEC_CLI_FAILED",
            "pedelec-cli exited with an error",
            serde_json::json!({
                "status": output.status.code(),
                "stdout": stdout,
                "stderr": stderr
            }),
        ));
    }
    serde_json::from_str::<Value>(&stdout).map_err(|err| {
        AgentError::with_details(
            "PEDELEC_CLI_FAILED",
            "pedelec-cli stdout was not valid JSON",
            serde_json::json!({ "stdout": stdout, "error": err.to_string() }),
        )
    })
}

struct TimedOutput {
    output: Output,
    timed_out: bool,
}

fn run_command_with_timeout(
    command: &mut Command,
    timeout_ms: u64,
) -> Result<TimedOutput, std::io::Error> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(|output| TimedOutput {
                output,
                timed_out: false,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child.wait_with_output().map(|output| TimedOutput {
                output,
                timed_out: true,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn resolve_pedelec_cli(config: &AgentConfig) -> Result<PathBuf, AgentError> {
    if let Some(path) = &config.pedelec_cli_path {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    find_on_path("pedelec-cli")
        .ok_or_else(|| AgentError::new("PEDELEC_CLI_NOT_FOUND", "Cannot find pedelec-cli."))
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        for candidate in candidates(&dir, program) {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn candidates(dir: &Path, program: &str) -> Vec<PathBuf> {
    let mut values = vec![dir.join(program)];
    #[cfg(windows)]
    {
        values.push(dir.join(format!("{program}.exe")));
        values.push(dir.join(format!("{program}.cmd")));
        values.push(dir.join(format!("{program}.bat")));
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{AgentConfig, ModelProvider};

    fn config(sandbox: PathBuf) -> AgentConfig {
        AgentConfig {
            provider: ModelProvider::Ollama,
            provider_name: "ollama".into(),
            model: "fake".into(),
            ollama_base_url: "http://127.0.0.1:1".into(),
            ollama_timeout_ms: 1000,
            ollama_api_key: "ollama".into(),
            sandbox,
            pedelec_cli_path: None,
            core_runtime_file: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            max_image_bytes: 20 * 1024 * 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    #[test]
    fn filesystem_tool_works_without_host_routing_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello").unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config(temp.path().to_path_buf());

        let result = execute_tool(
            "fs.read_text_file",
            &serde_json::json!({ "path": "README.md" }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap();

        assert_eq!(result.content["text"], "hello");
    }

    #[test]
    fn tool_definitions_expose_bash_not_old_native_host_tools() {
        let tools = tool_definitions(false).to_string();

        assert!(tools.contains("\"name\":\"bash\""));
        assert!(!tools.contains("pedelec_cli.tool_spec"));
        assert!(!tools.contains("pedelec_cli.tool_call"));
    }

    #[test]
    fn bash_tool_does_not_pass_session_id_to_pedelec_cli() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("args.txt");
        let cli = fake_pedelec_cli(temp.path(), &capture);
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let mut cfg = config(temp.path().to_path_buf());
        cfg.pedelec_cli_path = Some(cli);

        let result = execute_tool(
            "bash",
            &serde_json::json!({
                "command": "pedelec-cli tool-call get_page '{\"id\":1}'"
            }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap();

        assert_eq!(result.content["ok"], true);
        let args = std::fs::read_to_string(capture).unwrap();
        assert!(args.contains("tool-call"));
        assert!(args.contains("get_page"));
        assert!(args.contains("id"));
        assert!(args.contains("1"));
        assert!(!args.contains("thread_outer"));
        assert!(!args.contains("session_inner"));
    }

    #[test]
    fn bash_tool_rejects_non_pedelec_cli_commands() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config(temp.path().to_path_buf());

        let err = execute_tool(
            "bash",
            &serde_json::json!({ "command": "ls" }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap_err();

        assert_eq!(err.code, "COMMAND_NOT_ALLOWED");
    }

    #[test]
    fn bash_tool_rejects_unsupported_shell_syntax() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config(temp.path().to_path_buf());

        let err = execute_tool(
            "bash",
            &serde_json::json!({ "command": "pedelec-cli tool-spec foo && rm -rf /" }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap_err();

        assert_eq!(err.code, "UNSUPPORTED_SHELL_SYNTAX");
    }

    #[test]
    fn parses_single_and_double_quoted_arguments() {
        let single = parse_restricted_bash_command(
            "pedelec-cli tool-call ask_user '{\"question\":\"要繼續嗎？\"}'",
        )
        .unwrap();
        assert_eq!(
            single,
            vec![
                "pedelec-cli",
                "tool-call",
                "ask_user",
                "{\"question\":\"要繼續嗎？\"}"
            ]
        );

        let double =
            parse_restricted_bash_command("pedelec-cli tool-spec \"get current page\"").unwrap();
        assert_eq!(double, vec!["pedelec-cli", "tool-spec", "get current page"]);
    }

    #[test]
    fn parser_rejects_command_substitution() {
        let err = parse_restricted_bash_command("pedelec-cli tool-spec $(cat secret)").unwrap_err();

        assert_eq!(err.code, "UNSUPPORTED_SHELL_SYNTAX");
    }

    #[test]
    fn old_native_host_tool_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config(temp.path().to_path_buf());

        let err = execute_tool(
            "pedelec_cli.tool_call",
            &serde_json::json!({ "toolName": "get_page", "args": {} }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap_err();

        assert_eq!(err.code, "INVALID_ARGUMENT");
    }

    #[cfg(windows)]
    fn fake_pedelec_cli(dir: &Path, capture: &Path) -> PathBuf {
        let path = dir.join("pedelec-cli.cmd");
        std::fs::write(
            &path,
            format!(
                "@echo off\r\necho %* > \"{}\"\r\necho {{\"ok\":true}}\r\n",
                capture.to_string_lossy()
            ),
        )
        .unwrap();
        path
    }

    #[cfg(not(windows))]
    fn fake_pedelec_cli(dir: &Path, capture: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("pedelec-cli");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"ok\":true}}'\n",
                capture.to_string_lossy()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }
}
