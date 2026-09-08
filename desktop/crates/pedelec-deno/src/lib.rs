use pedelec_core::{error_codes, DenoRunInput, DenoRunOutput, PedelecError};
use pedelec_ipc::{send_core_ipc_request, send_core_ipc_request_with_runtime_path, CoreIpcRequest};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DenoCliResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<DenoRunOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PedelecError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenoCliCommand {
    Run {
        thread_id: String,
        entrypoint: String,
        args: Vec<String>,
    },
}

pub fn run() {
    let response = run_deno_cli(std::env::args().collect());
    match serde_json::to_string(&response) {
        Ok(payload) => println!("{payload}"),
        Err(err) => eprintln!("cannot serialize pedelec-deno response: {err}"),
    }
}

fn run_deno_cli(args: Vec<String>) -> DenoCliResponse {
    run_deno_cli_with_runtime_file_path(args, runtime_file_path_from_env().as_deref())
}

pub fn run_deno_cli_with_runtime_file_path(
    args: Vec<String>,
    runtime_file_path: Option<&Path>,
) -> DenoCliResponse {
    match parse_deno_cli_args(&args) {
        Ok(DenoCliCommand::Run {
            thread_id,
            entrypoint,
            args,
        }) => {
            let request = CoreIpcRequest {
                request_id: next_deno_request_id(),
                r#type: "deno_run".to_string(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(serde_json::json!(DenoRunInput {
                    thread_id,
                    entrypoint,
                    args,
                })),
            };
            send_deno_request(request, runtime_file_path)
        }
        Err(err) => DenoCliResponse {
            ok: false,
            result: None,
            error: Some(err),
        },
    }
}

fn send_deno_request(request: CoreIpcRequest, runtime_file_path: Option<&Path>) -> DenoCliResponse {
    let response = match runtime_file_path {
        Some(path) => send_core_ipc_request_with_runtime_path(&request, path),
        None => send_core_ipc_request(&request),
    };

    match response {
        Ok(response) if response.ok => {
            let result = response
                .result
                .ok_or_else(|| {
                    PedelecError::new(
                        error_codes::IPC_UNAVAILABLE,
                        "Core IPC returned a successful Deno response without a result",
                    )
                })
                .and_then(|value| {
                    serde_json::from_value(value).map_err(|err| {
                        PedelecError::with_details(
                            error_codes::IPC_UNAVAILABLE,
                            "Core IPC returned an invalid Deno result",
                            serde_json::json!({ "error": err.to_string() }),
                        )
                    })
                });
            match result {
                Ok(result) => DenoCliResponse {
                    ok: true,
                    result: Some(result),
                    error: None,
                },
                Err(error) => DenoCliResponse {
                    ok: false,
                    result: None,
                    error: Some(error),
                },
            }
        }
        Ok(response) => DenoCliResponse {
            ok: false,
            result: None,
            error: response.error.or_else(|| {
                Some(PedelecError::new(
                    error_codes::IPC_UNAVAILABLE,
                    "Core IPC Deno request failed",
                ))
            }),
        },
        Err(err) => DenoCliResponse {
            ok: false,
            result: None,
            error: Some(err),
        },
    }
}

const DENO_CLI_USAGE: &str =
    "usage: pedelec-deno --thread-id <pedelec_thread_id> run <workspace-relative-script> [-- <script-args...>]";

pub fn parse_deno_cli_args(args: &[String]) -> Result<DenoCliCommand, PedelecError> {
    let thread_id = parse_thread_id_arg(args)?;
    if args.get(3).map(String::as_str) != Some("run") {
        return Err(deno_args_error(DENO_CLI_USAGE));
    }

    let entrypoint = args.get(4).ok_or_else(|| {
        deno_args_error("pedelec-deno run requires a workspace-relative entrypoint.")
    })?;
    validate_entrypoint(entrypoint)?;

    let script_args = match args.get(5) {
        None => Vec::new(),
        Some(separator) if separator == "--" => args[6..].to_vec(),
        Some(_) => {
            return Err(deno_args_error(
                "raw Deno options are not accepted; put script arguments after `--`.",
            ));
        }
    };

    Ok(DenoCliCommand::Run {
        thread_id,
        entrypoint: entrypoint.clone(),
        args: script_args,
    })
}

fn parse_thread_id_arg(args: &[String]) -> Result<String, PedelecError> {
    if args.get(1).map(String::as_str) != Some("--thread-id") {
        return Err(deno_args_error(DENO_CLI_USAGE));
    }

    args.get(2)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| deno_args_error("pedelec-deno requires a non-empty --thread-id value."))
}

fn validate_entrypoint(entrypoint: &str) -> Result<(), PedelecError> {
    if entrypoint.trim().is_empty()
        || entrypoint.starts_with('-')
        || entrypoint.chars().any(char::is_control)
    {
        return Err(deno_args_error(
            "pedelec-deno requires a non-empty workspace-relative entrypoint.",
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
        return Err(deno_args_error(
            "Deno entrypoint must be a workspace-relative path without traversal.",
        ));
    }
    Ok(())
}

fn deno_args_error(message: impl Into<String>) -> PedelecError {
    PedelecError::new(error_codes::DENO_ARGS_INVALID, message)
}

fn runtime_file_path_from_env() -> Option<PathBuf> {
    std::env::var_os("PEDELEC_CORE_IPC_RUNTIME_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn next_deno_request_id() -> String {
    format!(
        "deno_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_valid_run_without_script_args() {
        let command = parse_deno_cli_args(&argv(&[
            "pedelec-deno",
            "--thread-id",
            "thread_1",
            "run",
            "scripts/analyze.ts",
        ]))
        .unwrap();

        assert_eq!(
            command,
            DenoCliCommand::Run {
                thread_id: "thread_1".into(),
                entrypoint: "scripts/analyze.ts".into(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn missing_thread_id_is_rejected() {
        let error =
            parse_deno_cli_args(&argv(&["pedelec-deno", "run", "scripts/analyze.ts"])).unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
        assert!(error.message.contains("--thread-id"));
    }

    #[test]
    fn blank_thread_id_is_rejected() {
        let error = parse_deno_cli_args(&argv(&[
            "pedelec-deno",
            "--thread-id",
            "   ",
            "run",
            "scripts/analyze.ts",
        ]))
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
        assert!(error.message.contains("non-empty"));
    }

    #[test]
    fn unsupported_commands_are_rejected() {
        let error = parse_deno_cli_args(&argv(&[
            "pedelec-deno",
            "--thread-id",
            "thread_1",
            "task",
            "analyze",
        ]))
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
    }

    #[test]
    fn absolute_and_traversal_entrypoints_are_rejected() {
        for entrypoint in [
            if cfg!(windows) {
                r"C:\outside.ts"
            } else {
                "/outside.ts"
            },
            "../outside.ts",
            "scripts/../../outside.ts",
        ] {
            let error = parse_deno_cli_args(&argv(&[
                "pedelec-deno",
                "--thread-id",
                "thread_1",
                "run",
                entrypoint,
            ]))
            .unwrap_err();
            assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
        }
    }

    #[test]
    fn missing_entrypoint_is_rejected() {
        let error = parse_deno_cli_args(&argv(&["pedelec-deno", "--thread-id", "thread_1", "run"]))
            .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
    }

    #[test]
    fn script_args_after_separator_are_preserved_verbatim() {
        let command = parse_deno_cli_args(&argv(&[
            "pedelec-deno",
            "--thread-id",
            "thread_1",
            "run",
            "scripts/analyze.ts",
            "--",
            "--allow-net",
            "--",
            "value with spaces",
        ]))
        .unwrap();

        assert_eq!(
            command,
            DenoCliCommand::Run {
                thread_id: "thread_1".into(),
                entrypoint: "scripts/analyze.ts".into(),
                args: vec![
                    "--allow-net".into(),
                    "--".into(),
                    "value with spaces".into()
                ],
            }
        );
    }

    #[test]
    fn raw_deno_options_before_script_separator_are_rejected() {
        let error = parse_deno_cli_args(&argv(&[
            "pedelec-deno",
            "--thread-id",
            "thread_1",
            "run",
            "--allow-net",
        ]))
        .unwrap_err();
        assert_eq!(error.code, error_codes::DENO_ARGS_INVALID);
    }

    #[test]
    fn response_serializes_the_stable_json_shape() {
        let response = DenoCliResponse {
            ok: true,
            result: Some(DenoRunOutput {
                exit_code: 0,
                stdout: "out".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            }),
            error: None,
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["result"]["exitCode"], 0);
        assert_eq!(json["result"]["stdoutTruncated"], false);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn successful_core_response_with_invalid_result_is_an_error() {
        let value = serde_json::json!({ "not": "a Deno result" });
        let parsed = serde_json::from_value::<DenoRunOutput>(value);
        assert!(parsed.is_err());
    }
}
