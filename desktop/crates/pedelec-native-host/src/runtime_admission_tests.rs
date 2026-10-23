use super::*;
use pedelec_core::{CoreRuntime, DenoRunInput, DenoRunTarget, ThreadStatus, WorkspaceManager};
use pedelec_ipc::{
    send_core_ipc_request_with_runtime_path,
    start_core_ipc_server_with_runtime_path_services_dispatchers, NoopCoreIpcPlatformServices,
    RejectPersistentRuntimeDispatcher,
};
use pedelec_runtime::{DenoRuntimeOwner, ManagedDenoRuntime};
use serde_json::json;

fn message(origin: &str, instance: &str) -> Value {
    json!({ "type": "create_thread", "requestId": "admission", "callerOrigin": origin,
        "provider": "codex", "model": "test-model", "runtimeFullAccess": true,
        "nativeRuntimeGrant": { "version": 1, "origin": origin, "instanceId": instance,
            "instanceRequested": true, "agentSessionsApproved": true, "runtimeFullAccessApproved": true } })
}

#[test]
fn production_native_ingress_and_desktop_adapter_bind_grants_and_revoke() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspace")),
        ..CoreRuntime::default()
    }));
    let path = temp.path().join("runtime.json");
    // Exactly the adapter installed by the Desktop IPC initializer, no fake
    // SessionPlatform and no direct Core grant calls.
    let owner = Arc::new(DenoRuntimeOwner::new());
    let _server = start_core_ipc_server_with_runtime_path_services_dispatchers(
        runtime.clone(),
        &path,
        Arc::new(NoopCoreIpcPlatformServices),
        Arc::new(RejectPersistentRuntimeDispatcher),
        owner.clone(),
    )
    .unwrap();
    let origin = "https://native.example.test";
    let mut sandbox = message(origin, "sandbox");
    sandbox["runtimeFullAccess"] = json!(false);
    sandbox
        .as_object_mut()
        .unwrap()
        .remove("nativeRuntimeGrant");
    let sandbox = admit_native_message(sandbox, Some(&path)).unwrap();
    assert_eq!(
        send_core_ipc_request_with_runtime_path(&sandbox, &path)
            .unwrap()
            .result
            .unwrap()["runtimeFullAccessApplied"],
        false
    );
    let send =
        |request: &CoreIpcRequest| send_core_ipc_request_with_runtime_path(request, &path).unwrap();
    let raw = native_message_to_core_request(message(origin, "one")).unwrap();
    assert!(!send(&raw).ok); // Plain IPC cannot claim the envelope or approved:true.
    for field in [
        "instanceRequested",
        "agentSessionsApproved",
        "runtimeFullAccessApproved",
    ] {
        let mut invalid = message(origin, "one");
        invalid["nativeRuntimeGrant"][field] = json!(false);
        assert!(admit_native_message(invalid, Some(&path)).is_err());
    }
    let mut missing = message(origin, "one");
    missing
        .as_object_mut()
        .unwrap()
        .remove("nativeRuntimeGrant");
    missing["runtimeFullAccessApproved"] = json!(true);
    assert!(admit_native_message(missing, Some(&path)).is_err());
    let admitted = admit_native_message(message(origin, "one"), Some(&path)).unwrap();
    for field in ["origin", "instanceId"] {
        let mut forged = admitted.clone();
        forged.payload.as_mut().unwrap()["_nativeRuntimeProof"]["grant"][field] = json!("forged");
        assert!(!send(&forged).ok);
    }
    let mut other = admitted.clone();
    other.caller_origin = Some("https://other.example.test".into());
    assert!(!send(&other).ok);
    let response = send(&admitted);
    assert!(response.ok, "{:?}", response.error);
    let output = response.result.unwrap();
    assert_eq!(output["runtimeFullAccessApplied"], true);
    let id = output["threadId"].as_str().unwrap();
    assert!(!send(&admitted).ok); // One-time proof, even for an identical request.
    let second =
        admit_native_message(message("https://other.example.test", "two"), Some(&path)).unwrap();
    let other_output = send(&second).result.unwrap();
    let other_id = other_output["threadId"].as_str().unwrap();

    // Real runtime execution from a Thread created by the production bridge.
    let executable = std::env::var_os("PEDELEC_TEST_DENO")
        .map(PathBuf::from)
        .or_else(|| {
            let home = pedelec_shared::paths::pedelec_home_dir().ok()?;
            pedelec_shared::deno_release::managed_runtime_executable_for_current(&home).ok()
        })
        .filter(|p| p.is_file());
    let mut active = None;
    if let Some(executable) = executable {
        let outside = temp.path().join("outside.txt");
        let source = format!(
            r#"
await Deno.writeTextFile({}, 'bridge-ok');
if (await Deno.readTextFile({}) !== 'bridge-ok') throw Error('read');
if (!Deno.env.get(Deno.build.os === 'windows' ? 'SystemRoot' : 'PATH')) throw Error('env');
const child = await new Deno.Command(Deno.execPath(), {{ args: ['eval', "console.log('child-ok')"] }}).output();
if (!child.success) throw Error('child');
console.log('bridge-ok');
"#,
            serde_json::to_string(&outside).unwrap(),
            serde_json::to_string(&outside).unwrap()
        );
        let intent = {
            let mut core = runtime.lock().unwrap();
            core.thread_manager.thread_mut(id).unwrap().status = ThreadStatus::Running;
            core.thread_manager
                .provider_state_mut(id)
                .unwrap()
                .active_provider_turn_id = Some("test-turn".into());
            core.prepare_deno_run_intent(DenoRunInput {
                thread_id: id.into(),
                target: DenoRunTarget::StdinSource { source },
                args: vec![],
            })
            .unwrap()
        };
        owner
            .configure(ManagedDenoRuntime::new(executable, temp.path().join("cache")).unwrap())
            .unwrap();
        let result = owner.dispatch(intent.clone()).unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert!(result.stdout.contains("bridge-ok"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "bridge-ok");
        let mut long = intent;
        let started = temp.path().join("active-process.txt");
        long.target = pedelec_core::DenoExecutionTarget::StdinSource {
            source: format!("await Deno.writeTextFile({}, 'started'); setInterval(() => {{}}, 1000); await new Promise(() => {{}});",
                serde_json::to_string(&started).unwrap()),
        };
        let runner = owner.clone();
        active = Some(thread::spawn(move || runner.dispatch(long)));
        let start = Instant::now();
        while !started.exists() {
            assert!(start.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        }
    } else {
        eprintln!(
            "SKIP real execution: managed Deno not installed; admission assertions still ran"
        );
    }

    let revoke = admit_native_message(json!({ "type": "revoke_runtime_full_access", "requestId": "revoke",
        "callerOrigin": origin, "nativeRuntimeGrant": { "version": 1, "origin": origin, "revoke": true } }), Some(&path)).unwrap();
    assert_eq!(send(&revoke).result.unwrap()["revoked"], true);
    if let Some(active) = active {
        assert!(active.join().unwrap().is_err());
        assert_eq!(owner.active_run_count(), 0);
    }
    let mut core = runtime.lock().unwrap();
    assert!(core
        .resume_thread(pedelec_core::ResumeThreadInput {
            thread_id: id.into()
        })
        .is_err());
    assert!(core
        .resume_thread(pedelec_core::ResumeThreadInput {
            thread_id: other_id.into()
        })
        .is_ok());
    drop(core);
    let reapproved = admit_native_message(message(origin, "new-instance"), Some(&path)).unwrap();
    assert_eq!(
        send(&reapproved).result.unwrap()["runtimeFullAccessApplied"],
        true
    );
    // New approval grants new Threads, without resurrecting withdrawn ones.
    assert!(runtime
        .lock()
        .unwrap()
        .resume_thread(pedelec_core::ResumeThreadInput {
            thread_id: id.into(),
        })
        .is_err());
}
