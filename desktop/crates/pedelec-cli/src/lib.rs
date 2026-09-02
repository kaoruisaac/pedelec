use pedelec_core::{error_codes, PedelecError, ToolCallInput, ToolSpecInput};
use pedelec_ipc::{send_core_ipc_request, send_core_ipc_request_with_runtime_path, CoreIpcRequest};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct ToolCliResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
}

pub fn run() {
    let response = run_tool_cli(std::env::args().collect());
    match serde_json::to_string(&response) {
        Ok(payload) => println!("{payload}"),
        Err(err) => eprintln!("cannot serialize pedelec-cli response: {err}"),
    }
}

fn run_tool_cli(args: Vec<String>) -> ToolCliResponse {
    run_tool_cli_with_runtime_file_path(args, runtime_file_path_from_env().as_deref())
}

pub fn run_tool_cli_with_runtime_file_path(
    args: Vec<String>,
    runtime_file_path: Option<&Path>,
) -> ToolCliResponse {
    match parse_tool_cli_args(&args) {
        Ok(ToolCliCommand::Call(input)) => {
            let request = CoreIpcRequest {
                request_id: next_cli_request_id(),
                r#type: "tool_call".to_string(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(serde_json::json!(input)),
            };
            send_cli_request(request, runtime_file_path)
        }
        Ok(ToolCliCommand::Spec(input)) => {
            let request = CoreIpcRequest {
                request_id: next_cli_request_id(),
                r#type: "tool_spec".to_string(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(serde_json::json!(input)),
            };
            send_cli_request(request, runtime_file_path)
        }
        Err(err) => ToolCliResponse {
            ok: false,
            result: None,
            error: Some(err),
        },
    }
}

fn send_cli_request(request: CoreIpcRequest, runtime_file_path: Option<&Path>) -> ToolCliResponse {
    let response = match runtime_file_path {
        Some(path) => send_core_ipc_request_with_runtime_path(&request, path),
        None => send_core_ipc_request(&request),
    };
    match response {
        Ok(response) if response.ok => ToolCliResponse {
            ok: true,
            result: response.result,
            error: None,
        },
        Ok(response) => ToolCliResponse {
            ok: false,
            result: None,
            error: response.error.or_else(|| {
                Some(PedelecError::new(
                    error_codes::IPC_UNAVAILABLE,
                    "Core IPC request failed",
                ))
            }),
        },
        Err(err) => ToolCliResponse {
            ok: false,
            result: None,
            error: Some(err),
        },
    }
}

fn runtime_file_path_from_env() -> Option<PathBuf> {
    std::env::var_os("PEDELEC_CORE_IPC_RUNTIME_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug)]
enum ToolCliCommand {
    Call(ToolCallInput),
    Spec(ToolSpecInput),
}

const TOOL_CLI_USAGE: &str = "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name> OR pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> '<json_args>'";

fn parse_tool_cli_args(args: &[String]) -> Result<ToolCliCommand, PedelecError> {
    let thread_id = parse_thread_id_arg(args)?;
    match args.get(3).map(String::as_str) {
        Some("tool-call") => parse_tool_call_args(args, &thread_id).map(ToolCliCommand::Call),
        Some("tool-spec") => parse_tool_spec_args(args, &thread_id).map(ToolCliCommand::Spec),
        _ => Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            TOOL_CLI_USAGE,
        )),
    }
}

fn parse_thread_id_arg(args: &[String]) -> Result<String, PedelecError> {
    if args.get(1).map(String::as_str) != Some("--thread-id") {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            TOOL_CLI_USAGE,
        ));
    }

    args.get(2)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::TOOL_ARGS_INVALID,
                "pedelec-cli requires a non-empty --thread-id value.",
            )
        })
}

fn parse_tool_call_args(args: &[String], thread_id: &str) -> Result<ToolCallInput, PedelecError> {
    if args.len() != 6 {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool_name> '<json_args>'",
        ));
    }

    let json_args = serde_json::from_str::<Value>(&args[5]).map_err(|err| {
        PedelecError::with_details(
            error_codes::TOOL_ARGS_INVALID,
            "tool args must be valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    Ok(ToolCallInput {
        thread_id: thread_id.to_string(),
        tool_name: args[4].clone(),
        args: json_args,
    })
}

fn parse_tool_spec_args(args: &[String], thread_id: &str) -> Result<ToolSpecInput, PedelecError> {
    if args.len() != 5 {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool_name>",
        ));
    }

    Ok(ToolSpecInput {
        thread_id: thread_id.to_string(),
        tool_name: args[4].clone(),
    })
}

fn next_cli_request_id() -> String {
    format!(
        "cli_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_cli_args_return_json_error_shape() {
        let response = run_tool_cli(vec!["pedelec-cli".into()]);

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    #[test]
    fn invalid_json_args_return_tool_args_invalid() {
        let response = run_tool_cli(vec![
            "pedelec-cli".into(),
            "--thread-id".into(),
            "thread_1".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{".into(),
        ]);

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    #[test]
    fn tool_call_format_reads_explicit_thread_id() {
        let input = parse_tool_cli_args(&[
            "pedelec-cli".into(),
            "--thread-id".into(),
            "thread_1".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap();

        let ToolCliCommand::Call(input) = input else {
            panic!("expected tool call command");
        };
        assert_eq!(input.thread_id, "thread_1");
        assert_eq!(input.tool_name, "get_app_state");
        assert_eq!(input.args, serde_json::json!({}));
    }

    #[test]
    fn tool_spec_format_reads_explicit_thread_id() {
        let command = parse_tool_cli_args(&[
            "pedelec-cli".into(),
            "--thread-id".into(),
            "thread_1".into(),
            "tool-spec".into(),
            "get_app_state".into(),
        ])
        .unwrap();

        let ToolCliCommand::Spec(input) = command else {
            panic!("expected tool spec command");
        };
        assert_eq!(input.thread_id, "thread_1");
        assert_eq!(input.tool_name, "get_app_state");
    }

    #[test]
    fn missing_thread_id_fails_explicitly() {
        let err = parse_tool_cli_args(&[
            "pedelec-cli".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, error_codes::TOOL_ARGS_INVALID);
        assert!(err.message.contains("--thread-id"));
    }

    #[test]
    fn blank_thread_id_fails_explicitly() {
        let err = parse_tool_cli_args(&[
            "pedelec-cli".into(),
            "--thread-id".into(),
            "   ".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, error_codes::TOOL_ARGS_INVALID);
        assert!(err.message.contains("non-empty"));
    }
}
