use crate::pedelec_core::{error_codes, PedelecError, ToolCallInput, ToolSpecInput};
use crate::pedelec_ipc::{
    send_core_ipc_request, send_core_ipc_request_with_runtime_path, CoreIpcRequest,
};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

#[cfg(test)]
static PEDELEC_THREAD_ID_ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) struct ThreadIdEnvGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl ThreadIdEnvGuard {
    pub(crate) fn set(value: Option<&str>) -> Self {
        let lock = PEDELEC_THREAD_ID_ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("PEDELEC_THREAD_ID");
        match value {
            Some(value) => std::env::set_var("PEDELEC_THREAD_ID", value),
            None => std::env::remove_var("PEDELEC_THREAD_ID"),
        }
        Self {
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for ThreadIdEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("PEDELEC_THREAD_ID", value),
            None => std::env::remove_var("PEDELEC_THREAD_ID"),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ToolCliResponse {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<PedelecError>,
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

pub(crate) fn run_tool_cli_with_runtime_file_path(
    args: Vec<String>,
    runtime_file_path: Option<&Path>,
) -> ToolCliResponse {
    match parse_tool_cli_args(&args) {
        Ok(ToolCliCommand::Call(input)) => {
            let request = CoreIpcRequest {
                request_id: next_cli_request_id(),
                r#type: "tool_call".to_string(),
                payload: Some(serde_json::json!(input)),
            };
            send_cli_request(request, runtime_file_path)
        }
        Ok(ToolCliCommand::Spec(input)) => {
            let request = CoreIpcRequest {
                request_id: next_cli_request_id(),
                r#type: "tool_spec".to_string(),
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

enum ToolCliCommand {
    Call(ToolCallInput),
    Spec(ToolSpecInput),
}

fn parse_tool_cli_args(args: &[String]) -> Result<ToolCliCommand, PedelecError> {
    match args.get(1).map(String::as_str) {
        Some("tool-call") => parse_tool_call_args(args).map(ToolCliCommand::Call),
        Some("tool-spec") => parse_tool_spec_args(args).map(ToolCliCommand::Spec),
        _ => Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli tool-spec <tool_name> OR pedelec-cli tool-call <tool_name> '<json_args>'",
        )),
    }
}

fn parse_tool_call_args(args: &[String]) -> Result<ToolCallInput, PedelecError> {
    if args.len() != 4 {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli tool-call <tool_name> '<json_args>'",
        ));
    }

    let thread_id = thread_id_from_env()?;
    let json_args = serde_json::from_str::<Value>(&args[3]).map_err(|err| {
        PedelecError::with_details(
            error_codes::TOOL_ARGS_INVALID,
            "tool args must be valid JSON",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;

    Ok(ToolCallInput {
        thread_id,
        tool_name: args[2].clone(),
        args: json_args,
    })
}

fn parse_tool_spec_args(args: &[String]) -> Result<ToolSpecInput, PedelecError> {
    if args.len() != 3 {
        return Err(PedelecError::new(
            error_codes::TOOL_ARGS_INVALID,
            "usage: pedelec-cli tool-spec <tool_name>",
        ));
    }

    Ok(ToolSpecInput {
        thread_id: thread_id_from_env()?,
        tool_name: args[2].clone(),
    })
}

fn thread_id_from_env() -> Result<String, PedelecError> {
    std::env::var("PEDELEC_THREAD_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::PEDELEC_THREAD_ID_NOT_FOUND,
                "PEDELEC_THREAD_ID is required for pedelec-cli tool-call.",
            )
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
        let _env = ThreadIdEnvGuard::set(Some("thread_1"));
        let response = run_tool_cli(vec![
            "pedelec-cli".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{".into(),
        ]);

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    #[test]
    fn new_tool_call_format_reads_thread_id_from_env() {
        let _env = ThreadIdEnvGuard::set(Some("thread_1"));

        let input = parse_tool_call_args(&[
            "pedelec-cli".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap();

        assert_eq!(input.thread_id, "thread_1");
        assert_eq!(input.tool_name, "get_app_state");
        assert_eq!(input.args, serde_json::json!({}));
    }

    #[test]
    fn tool_spec_format_reads_thread_id_from_env() {
        let _env = ThreadIdEnvGuard::set(Some("thread_1"));

        let command = parse_tool_cli_args(&[
            "pedelec-cli".into(),
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
    fn missing_thread_id_env_returns_specific_error() {
        let _env = ThreadIdEnvGuard::set(None);

        let err = parse_tool_call_args(&[
            "pedelec-cli".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, error_codes::PEDELEC_THREAD_ID_NOT_FOUND);
        assert_eq!(
            err.message,
            "PEDELEC_THREAD_ID is required for pedelec-cli tool-call."
        );
    }

    #[test]
    fn blank_thread_id_env_returns_specific_error() {
        let _env = ThreadIdEnvGuard::set(Some("   "));

        let err = parse_tool_call_args(&[
            "pedelec-cli".into(),
            "tool-call".into(),
            "get_app_state".into(),
            "{}".into(),
        ])
        .unwrap_err();

        assert_eq!(err.code, error_codes::PEDELEC_THREAD_ID_NOT_FOUND);
    }
}
