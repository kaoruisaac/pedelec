use pedelec_core::{
    CoreRuntime, DenoExecutionIntent, DenoExecutionTarget, DenoRunOutput, DenoRuntimeDispatcher,
    OpenWorkspaceInput, PedelecError, ThreadStatus, WorkspaceManager,
};
use pedelec_ipc::{
    start_core_ipc_server_with_runtime_path_services_dispatchers, NoopCoreIpcPlatformServices,
    RejectPersistentRuntimeDispatcher,
};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const HELPERS: [&str; 2] = [
    env!("CARGO_BIN_EXE_pedelec-ts-runtime"),
    env!("CARGO_BIN_EXE_pedelec-deno"),
];

#[derive(Default)]
struct RecordingRuntime(Mutex<Vec<DenoExecutionIntent>>);

impl DenoRuntimeDispatcher for RecordingRuntime {
    fn dispatch(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
        self.0.lock().unwrap().push(intent);
        Ok(DenoRunOutput {
            exit_code: 7,
            stdout: "same runtime result\n".into(),
            stderr: "same stderr\n".into(),
            stdout_truncated: false,
            stderr_truncated: true,
        })
    }
}

fn execute(helper: &str, runtime_file: &Path, args: &[&str], source: Option<&str>) -> Value {
    let mut child = Command::new(helper)
        .env("PEDELEC_CORE_IPC_RUNTIME_FILE", runtime_file)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(source) = source {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    // Preserve the legacy process status; script exit codes live in the JSON result.
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn both_executables_preserve_targets_args_results_and_core_thread_modes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("script.ts"), "console.log('file');").unwrap();
    let mut core = CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
        settings_file_path: Some(temp.path().join("settings.json")),
        ..CoreRuntime::default()
    };
    let workspace_id = core
        .open_workspace(
            OpenWorkspaceInput {
                path: workspace.clone(),
            },
            "https://test.example",
            None,
        )
        .unwrap()
        .workspace_id;
    let authorization = core
        .grant_extension_runtime_full_access("https://test.example")
        .unwrap();
    let mut threads = Vec::new();
    for full in [false, true] {
        let input = serde_json::from_value(json!({
            "provider": "codex", "model": "test-model", "runtimeFullAccess": full,
            "workspaceId": workspace_id,
        }))
        .unwrap();
        let created = if full {
            core.create_trusted_extension_thread(input, &authorization)
                .unwrap()
        } else {
            core.create_thread(input).unwrap()
        };
        core.thread_manager
            .thread_mut(&created.thread_id)
            .unwrap()
            .status = ThreadStatus::Running;
        core.thread_manager
            .provider_state_mut(&created.thread_id)
            .unwrap()
            .active_provider_turn_id = Some("turn".into());
        threads.push((created.thread_id, full));
    }
    let dispatcher = Arc::new(RecordingRuntime::default());
    let runtime_file = temp.path().join("runtime.json");
    start_core_ipc_server_with_runtime_path_services_dispatchers(
        Arc::new(Mutex::new(core)),
        &runtime_file,
        Arc::new(NoopCoreIpcPlatformServices),
        Arc::new(RejectPersistentRuntimeDispatcher),
        dispatcher.clone(),
    )
    .unwrap();
    for (thread_id, full) in threads {
        for target in ["script.ts", "-"] {
            let mut responses = Vec::new();
            for helper in HELPERS {
                responses.push(execute(
                    helper,
                    &runtime_file,
                    &[
                        "--thread-id",
                        &thread_id,
                        "run",
                        target,
                        "--",
                        "--allow-all",
                        "value with spaces",
                    ],
                    (target == "-").then_some("console.log('stdin');"),
                ));
                let intent = dispatcher.0.lock().unwrap().pop().unwrap();
                assert_eq!(intent.thread_id, thread_id);
                assert_eq!(intent.runtime_full_access, full);
                assert_eq!(intent.runtime_authorization.is_some(), full);
                assert_eq!(intent.args, ["--allow-all", "value with spaces"]);
                assert_eq!(
                    intent.target,
                    if target == "-" {
                        DenoExecutionTarget::StdinSource {
                            source: "console.log('stdin');".into(),
                        }
                    } else {
                        DenoExecutionTarget::WorkspaceFile {
                            entrypoint: workspace.canonicalize().unwrap().join("script.ts"),
                        }
                    }
                );
            }
            assert_eq!(responses[0], responses[1]);
            assert_eq!(responses[0]["ok"], true);
            assert_eq!(responses[0]["result"]["exitCode"], 7);
            assert_eq!(responses[0]["result"]["stderrTruncated"], true);
        }
    }
}

#[test]
fn both_executables_reject_runtime_flags_subcommands_and_traversal() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["--thread-id", "t", "eval", "code"],
        vec!["--thread-id", "t", "--allow-all", "run", "script.ts"],
        vec!["--thread-id", "t", "run", "../outside.ts"],
        vec![
            "--thread-id",
            "t",
            "run",
            "script.ts",
            "--runtime-full-access",
        ],
        vec!["--thread-id", "t", "run", "script.ts", "&&", "echo", "nope"],
    ] {
        for helper in HELPERS {
            let response = execute(
                helper,
                &temp.path().join("missing-runtime.json"),
                &args,
                None,
            );
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"]["code"], "DENO_ARGS_INVALID");
        }
    }
}
