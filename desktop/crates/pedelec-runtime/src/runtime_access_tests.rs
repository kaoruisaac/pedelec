use super::*;
use pedelec_core::{
    CoreRuntime, CreateThreadInput, DenoRunInput, DenoRunTarget, ThreadStatus, WorkspaceManager,
};

const ORIGIN: &str = "https://runtime.example.test";

#[path = "process_tree_test_support.rs"]
mod tree_fixture;

#[test]
fn tree_cleanup_failures_and_capture_failures_preserve_terminal_codes_and_diagnostics() {
    let run = ActiveDenoRun {
        child: Mutex::new(None),
        cancel_requested: AtomicBool::new(true),
        cleanup_error: Mutex::new(Some(
            "process-tree cleanup failed; descendants may remain".into(),
        )),
    };
    // Repeated cleanup must retain a failure already observed by cancel_thread.
    assert!(terminate_active_child(&run).is_some());
    for error in [
        cancelled_error("test", Err("stdout capture thread did not finish".into())),
        timeout_error("test", Err("stderr capture thread did not finish".into())),
    ] {
        let code = error.code.clone();
        let error = with_cleanup_diagnostics(error, &run);
        assert_eq!(error.code, code);
        let details = error.details.unwrap();
        assert!(details["processTreeCleanupError"]
            .as_str()
            .unwrap()
            .contains("descendants may remain"));
        assert!(details["captureError"]
            .as_str()
            .unwrap()
            .contains("capture thread did not finish"));
    }
}

#[test]
fn real_full_access_process_tree_lifecycle() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    for action in ["cancel", "revoke", "timeout", "shutdown", "normal_exit"] {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("pids.json");
        let (mut core, mut intent) = full_intent(
            &temp,
            &tree_fixture::source(&marker, action == "normal_exit"),
        );
        intent.timeout_ms = if action == "timeout" { 2000 } else { 15000 };
        let owner = configured(&executable, &temp, 4096);
        let id = intent.thread_id.clone();
        let dispatch_owner = owner.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = tx.send(dispatch_owner.dispatch(intent));
        });
        let pids = tree_fixture::wait_pids(&marker);
        let _cleanup = tree_fixture::Cleanup(pids.clone());
        if action != "normal_exit" {
            assert!(
                pids.iter().all(|pid| tree_fixture::alive(*pid)),
                "{action}: {pids:?}"
            );
        }
        match action {
            "cancel" => owner.cancel_thread(&id),
            "revoke" => {
                core.revoke_runtime_full_access(ORIGIN).unwrap();
            }
            "shutdown" => assert!(owner.shutdown_all().is_empty()),
            _ => {}
        }
        let result = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("dispatch must finish with inherited output pipes");
        if action == "normal_exit" {
            assert_eq!(result.unwrap().exit_code, 0);
        } else {
            assert_eq!(
                result.unwrap_err().code,
                if action == "timeout" {
                    error_codes::DENO_EXECUTION_TIMEOUT
                } else {
                    error_codes::DENO_EXECUTION_CANCELLED
                },
                "{action}"
            );
        }
        worker.join().unwrap();
        tree_fixture::assert_stopped(&pids);
        assert_eq!(owner.active_run_count(), 0, "{action}");
    }
}

#[test]
fn real_process_tree_cancel_does_not_affect_another_thread_or_workspace() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("first.json");
    let (_core, intent) = full_intent(&temp, &tree_fixture::source(&marker, false));
    let id = intent.thread_id.clone();
    let other_temp = tempfile::tempdir().unwrap();
    let other_marker = other_temp.path().join("other.json");
    let (_other_core, mut other) =
        full_intent(&other_temp, &tree_fixture::source(&other_marker, false));
    // These fixture Cores each start their local thread counter at one.
    other.thread_id = format!("other-{}", other.thread_id);
    other.owner = DenoExecutionOwner::Thread {
        thread_id: other.thread_id.clone(),
    };
    let other_id = other.thread_id.clone();
    let owner = configured(&executable, &temp, 4096);
    let first_owner = owner.clone();
    let first = thread::spawn(move || first_owner.dispatch(intent));
    let other_owner = owner.clone();
    let second = thread::spawn(move || other_owner.dispatch(other));
    let pids = tree_fixture::wait_pids(&marker);
    let other_pids = tree_fixture::wait_pids(&other_marker);
    let _cleanup = tree_fixture::Cleanup([pids.clone(), other_pids.clone()].concat());
    // A workspace invocation uses the same container infrastructure with the
    // original Sandbox permissions. Keep it alive while cancelling the thread.
    let workspace = DenoExecutionIntent {
        runtime_full_access: false,
        runtime_authorization: None,
        thread_id: String::new(),
        owner: DenoExecutionOwner::Workspace {
            workspace_id: "unrelated".into(),
            run_id: "workspace-run".into(),
        },
        workspace_path: temp.path().into(),
        target: DenoExecutionTarget::StdinSource {
            source: "setInterval(() => {}, 1000)".into(),
        },
        args: vec![],
        timeout_ms: 15000,
        import_map_path: None,
    };
    let workspace_owner = owner.clone();
    let workspace_worker = thread::spawn(move || workspace_owner.dispatch(workspace));
    let start = Instant::now();
    while owner.active_run_count() != 3 {
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }
    owner.cancel_thread(&id);
    assert_eq!(
        first.join().unwrap().unwrap_err().code,
        error_codes::DENO_EXECUTION_CANCELLED
    );
    tree_fixture::assert_stopped(&pids);
    assert!(other_pids.iter().all(|pid| tree_fixture::alive(*pid)));
    assert!(owner.is_thread_active(&other_id));
    assert_eq!(owner.active_run_count(), 2);
    assert!(owner.shutdown_all().is_empty());
    assert_eq!(
        second.join().unwrap().unwrap_err().code,
        error_codes::DENO_EXECUTION_CANCELLED
    );
    assert_eq!(
        workspace_worker.join().unwrap().unwrap_err().code,
        error_codes::DENO_EXECUTION_CANCELLED
    );
    tree_fixture::assert_stopped(&other_pids);
    assert_eq!(owner.active_run_count(), 0);
}

fn full_intent(temp: &tempfile::TempDir, source: &str) -> (CoreRuntime, DenoExecutionIntent) {
    let mut core = CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspace")),
        ..CoreRuntime::default()
    };
    let grant = core.grant_extension_runtime_full_access(ORIGIN).unwrap();
    let input: CreateThreadInput = serde_json::from_value(serde_json::json!({
        "provider": "codex", "model": "test-model", "runtimeFullAccess": true,
    }))
    .unwrap();
    let output = core.create_trusted_extension_thread(input, &grant).unwrap();
    core.thread_manager
        .thread_mut(&output.thread_id)
        .unwrap()
        .status = ThreadStatus::Running;
    core.thread_manager
        .provider_state_mut(&output.thread_id)
        .unwrap()
        .active_provider_turn_id = Some("active-turn".into());
    let intent = core
        .prepare_deno_run_intent(DenoRunInput {
            thread_id: output.thread_id,
            target: DenoRunTarget::StdinSource {
                source: source.into(),
            },
            args: vec!["--deny-read".into(), "--import-map=attacker.json".into()],
        })
        .unwrap();
    assert!(intent.runtime_full_access);
    (core, intent)
}

fn configured(executable: &Path, temp: &tempfile::TempDir, cap: usize) -> DenoRuntimeOwner {
    let owner = DenoRuntimeOwner::with_policy(DenoRuntimePolicy {
        execution_timeout: Duration::from_secs(5),
        stdout_cap_bytes: cap,
        stderr_cap_bytes: cap,
    });
    owner
        .configure(ManagedDenoRuntime::new(executable, temp.path().join("cache")).unwrap())
        .unwrap();
    owner
}

fn wait_for_child(owner: &DenoRuntimeOwner) {
    let started = Instant::now();
    loop {
        let spawned = owner
            .inner
            .active_runs
            .lock()
            .unwrap()
            .values()
            .any(|entry| entry.run.child.lock().unwrap().is_some());
        if spawned {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Runtime child did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn full_access_argv_has_no_sandbox_restrictions_and_arguments_stay_after_entrypoint() {
    let temp = tempfile::tempdir().unwrap();
    let (_core, intent) = full_intent(&temp, "0");
    let prepared = PreparedDenoExecution::prepare(&intent).unwrap();
    let args = build_deno_command_args(&prepared);
    let strings: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
    let boundary = strings.iter().position(|arg| arg == "--").unwrap();
    assert!(strings[..boundary].iter().any(|arg| arg == "--allow-all"));
    assert!(!strings[..boundary]
        .iter()
        .any(|arg| arg.starts_with("--deny-")
            || ["--no-remote", "--cached-only", "--no-npm"].contains(&arg.as_ref())));
    assert_eq!(
        &strings[boundary + 1..],
        ["-", "--deny-read", "--import-map=attacker.json"]
    );
    let mut workspace = intent.clone();
    workspace.owner = DenoExecutionOwner::Workspace {
        workspace_id: "workspace".into(),
        run_id: "run".into(),
    };
    let sandbox = PreparedDenoExecution::prepare(&workspace).unwrap();
    assert!(!sandbox.runtime_full_access);
    assert!(build_deno_command_args(&sandbox).contains(&OsString::from("--deny-net")));
    // A serialized capability cannot bypass the in-process admission boundary.
    let restored: DenoExecutionIntent =
        serde_json::from_value(serde_json::to_value(&intent).unwrap()).unwrap();
    assert!(PreparedDenoExecution::prepare(&restored).is_err());
}

#[test]
fn revoked_prepared_intent_cannot_spawn_and_outside_entrypoint_stays_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let (mut core, mut intent) = full_intent(&temp, "0");
    let outside = temp.path().join("outside.ts");
    fs::write(&outside, "0").unwrap();
    intent.target = DenoExecutionTarget::WorkspaceFile {
        entrypoint: outside,
    };
    assert_eq!(
        PreparedDenoExecution::prepare(&intent).unwrap_err().code,
        error_codes::DENO_ENTRYPOINT_INVALID
    );
    core.revoke_runtime_full_access(ORIGIN).unwrap();
    assert!(PreparedDenoExecution::prepare(&intent).is_err());
}

#[test]
fn real_full_access_can_use_external_files_host_environment_subprocesses_sys_and_remote_modules() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.txt");
    let outside_json = serde_json::to_string(&outside).unwrap();
    // Serve a remote dependency locally: no registry availability or external data.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    use std::io::{Read, Write};
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut buffer = [0; 8192];
                    // HTTP clients may open and discard a speculative connection.
                    // Do not consume our single fixture response until headers arrive.
                    let mut request = Vec::new();
                    loop {
                        let read = stream.read(&mut buffer).unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            break;
                        }
                        assert!(request.len() <= 16384);
                    }
                    if request.is_empty() {
                        continue;
                    }
                    let body = "export const value = 'remote-ok';";
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("remote fixture: {error}"),
            }
        }
        panic!("remote fixture did not receive a request");
    });
    let source = format!(
        r#"
const path = {outside_json};
await Deno.writeTextFile(path, 'outside-ok');
if (await Deno.readTextFile(path) !== 'outside-ok') throw Error('outside read');
const envKey = Deno.build.os === 'windows' ? 'SystemRoot' : 'PATH';
if (!Deno.env.get(envKey)) throw Error('host environment missing');
if (!Deno.env.get('DENO_DIR')?.includes('cache')) throw Error('managed DENO_DIR');
if (!(await Deno.permissions.query({{ name: 'ffi' }})).state.includes('granted')) throw Error('ffi');
if (!Deno.hostname()) throw Error('sys');
const child = await new Deno.Command(Deno.execPath(), {{ args: ['eval', "console.log('child-ok')"] }}).output();
if (!child.success || !new TextDecoder().decode(child.stdout).includes('child-ok')) throw Error('run');
const remote = await import('http://{address}/module.js');
if (remote.value !== 'remote-ok') throw Error('remote');
if (Deno.args[0] !== '--deny-read' || Deno.args[1] !== '--import-map=attacker.json') throw Error('args');
console.log('full-access-ok');
"#
    );
    let (_core, intent) = full_intent(&temp, &source);
    let owner = configured(&executable, &temp, 4096);
    let output = owner.dispatch(intent).unwrap();
    server.join().unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("full-access-ok"));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside-ok");
    assert_eq!(owner.active_run_count(), 0);
}

#[test]
fn real_full_access_retains_timeout_output_caps_and_revocation_cancellation() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let (mut core, mut intent) = full_intent(
        &temp,
        "console.log('x'.repeat(4096)); console.error('y'.repeat(4096));",
    );
    let owner = configured(&executable, &temp, 128);
    let output = owner.dispatch(intent.clone()).unwrap();
    assert!(output.stdout_truncated && output.stderr_truncated);
    assert!(output.stdout.len() <= 128 && output.stderr.len() <= 128);
    intent.target = DenoExecutionTarget::StdinSource {
        source: "setInterval(() => {}, 1000); await new Promise(() => {});".into(),
    };
    intent.timeout_ms = 200;
    assert_eq!(
        owner.dispatch(intent.clone()).unwrap_err().code,
        error_codes::DENO_EXECUTION_TIMEOUT
    );
    intent.timeout_ms = 5000;
    let id = intent.thread_id.clone();
    let dispatch_owner = owner.clone();
    let worker = thread::spawn(move || dispatch_owner.dispatch(intent));
    let started = Instant::now();
    while !owner.is_thread_active(&id) && started.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(owner.is_thread_active(&id));
    wait_for_child(&owner);
    core.revoke_runtime_full_access(ORIGIN).unwrap();
    let error = worker.join().unwrap().unwrap_err();
    assert!([
        error_codes::DENO_EXECUTION_CANCELLED,
        error_codes::INVALID_INPUT
    ]
    .contains(&error.code.as_str()));
    assert_eq!(owner.active_run_count(), 0);
}

#[test]
fn real_full_access_respects_explicit_cancel_and_shutdown() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    for shutdown in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (_core, intent) = full_intent(
            &temp,
            "setInterval(() => {}, 1000); await new Promise(() => {});",
        );
        let id = intent.thread_id.clone();
        let owner = configured(&executable, &temp, 4096);
        let dispatch_owner = owner.clone();
        let worker = thread::spawn(move || dispatch_owner.dispatch(intent));
        let started = Instant::now();
        while !owner.is_thread_active(&id) && started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(owner.is_thread_active(&id));
        wait_for_child(&owner);
        if shutdown {
            assert!(owner.shutdown_all().is_empty());
        } else {
            owner.cancel_thread(&id);
        }
        assert_eq!(
            worker.join().unwrap().unwrap_err().code,
            error_codes::DENO_EXECUTION_CANCELLED
        );
        assert_eq!(owner.active_run_count(), 0);
    }
}

#[test]
#[ignore = "requires access to the public npm registry"]
fn real_full_access_loads_npm_through_the_managed_cache() {
    let Some(executable) = super::deno_execution_fixture::test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let (_core, intent) = full_intent(&temp, "const { default: isNumber } = await import('npm:is-number@7.0.0'); if (!isNumber(42)) throw Error('npm'); console.log('npm-ok');");
    let owner = configured(&executable, &temp, 4096);
    let output = owner.dispatch(intent).unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("npm-ok"));
    assert!(!temp.path().join("workspace/node_modules").exists());
    assert!(temp.path().join("cache/npm").is_dir());
}
