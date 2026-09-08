use super::config::ToolHostConfig;
use super::conversation::InferenceAttachment;
use super::error::AgentError;
use super::sandbox::Sandbox;
use super::tavily::TavilyRoundWrapper;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> AgentToolDefinition {
    AgentToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

pub fn agent_tool_definitions(vision: bool, web_search_enabled: bool) -> Vec<AgentToolDefinition> {
    let mut definitions = vec![
        tool_def(
            "fs.list_text_files",
            "List readable UTF-8 text files inside the sandbox.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "default": "." },
                    "maxDepth": { "type": "integer", "default": 3 }
                },
                "additionalProperties": false
            }),
        ),
        tool_def(
            "bash",
            "Run a restricted Pedelec helper command. This is not a full shell. It permits Pedelec App Tool commands through `pedelec-cli` and JavaScript/TypeScript script execution through `pedelec-deno`. Allowed forms are `pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>`, `pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ...`, `pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path>`, or that `pedelec-deno` form followed by `-- <script-args...>`.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeoutMs": { "type": "integer" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        tool_def(
            "fs.read_text_file",
            "Read one UTF-8 text file inside the sandbox.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
    ];
    if vision {
        definitions.push(tool_def(
            "fs.list_image_files",
            "List supported PNG, JPEG, and WebP images inside the sandbox.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "default": "." },
                    "maxDepth": { "type": "integer", "default": 3 }
                },
                "additionalProperties": false
            }),
        ));
        definitions.push(tool_def(
            "fs.read_image",
            "Read one sandbox image so it can be viewed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ));
    }
    if web_search_enabled {
        definitions.push(tool_def(
            "web.search",
            "Search the public web for current, recent, or externally verifiable information. Use it only when web information would materially improve the answer.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A focused web search query."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ));
    }
    definitions
}

#[derive(Debug)]
pub struct ToolExecutionResult {
    pub content: Value,
    pub attachments: Vec<InferenceAttachment>,
}

#[allow(dead_code)]
pub fn execute_tool(
    tool: &str,
    args: &Value,
    session_id: &str,
    sandbox: &Sandbox,
    config: &ToolHostConfig,
) -> Result<ToolExecutionResult, AgentError> {
    execute_tool_with_tavily(tool, args, session_id, sandbox, config, None)
}

pub fn execute_tool_with_tavily(
    tool: &str,
    args: &Value,
    _session_id: &str,
    sandbox: &Sandbox,
    config: &ToolHostConfig,
    tavily: Option<&mut TavilyRoundWrapper<'_>>,
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
                attachments: vec![InferenceAttachment {
                    media_type: info.media_type,
                    bytes,
                }],
            })
        }
        "bash" => Ok(ToolExecutionResult {
            content: bash_tool(args, config)?,
            attachments: vec![],
        }),
        "web.search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::new("INVALID_ARGUMENT", "web.search requires query"))?;
            let content = tavily
                .ok_or_else(|| {
                    AgentError::new("WEB_SEARCH_UNAVAILABLE", "Web search is not configured.")
                })?
                .search(query)?;
            Ok(ToolExecutionResult {
                content,
                attachments: vec![],
            })
        }
        _ => Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Unknown tool",
            serde_json::json!({ "tool": tool }),
        )),
    }
}

fn bash_tool(args: &Value, config: &ToolHostConfig) -> Result<Value, AgentError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| AgentError::new("INVALID_ARGUMENT", "bash requires command"))?;
    let requested_timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);
    let argv = parse_restricted_bash_command(command)?;
    match validate_restricted_command(&argv)? {
        RestrictedCommand::PedelecCli => {
            let timeout_ms = requested_timeout_ms.unwrap_or(config.pedelec_cli_timeout_ms);
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
        RestrictedCommand::PedelecDeno => {
            let timeout_ms = requested_timeout_ms
                .unwrap_or(config.pedelec_deno_timeout_ms)
                .max(config.pedelec_deno_timeout_ms);
            let deno_path = resolve_pedelec_deno(config)?;
            let mut process = Command::new(deno_path);
            process
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(runtime_file) = &config.core_runtime_file {
                process.env("PEDELEC_CORE_IPC_RUNTIME_FILE", runtime_file);
            }
            run_pedelec_deno_command(process, timeout_ms)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestrictedCommand {
    PedelecCli,
    PedelecDeno,
}

fn parse_restricted_bash_command(command: &str) -> Result<Vec<String>, AgentError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "command must be a non-empty restricted Pedelec helper command.",
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
            "command must be a non-empty restricted Pedelec helper command.",
        ));
    }
    Ok(args)
}

fn unsupported_shell_syntax(message: &str) -> AgentError {
    AgentError::new(
        "UNSUPPORTED_SHELL_SYNTAX",
        format!(
            "{message} Use only restricted Pedelec helpers: pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>, pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ..., or pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path> [-- <script-args...>]."
        ),
    )
}

fn validate_restricted_command(argv: &[String]) -> Result<RestrictedCommand, AgentError> {
    match argv.first().map(String::as_str) {
        Some("pedelec-cli") => {
            validate_pedelec_cli_command(argv)?;
            Ok(RestrictedCommand::PedelecCli)
        }
        Some("pedelec-deno") => {
            validate_pedelec_deno_command(argv)?;
            Ok(RestrictedCommand::PedelecDeno)
        }
        _ => Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only restricted Pedelec helper commands are allowed; use pedelec-cli for App Tools or pedelec-deno for JavaScript/TypeScript execution.",
            serde_json::json!({ "allowed": restricted_command_usage() }),
        )),
    }
}

fn restricted_command_usage() -> [&'static str; 3] {
    [
        "pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>",
        "pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ...",
        "pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path> [-- <script-args...>]",
    ]
}

fn validate_pedelec_cli_command(argv: &[String]) -> Result<(), AgentError> {
    if argv.first().map(String::as_str) != Some("pedelec-cli") {
        return Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only restricted Pedelec helper commands are allowed; use pedelec-cli for App Tools or pedelec-deno for JavaScript/TypeScript execution.",
            serde_json::json!({ "allowed": [
                "pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>",
                "pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ...",
                "pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path> [-- <script-args...>]"
            ] }),
        ));
    }

    if argv.get(1).map(String::as_str) != Some("--thread-id") {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name> OR pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ...",
        ));
    }
    if argv
        .get(2)
        .map(String::as_str)
        .is_none_or(|thread_id| thread_id.trim().is_empty())
    {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "pedelec-cli requires a non-empty --thread-id value.",
        ));
    }

    match argv.get(3).map(String::as_str) {
        Some("tool-spec") if argv.len() == 5 && !argv[4].trim().is_empty() => Ok(()),
        Some("tool-spec") => Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>",
        )),
        Some("tool-call") if argv.len() >= 5 && !argv[4].trim().is_empty() => Ok(()),
        Some("tool-call") => Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> ...",
        )),
        _ => Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only pedelec-cli App Tool commands or pedelec-deno JavaScript/TypeScript commands are allowed.",
            serde_json::json!({ "command": argv }),
        )),
    }
}

fn validate_pedelec_deno_command(argv: &[String]) -> Result<(), AgentError> {
    if argv.get(1).map(String::as_str) != Some("--thread-id") {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path> [-- <script-args...>]",
        ));
    }
    if argv
        .get(2)
        .map(String::as_str)
        .is_none_or(|thread_id| thread_id.trim().is_empty())
    {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "pedelec-deno requires a non-empty --thread-id value.",
        ));
    }
    if argv.get(3).map(String::as_str) != Some("run") {
        return Err(AgentError::with_details(
            "COMMAND_NOT_ALLOWED",
            "Only the pedelec-deno `run` subcommand is allowed.",
            serde_json::json!({ "allowed": restricted_command_usage() }),
        ));
    }

    let entrypoint = argv.get(4).ok_or_else(|| {
        AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script-path> [-- <script-args...>]",
        )
    })?;
    validate_workspace_relative_entrypoint(entrypoint)?;

    match argv.get(5) {
        None => Ok(()),
        Some(separator) if separator == "--" => Ok(()),
        Some(_) => Err(AgentError::new(
            "COMMAND_NOT_ALLOWED",
            "Raw Deno options are not allowed; put script arguments after `--`.",
        )),
    }
}

fn validate_workspace_relative_entrypoint(entrypoint: &str) -> Result<(), AgentError> {
    if entrypoint.trim().is_empty()
        || entrypoint.starts_with('-')
        || entrypoint.chars().any(char::is_control)
    {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "pedelec-deno requires a non-empty workspace-relative script path.",
        ));
    }

    let path = Path::new(entrypoint);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "pedelec-deno requires a workspace-relative script path without traversal.",
        ));
    }
    Ok(())
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

fn run_pedelec_deno_command(mut command: Command, timeout_ms: u64) -> Result<Value, AgentError> {
    let timed_output = run_command_with_timeout(&mut command, timeout_ms).map_err(|err| {
        AgentError::with_details(
            "PEDELEC_DENO_FAILED",
            "Failed to execute pedelec-deno",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let output = timed_output.output;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if timed_output.timed_out {
        return Err(AgentError::with_details(
            "PEDELEC_DENO_TIMEOUT",
            "pedelec-deno helper command timed out before returning its structured result.",
            serde_json::json!({
                "timeoutMs": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
            }),
        ));
    }
    if !output.status.success() {
        return Err(AgentError::with_details(
            "PEDELEC_DENO_FAILED",
            "pedelec-deno exited with an error",
            serde_json::json!({
                "status": output.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            }),
        ));
    }
    serde_json::from_str::<Value>(&stdout).map_err(|err| {
        AgentError::with_details(
            "PEDELEC_DENO_FAILED",
            "pedelec-deno stdout was not a structured JSON response",
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

fn resolve_pedelec_cli(config: &ToolHostConfig) -> Result<PathBuf, AgentError> {
    if let Some(path) = &config.pedelec_cli_path {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    find_on_path("pedelec-cli")
        .ok_or_else(|| AgentError::new("PEDELEC_CLI_NOT_FOUND", "Cannot find pedelec-cli."))
}

fn resolve_pedelec_deno(config: &ToolHostConfig) -> Result<PathBuf, AgentError> {
    if let Some(path) = &config.pedelec_deno_path {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    find_on_path("pedelec-deno").ok_or_else(|| {
        AgentError::new(
            "PEDELEC_DENO_NOT_FOUND",
            "Cannot find pedelec-deno; do not fall back to another JavaScript runtime.",
        )
    })
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

    fn config() -> ToolHostConfig {
        ToolHostConfig {
            pedelec_cli_path: None,
            pedelec_deno_path: None,
            core_runtime_file: None,
            pedelec_cli_timeout_ms: 1000,
            pedelec_deno_timeout_ms: 1000,
        }
    }

    #[test]
    fn filesystem_tool_works_without_host_routing_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello").unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config();

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
        let tools = agent_tool_definitions(false, false);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash"));
        assert!(!names
            .iter()
            .any(|name| name.contains("pedelec_cli.tool_spec")));
        assert!(!names
            .iter()
            .any(|name| name.contains("pedelec_cli.tool_call")));
        let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
        assert!(bash
            .description
            .contains("restricted Pedelec helper command"));
        assert!(bash.description.contains("pedelec-cli"));
        assert!(bash.description.contains("pedelec-deno"));
        assert!(bash.description.contains("not a full shell"));
        assert!(tools.iter().all(|tool| tool.input_schema.is_object()));
    }

    #[test]
    fn web_search_definition_is_conditional_and_only_accepts_query() {
        assert!(!agent_tool_definitions(false, false)
            .iter()
            .any(|tool| tool.name == "web.search"));
        let tools = agent_tool_definitions(false, true);
        let web = tools.iter().find(|tool| tool.name == "web.search").unwrap();
        assert_eq!(web.input_schema["required"], serde_json::json!(["query"]));
        assert!(web.input_schema["properties"].get("max_results").is_none());
    }

    #[test]
    fn bash_tool_does_not_pass_session_id_to_pedelec_cli() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("args.txt");
        let cli = fake_pedelec_cli(temp.path(), &capture);
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let mut cfg = config();
        cfg.pedelec_cli_path = Some(cli);

        let result = execute_tool(
            "bash",
            &serde_json::json!({
                "command": "pedelec-cli --thread-id thread_explicit tool-call get_page '{\"id\":1}'"
            }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap();

        assert_eq!(result.content["ok"], true);
        let args = std::fs::read_to_string(capture).unwrap();
        assert!(args.contains("tool-call"));
        assert!(args.contains("--thread-id"));
        assert!(args.contains("thread_explicit"));
        assert!(args.contains("get_page"));
        assert!(args.contains("id"));
        assert!(args.contains("1"));
        assert!(!args.contains("session_inner"));
    }

    #[test]
    fn bash_tool_executes_pedelec_deno_and_preserves_structured_output() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("args.txt");
        let deno = fake_pedelec_deno(temp.path(), &capture);
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let mut cfg = config();
        cfg.pedelec_deno_path = Some(deno);

        let result = execute_tool(
            "bash",
            &serde_json::json!({
                "command": "pedelec-deno --thread-id thread_explicit run scripts/test.ts -- foo bar"
            }),
            "session_inner",
            &sandbox,
            &cfg,
        )
        .unwrap();

        assert_eq!(result.content["ok"], true);
        assert_eq!(result.content["result"]["exitCode"], 0);
        assert_eq!(result.content["result"]["stdout"], "script output");
        let args = std::fs::read_to_string(capture).unwrap();
        assert!(args.contains("--thread-id"));
        assert!(args.contains("thread_explicit"));
        assert!(args.contains("run"));
        assert!(args.contains("scripts/test.ts"));
        assert!(args.contains("foo"));
        assert!(args.contains("bar"));
        assert!(!args.contains("session_inner"));
    }

    #[test]
    fn bash_tool_rejects_non_pedelec_helper_commands() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config();

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
    fn parser_accepts_the_pedelec_deno_v1_command_forms() {
        assert_eq!(
            parse_restricted_bash_command("pedelec-deno --thread-id thread_1 run scripts/test.ts")
                .and_then(|argv| {
                    validate_restricted_command(&argv)?;
                    Ok(argv)
                })
                .unwrap(),
            vec![
                "pedelec-deno",
                "--thread-id",
                "thread_1",
                "run",
                "scripts/test.ts"
            ]
        );
        assert_eq!(
            parse_restricted_bash_command(
                "pedelec-deno --thread-id thread_1 run scripts/test.ts -- foo bar"
            )
            .and_then(|argv| {
                validate_restricted_command(&argv)?;
                Ok(argv)
            })
            .unwrap(),
            vec![
                "pedelec-deno",
                "--thread-id",
                "thread_1",
                "run",
                "scripts/test.ts",
                "--",
                "foo",
                "bar"
            ]
        );
    }

    #[test]
    fn parser_rejects_other_runtimes_and_pedelec_deno_extensions() {
        for command in [
            "deno run scripts/test.ts",
            "node scripts/test.js",
            "bun scripts/test.ts",
            "npx something",
            "pedelec-deno run scripts/test.ts",
            "pedelec-deno --thread-id thread_1 eval code",
            "pedelec-deno --thread-id thread_1 --allow-all run scripts/test.ts",
            "pedelec-deno --thread-id thread_1 run scripts/test.ts && echo nope",
            "pedelec-deno --thread-id thread_1 run $(cat secret)",
        ] {
            let error = parse_restricted_bash_command(command)
                .and_then(|argv| validate_restricted_command(&argv))
                .unwrap_err();
            assert!(
                matches!(
                    error.code.as_str(),
                    "COMMAND_NOT_ALLOWED" | "INVALID_ARGUMENT" | "UNSUPPORTED_SHELL_SYNTAX"
                ),
                "unexpected error for {command}: {error:?}"
            );
        }
    }

    #[test]
    fn parser_rejects_pedelec_deno_entrypoint_escape_and_unseparated_flags() {
        for command in [
            "pedelec-deno --thread-id thread_1 run ../outside.ts",
            "pedelec-deno --thread-id thread_1 run scripts/../../outside.ts",
        ] {
            let error = parse_restricted_bash_command(command)
                .and_then(|argv| validate_restricted_command(&argv))
                .unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
        }

        let error = parse_restricted_bash_command(
            "pedelec-deno --thread-id thread_1 run scripts/test.ts --allow-net",
        )
        .and_then(|argv| validate_restricted_command(&argv))
        .unwrap_err();
        assert_eq!(error.code, "COMMAND_NOT_ALLOWED");
    }

    #[test]
    fn bash_tool_rejects_unsupported_shell_syntax() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config();

        let err = execute_tool(
            "bash",
            &serde_json::json!({ "command": "pedelec-cli --thread-id thread_inner tool-spec foo && rm -rf /" }),
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
            "pedelec-cli --thread-id thread_1 tool-call ask_user '{\"question\":\"要繼續嗎？\"}'",
        )
        .unwrap();
        assert_eq!(
            single,
            vec![
                "pedelec-cli",
                "--thread-id",
                "thread_1",
                "tool-call",
                "ask_user",
                "{\"question\":\"要繼續嗎？\"}"
            ]
        );

        let double = parse_restricted_bash_command(
            "pedelec-cli --thread-id thread_1 tool-spec \"get current page\"",
        )
        .unwrap();
        assert_eq!(
            double,
            vec![
                "pedelec-cli",
                "--thread-id",
                "thread_1",
                "tool-spec",
                "get current page"
            ]
        );
    }

    #[test]
    fn parser_rejects_command_substitution() {
        let err = parse_restricted_bash_command(
            "pedelec-cli --thread-id thread_1 tool-spec $(cat secret)",
        )
        .unwrap_err();

        assert_eq!(err.code, "UNSUPPORTED_SHELL_SYNTAX");
    }

    #[test]
    fn restricted_command_requires_explicit_thread_id() {
        let err = validate_pedelec_cli_command(&[
            "pedelec-cli".into(),
            "tool-spec".into(),
            "get_page".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(err.message.contains("--thread-id"));

        let err = validate_pedelec_cli_command(&[
            "pedelec-cli".into(),
            "--thread-id".into(),
            "   ".into(),
            "tool-spec".into(),
            "get_page".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(err.message.contains("non-empty"));
    }

    #[test]
    fn old_native_host_tool_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 200).unwrap();
        let cfg = config();

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

    #[cfg(windows)]
    fn fake_pedelec_deno(dir: &Path, capture: &Path) -> PathBuf {
        let path = dir.join("pedelec-deno.cmd");
        std::fs::write(
            &path,
            format!(
                "@echo off\r\necho %* > \"{}\"\r\necho {{\"ok\":true,\"result\":{{\"exitCode\":0,\"stdout\":\"script output\",\"stderr\":\"\",\"stdoutTruncated\":false,\"stderrTruncated\":false}}}}\r\n",
                capture.to_string_lossy()
            ),
        )
        .unwrap();
        path
    }

    #[cfg(not(windows))]
    fn fake_pedelec_deno(dir: &Path, capture: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("pedelec-deno");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"ok\":true,\"result\":{{\"exitCode\":0,\"stdout\":\"script output\",\"stderr\":\"\",\"stdoutTruncated\":false,\"stderrTruncated\":false}}}}'\n",
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
