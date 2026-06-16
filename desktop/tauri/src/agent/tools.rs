use super::config::AgentConfig;
use super::error::AgentError;
use super::sandbox::Sandbox;
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn tool_definitions() -> Value {
    serde_json::json!([
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
        {
            "type": "function",
            "function": {
                "name": "web.search",
                "description": "Search the web for current information.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "maxResults": { "type": "integer" }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "pedelec_cli.tool_call",
                "description": "Call a Pedelec host app tool through pedelec-cli.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "toolName": { "type": "string" },
                        "args": { "type": "object" }
                    },
                    "required": ["toolName", "args"],
                    "additionalProperties": false
                }
            }
        }
    ])
}

pub fn execute_tool(
    tool: &str,
    args: &Value,
    session_id: &str,
    sandbox: &Sandbox,
    config: &AgentConfig,
) -> Result<Value, AgentError> {
    match tool {
        "fs.list_text_files" => {
            let dir = args.get("dir").and_then(Value::as_str).unwrap_or(".");
            let max_depth = args
                .get("maxDepth")
                .and_then(Value::as_u64)
                .unwrap_or(3)
                .min(16) as usize;
            let files = sandbox.list_text_files(dir, max_depth)?;
            Ok(serde_json::json!({ "files": files }))
        }
        "fs.read_text_file" => {
            let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
                AgentError::new("INVALID_ARGUMENT", "fs.read_text_file requires path")
            })?;
            let (text, truncated) = sandbox.read_text_file(path)?;
            Ok(serde_json::json!({ "path": path, "text": text, "truncated": truncated }))
        }
        "web.search" => web_search(args, config),
        "pedelec_cli.tool_call" => pedelec_cli_tool_call(args, session_id, config),
        _ => Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Unknown tool",
            serde_json::json!({ "tool": tool }),
        )),
    }
}

fn web_search(args: &Value, config: &AgentConfig) -> Result<Value, AgentError> {
    let provider = config
        .web_search_provider
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let api_key = config.brave_search_api_key.as_deref().unwrap_or("");
    if provider != "brave" || api_key.trim().is_empty() {
        return Err(AgentError::new(
            "WEB_SEARCH_UNCONFIGURED",
            "Web search provider or API key is not configured",
        ));
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AgentError::new("INVALID_ARGUMENT", "web.search requires query"))?;
    let max_results = args
        .get("maxResults")
        .and_then(Value::as_u64)
        .unwrap_or(config.web_search_max_results as u64)
        .min(config.web_search_max_results as u64) as usize;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(config.web_search_timeout_ms))
        .build()
        .map_err(|err| {
            AgentError::with_details(
                "WEB_SEARCH_FAILED",
                "Failed to create web search client",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
    let url: String = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        percent_encode_query(query),
        max_results
    );
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .map_err(|err| {
            AgentError::with_details(
                "WEB_SEARCH_FAILED",
                "Web search request failed",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
    let status = response.status();
    let text = response.text().map_err(|err| {
        AgentError::with_details(
            "WEB_SEARCH_FAILED",
            "Failed to read web search response",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    if !status.is_success() {
        return Err(AgentError::with_details(
            "WEB_SEARCH_FAILED",
            "Web search request returned an error",
            serde_json::json!({ "status": status.as_u16(), "body": text }),
        ));
    }
    let value = serde_json::from_str::<Value>(&text).map_err(|err| {
        AgentError::with_details(
            "WEB_SEARCH_FAILED",
            "Web search response was invalid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let results = value
        .pointer("/web/results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(max_results)
                .map(|item| {
                    serde_json::json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                        "snippet": item.get("description").and_then(Value::as_str).unwrap_or("")
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({ "query": query, "results": results }))
}

fn pedelec_cli_tool_call(
    args: &Value,
    session_id: &str,
    config: &AgentConfig,
) -> Result<Value, AgentError> {
    let tool_name = args
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AgentError::new("INVALID_ARGUMENT", "toolName must be a non-empty string")
        })?;
    let tool_args = args
        .get("args")
        .filter(|value| value.is_object())
        .ok_or_else(|| AgentError::new("INVALID_ARGUMENT", "args must be a JSON object"))?;
    let cli_path = resolve_pedelec_cli(config)?;
    let json_args = serde_json::to_string(tool_args).map_err(|err| {
        AgentError::with_details(
            "PEDELEC_CLI_FAILED",
            "Failed to serialize tool args",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let mut command = Command::new(cli_path);
    command
        .arg("tool-call")
        .arg(session_id)
        .arg(tool_name)
        .arg(json_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(runtime_file) = &config.core_runtime_file {
        command.env("PEDELEC_CORE_IPC_RUNTIME_FILE", runtime_file);
    }
    let output =
        run_command_with_timeout(command, config.pedelec_cli_timeout_ms).map_err(|err| {
            AgentError::with_details(
                "PEDELEC_CLI_FAILED",
                "Failed to execute pedelec-cli",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        return Err(AgentError::with_details(
            "PEDELEC_CLI_FAILED",
            "pedelec-cli exited with an error",
            serde_json::json!({
                "status": output.status.code(),
                "stdout": stdout,
                "stderr": String::from_utf8_lossy(&output.stderr)
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

fn run_command_with_timeout(
    mut command: Command,
    timeout_ms: u64,
) -> Result<Output, std::io::Error> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child.wait_with_output();
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

fn percent_encode_query(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push_str("%20"),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{AgentConfig, ModelProvider};

    fn config(temp: &Path) -> AgentConfig {
        AgentConfig {
            provider: ModelProvider::Ollama,
            provider_name: "ollama".into(),
            model: "fake".into(),
            ollama_base_url: "http://127.0.0.1:1".into(),
            ollama_timeout_ms: 1000,
            home: temp.join("home"),
            sandbox: temp.to_path_buf(),
            web_search_provider: None,
            web_search_timeout_ms: 1000,
            web_search_max_results: 5,
            brave_search_api_key: None,
            pedelec_cli_path: None,
            core_runtime_file: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    #[test]
    fn web_search_without_key_is_unconfigured() {
        let temp = tempfile::tempdir().unwrap();
        let err =
            web_search(&serde_json::json!({ "query": "x" }), &config(temp.path())).unwrap_err();

        assert_eq!(err.code, "WEB_SEARCH_UNCONFIGURED");
    }
}
