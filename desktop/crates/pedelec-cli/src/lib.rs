use pedelec_core::{error_codes, PedelecError, ToolCallInput, ToolSpecInput};
use pedelec_ipc::{
    send_core_ipc_request, send_core_ipc_request_with_runtime_path, CoreIpcRequest, CoreIpcResponse,
};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAX_PROVIDER_ARTIFACT_HOOK_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct ToolCliResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
}

pub fn run() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("provider-artifact-hook") {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let result = execute_provider_artifact_hook_cli(
            &args,
            std::env::var("PEDELEC_PROVIDER").ok().as_deref(),
            runtime_file_path_from_env().as_deref(),
            stdin.lock(),
            stdout.lock(),
            |request, runtime_file_path| match runtime_file_path {
                Some(path) => send_core_ipc_request_with_runtime_path(request, path),
                None => Err(PedelecError::new(
                    error_codes::CORE_RUNTIME_UNAVAILABLE,
                    "provider artifact hook is missing PEDELEC_CORE_IPC_RUNTIME_FILE",
                )),
            },
        );
        if let Err(error) = result {
            eprintln!(
                "pedelec-cli provider artifact hook failed: {}",
                error.message
            );
            std::process::exit(1);
        }
        return;
    }

    let response = run_tool_cli(args);
    match serde_json::to_string(&response) {
        Ok(payload) => println!("{payload}"),
        Err(err) => eprintln!("cannot serialize pedelec-cli response: {err}"),
    }
}

fn execute_provider_artifact_hook_cli<R, W, F>(
    args: &[String],
    provider_env: Option<&str>,
    runtime_file_path: Option<&Path>,
    mut stdin: R,
    mut stdout: W,
    mut send_request: F,
) -> Result<(), PedelecError>
where
    R: Read,
    W: Write,
    F: FnMut(&CoreIpcRequest, Option<&Path>) -> Result<CoreIpcResponse, PedelecError>,
{
    let (provider, stage) = parse_provider_artifact_hook_args(args)?;
    if provider != "antigravity" || provider_env != Some("antigravity") {
        return Err(PedelecError::new(
            error_codes::IPC_UNAUTHORIZED,
            "provider artifact hook requires the Antigravity runtime environment",
        ));
    }
    let runtime_file_path = runtime_file_path.ok_or_else(|| {
        PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "provider artifact hook is missing PEDELEC_CORE_IPC_RUNTIME_FILE",
        )
    })?;
    let mut bytes = Vec::new();
    stdin
        .by_ref()
        .take((MAX_PROVIDER_ARTIFACT_HOOK_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            PedelecError::with_details(
                error_codes::INVALID_INPUT,
                "provider artifact hook could not read stdin",
                serde_json::json!({"reason":error.to_string()}),
            )
        })?;
    if bytes.len() > MAX_PROVIDER_ARTIFACT_HOOK_PAYLOAD_BYTES {
        return Err(PedelecError::new(
            error_codes::MESSAGE_TOO_LARGE,
            "provider artifact hook input exceeds the 1 MiB limit",
        ));
    }
    let hook_payload = serde_json::from_slice::<Value>(&bytes).map_err(|error| {
        PedelecError::with_details(
            error_codes::INVALID_INPUT,
            "provider artifact hook stdin is not valid JSON",
            serde_json::json!({"reason":error.to_string()}),
        )
    })?;
    if !hook_payload.is_object() {
        return Err(PedelecError::new(
            error_codes::INVALID_INPUT,
            "provider artifact hook stdin must contain a JSON object",
        ));
    }
    let request = CoreIpcRequest {
        request_id: next_cli_request_id(),
        r#type: "provider_artifact_hook".to_string(),
        caller_origin: None,
        caller_sdk_version: None,
        payload: Some(serde_json::json!({
            "provider":provider,
            "stage":stage,
            "hookPayload":hook_payload,
        })),
    };
    let response = send_request(&request, Some(runtime_file_path))?;
    if !response.ok {
        return Err(response.error.unwrap_or_else(|| {
            PedelecError::new(error_codes::IPC_UNAVAILABLE, "Core IPC hook request failed")
        }));
    }
    // PreToolUse must allow the provider tool only after the Core snapshot
    // succeeds. PostToolUse has no decision field.
    let response_body = if stage == "before" {
        r#"{"decision":"allow"}"#
    } else {
        "{}"
    };
    writeln!(stdout, "{response_body}").map_err(|error| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "provider artifact hook could not write its response",
            serde_json::json!({"reason":error.to_string()}),
        )
    })
}

fn parse_provider_artifact_hook_args(
    args: &[String],
) -> Result<(&'static str, &'static str), PedelecError> {
    if args.len() != 4
        || args.get(1).map(String::as_str) != Some("provider-artifact-hook")
        || args.get(2).map(String::as_str) != Some("antigravity")
        || !matches!(args.get(3).map(String::as_str), Some("before" | "after"))
    {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli provider-artifact-hook antigravity before|after",
        ));
    }
    Ok((
        "antigravity",
        if args[3] == "before" {
            "before"
        } else {
            "after"
        },
    ))
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
    send_cli_request_with(request, runtime_file_path, |request, runtime_file_path| {
        match runtime_file_path {
            Some(path) => send_core_ipc_request_with_runtime_path(request, path),
            None => send_core_ipc_request(request),
        }
    })
}

fn send_cli_request_with<F>(
    request: CoreIpcRequest,
    runtime_file_path: Option<&Path>,
    mut send_request: F,
) -> ToolCliResponse
where
    F: FnMut(&CoreIpcRequest, Option<&Path>) -> Result<CoreIpcResponse, PedelecError>,
{
    let response = send_request(&request, runtime_file_path);
    let response = if request.r#type == "tool_call" && response.is_err() {
        send_request(&request, runtime_file_path)
    } else {
        response
    };
    match response {
        Ok(response) if response.ok => ToolCliResponse {
            ok: true,
            result: response.result,
            error: None,
        },
        Ok(response) => {
            let error = match response.error {
                Some(error) => error,
                None => PedelecError::new(error_codes::IPC_UNAVAILABLE, "Core IPC request failed"),
            };
            ToolCliResponse {
                ok: false,
                result: None,
                error: Some(error),
            }
        }
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
    use std::io::Cursor;

    fn tool_call_request() -> CoreIpcRequest {
        CoreIpcRequest {
            request_id: "cli-test".into(),
            r#type: "tool_call".into(),
            caller_origin: None,
            caller_sdk_version: None,
            payload: Some(serde_json::json!({
                "threadId": "thread-cli-test",
                "toolName": "generate_image",
                "args": { "prompt": "a bicycle" }
            })),
        }
    }

    fn structured_error_response(error: PedelecError) -> CoreIpcResponse {
        CoreIpcResponse {
            request_id: "cli-test".into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    #[test]
    fn invalid_cli_args_return_json_error_shape() {
        let response = run_tool_cli(vec!["pedelec-cli".into()]);

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    fn provider_artifact_hook_args(stage: &str) -> Vec<String> {
        vec![
            "pedelec-cli".into(),
            "provider-artifact-hook".into(),
            "antigravity".into(),
            stage.into(),
        ]
    }

    fn successful_hook_response(request: &CoreIpcRequest) -> CoreIpcResponse {
        CoreIpcResponse {
            request_id: request.request_id.clone(),
            ok: true,
            result: Some(serde_json::json!({})),
            error: None,
        }
    }

    #[test]
    fn provider_artifact_hook_before_returns_allow_after_core_success() {
        let args = provider_artifact_hook_args("before");
        let runtime_path = PathBuf::from("runtime.json");
        let mut output = Vec::new();
        let mut sent = None;
        execute_provider_artifact_hook_cli(
            &args,
            Some("antigravity"),
            Some(&runtime_path),
            Cursor::new(br#"{"conversationId":"conv-1","artifactDirectoryPath":"C:/artifacts","toolCall":{"name":"generate_image","args":{}},"stepIdx":4}"#),
            &mut output,
            |request, path| {
                assert_eq!(path, Some(runtime_path.as_path()));
                sent = Some(request.clone());
                Ok(successful_hook_response(request))
            },
        )
        .unwrap();

        assert_eq!(output, b"{\"decision\":\"allow\"}\n");
        let request = sent.unwrap();
        assert_eq!(request.r#type, "provider_artifact_hook");
        assert_eq!(request.caller_origin, None);
        assert_eq!(request.payload.as_ref().unwrap()["provider"], "antigravity");
        assert_eq!(request.payload.as_ref().unwrap()["stage"], "before");
        assert_eq!(
            request.payload.as_ref().unwrap()["hookPayload"]["conversationId"],
            "conv-1"
        );
    }

    #[test]
    fn provider_artifact_hook_after_returns_empty_object() {
        let args = provider_artifact_hook_args("after");
        let runtime_path = PathBuf::from("runtime.json");
        let mut output = Vec::new();
        execute_provider_artifact_hook_cli(
            &args,
            Some("antigravity"),
            Some(&runtime_path),
            Cursor::new(br#"{}"#),
            &mut output,
            |request, _| Ok(successful_hook_response(request)),
        )
        .unwrap();

        assert_eq!(output, b"{}\n");
    }

    #[test]
    fn provider_artifact_hook_before_failure_does_not_emit_allow() {
        let args = provider_artifact_hook_args("before");
        let runtime_path = PathBuf::from("runtime.json");
        let mut output = Vec::new();
        let error = execute_provider_artifact_hook_cli(
            &args,
            Some("antigravity"),
            Some(&runtime_path),
            Cursor::new(br#"{}"#),
            &mut output,
            |_, _| {
                Err(PedelecError::new(
                    error_codes::IPC_UNAVAILABLE,
                    "Core IPC connection closed",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.code, error_codes::IPC_UNAVAILABLE);
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("\"decision\":\"allow\""));

        let mut output = Vec::new();
        let error = execute_provider_artifact_hook_cli(
            &args,
            Some("antigravity"),
            Some(&runtime_path),
            Cursor::new(br#"{}"#),
            &mut output,
            |request, _| {
                Ok(CoreIpcResponse {
                    request_id: request.request_id.clone(),
                    ok: false,
                    result: None,
                    error: Some(PedelecError::new(
                        error_codes::PROVIDER_ARTIFACT_INVALID,
                        "before snapshot failed",
                    )),
                })
            },
        )
        .unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_ARTIFACT_INVALID);
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("\"decision\":\"allow\""));
    }

    #[test]
    fn provider_artifact_hook_rejects_invalid_environment_and_json() {
        let args = provider_artifact_hook_args("after");
        let runtime_path = PathBuf::from("runtime.json");
        let mut output = Vec::new();
        let error = execute_provider_artifact_hook_cli(
            &args,
            Some("claude"),
            Some(&runtime_path),
            Cursor::new(br#"{}"#),
            &mut output,
            |_, _| panic!("invalid provider must not reach Core IPC"),
        )
        .unwrap_err();
        assert_eq!(error.code, error_codes::IPC_UNAUTHORIZED);
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("\"decision\":\"allow\""));

        let mut output = Vec::new();
        let error = execute_provider_artifact_hook_cli(
            &args,
            Some("antigravity"),
            Some(&runtime_path),
            Cursor::new(b"not-json"),
            &mut output,
            |_, _| panic!("invalid JSON must not reach Core IPC"),
        )
        .unwrap_err();
        assert_eq!(error.code, error_codes::INVALID_INPUT);
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("\"decision\":\"allow\""));
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

    #[test]
    fn tool_call_retries_transport_failure_once_with_the_same_request() {
        let request = tool_call_request();
        let mut calls = 0;
        let mut attempted_requests = Vec::new();
        let response = send_cli_request_with(request.clone(), None, |attempt, _runtime_path| {
            calls += 1;
            attempted_requests.push(attempt.clone());
            if calls == 1 {
                Err(PedelecError::new(
                    error_codes::IPC_UNAVAILABLE,
                    "Core IPC connection closed",
                ))
            } else {
                Ok(CoreIpcResponse {
                    request_id: attempt.request_id.clone(),
                    ok: true,
                    result: Some(serde_json::json!({ "id": "image-1" })),
                    error: None,
                })
            }
        });

        assert_eq!(calls, 2);
        assert_eq!(attempted_requests, vec![request.clone(), request]);
        assert!(response.ok);
        assert_eq!(
            response.result,
            Some(serde_json::json!({ "id": "image-1" }))
        );
    }

    #[test]
    fn second_tool_call_transport_failure_returns_plain_structured_error() {
        let request = tool_call_request();
        let mut calls = 0;
        let response = send_cli_request_with(request, None, |_attempt, _runtime_path| {
            calls += 1;
            Err(PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                format!("Core IPC failure {calls}"),
                serde_json::json!({ "attempt": calls }),
            ))
        });

        assert_eq!(calls, 2);
        let error = response.error.unwrap();
        assert_eq!(error.code, error_codes::IPC_UNAVAILABLE);
        assert_eq!(error.message, "Core IPC failure 2");
        let details = error.details.unwrap();
        assert_eq!(details["attempt"], 2);
        assert!(details.get("retry").is_none());
        assert!(!error.message.contains("Exact-retry"));
        assert!(!error.message.contains("agent"));
    }

    #[test]
    fn successful_tool_call_does_not_emit_retry_guidance() {
        let mut calls = 0;
        let response =
            send_cli_request_with(tool_call_request(), None, |request, _runtime_path| {
                calls += 1;
                Ok(CoreIpcResponse {
                    request_id: request.request_id.clone(),
                    ok: true,
                    result: Some(serde_json::json!({ "id": "image-1" })),
                    error: None,
                })
            });

        assert_eq!(calls, 1);
        assert!(response.ok);
        assert!(response.error.is_none());
        assert_eq!(
            response.result,
            Some(serde_json::json!({ "id": "image-1" }))
        );
    }

    #[test]
    fn structured_tool_timeout_is_final_and_not_retry_guidance() {
        let mut calls = 0;
        let response =
            send_cli_request_with(tool_call_request(), None, |_request, _runtime_path| {
                calls += 1;
                Ok(structured_error_response(PedelecError::new(
                    error_codes::TOOL_TIMEOUT,
                    "tool timeout",
                )))
            });

        assert_eq!(calls, 1);
        let error = response.error.unwrap();
        assert_eq!(error.code, error_codes::TOOL_TIMEOUT);
        assert_eq!(error.message, "tool timeout");
        assert!(error.details.is_none());
    }

    #[test]
    fn structured_app_tool_error_keeps_normal_error_semantics() {
        let mut calls = 0;
        let response =
            send_cli_request_with(tool_call_request(), None, |_request, _runtime_path| {
                calls += 1;
                Ok(structured_error_response(PedelecError::new(
                    error_codes::TOOL_NOT_FOUND,
                    "tool was not found in registry",
                )))
            });

        assert_eq!(calls, 1);
        let error = response.error.unwrap();
        assert_eq!(error.code, error_codes::TOOL_NOT_FOUND);
        assert_eq!(error.message, "tool was not found in registry");
        assert!(error.details.is_none());
    }

    #[test]
    fn tool_spec_does_not_retry_transport_failures() {
        let request = CoreIpcRequest {
            request_id: "cli-spec-test".into(),
            r#type: "tool_spec".into(),
            caller_origin: None,
            caller_sdk_version: None,
            payload: Some(serde_json::json!({
                "threadId": "thread-cli-test",
                "toolName": "generate_image"
            })),
        };
        let mut calls = 0;
        let response = send_cli_request_with(request, None, |_request, _runtime_path| {
            calls += 1;
            Err(PedelecError::new(
                error_codes::IPC_UNAVAILABLE,
                "Core IPC connection closed",
            ))
        });

        assert_eq!(calls, 1);
        assert_eq!(response.error.unwrap().code, error_codes::IPC_UNAVAILABLE);
    }
}
