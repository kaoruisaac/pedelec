use super::*;
use pedelec_core::{CoreRuntime, ExtensionRuntimeAuthorization, WorkspaceManager};

const ORIGIN: &str = "https://runtime.example.test";

#[path = "../../pedelec-runtime/src/process_tree_test_support.rs"]
mod tree_fixture;

#[test]
fn real_runtime_revoke_ipc_cleans_only_its_origin_process_tree_and_denies_reuse() {
    use pedelec_core::{DenoRunInput, DenoRunTarget, ResumeThreadInput, ThreadStatus};
    use pedelec_runtime::{DenoRuntimeOwner, ManagedDenoRuntime};
    use std::time::Duration;
    let executable = if let Some(path) = std::env::var_os("PEDELEC_TEST_DENO") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "invalid PEDELEC_TEST_DENO");
        path
    } else {
        let path = pedelec_shared::paths::pedelec_home_dir()
            .ok()
            .and_then(|home| {
                pedelec_shared::deno_release::managed_runtime_executable_for_current(&home).ok()
            });
        let Some(path) = path.filter(|path| path.is_file()) else {
            eprintln!("skipping process-tree IPC fixture: provision managed Deno or set PEDELEC_TEST_DENO");
            return;
        };
        path
    };
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspaces")),
        ..CoreRuntime::default()
    }));
    let owner = Arc::new(DenoRuntimeOwner::new());
    owner
        .configure(ManagedDenoRuntime::new(executable, temp.path().join("cache")).unwrap())
        .unwrap();
    let mut runs = Vec::new();
    for (index, origin) in [ORIGIN, "https://other.example.test"]
        .into_iter()
        .enumerate()
    {
        let auth = runtime
            .lock()
            .unwrap()
            .grant_extension_runtime_full_access(origin)
            .unwrap();
        let platform = Arc::new(SessionPlatform {
            authorization: auth,
            instance_requested: true,
        });
        let created = handle_core_ipc_request_with_services(
            request(Some(origin), true),
            runtime.clone(),
            platform.clone(),
            Arc::new(RejectPersistentRuntimeDispatcher),
            owner.clone(),
        );
        assert!(created.ok, "{:?}", created.error);
        let id = created.result.unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let marker = temp.path().join(format!("pids-{index}.json"));
        let intent = {
            let mut core = runtime.lock().unwrap();
            core.thread_manager.thread_mut(&id).unwrap().status = ThreadStatus::Running;
            core.thread_manager
                .provider_state_mut(&id)
                .unwrap()
                .active_provider_turn_id = Some("active-turn".into());
            core.prepare_deno_run_intent(DenoRunInput {
                thread_id: id.clone(),
                target: DenoRunTarget::StdinSource {
                    source: tree_fixture::source(&marker, false),
                },
                args: vec![],
            })
            .unwrap()
        };
        let dispatch_owner = owner.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(dispatch_owner.dispatch(intent));
        });
        let pids = tree_fixture::wait_pids(&marker);
        let cleanup = tree_fixture::Cleanup(pids.clone());
        assert!(pids.iter().all(|pid| tree_fixture::alive(*pid)));
        runs.push((id, platform, pids, cleanup, rx, worker));
    }
    let (id, platform, pids, _cleanup, rx, worker) = runs.remove(0);
    let mut revoke = request(Some(ORIGIN), false);
    revoke.r#type = "revoke_runtime_full_access".into();
    let response = handle_core_ipc_request_with_services(
        revoke,
        runtime.clone(),
        platform,
        Arc::new(RejectPersistentRuntimeDispatcher),
        owner.clone(),
    );
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(
        response.result.unwrap()["threadIds"],
        serde_json::json!([id])
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err()
            .code,
        error_codes::DENO_EXECUTION_CANCELLED
    );
    worker.join().unwrap();
    tree_fixture::assert_stopped(&pids);
    assert_eq!(owner.active_run_count(), 1);
    let (other_id, _, other_pids, _other_cleanup, other_rx, other_worker) = runs.remove(0);
    assert!(owner.is_thread_active(&other_id));
    assert!(other_pids.iter().all(|pid| tree_fixture::alive(*pid)));
    {
        let mut core = runtime.lock().unwrap();
        assert!(core
            .prepare_deno_run_intent(DenoRunInput {
                thread_id: id.clone(),
                target: DenoRunTarget::StdinSource { source: "0".into() },
                args: vec![]
            })
            .is_err());
        core.thread_manager.thread_mut(&id).unwrap().status = ThreadStatus::Ended;
        assert!(core
            .resume_thread(ResumeThreadInput { thread_id: id })
            .is_err());
    }
    assert!(owner.shutdown_all().is_empty());
    assert_eq!(
        other_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err()
            .code,
        error_codes::DENO_EXECUTION_CANCELLED
    );
    other_worker.join().unwrap();
    tree_fixture::assert_stopped(&other_pids);
    assert_eq!(owner.active_run_count(), 0);
}

// A fixture for the out-of-band Native adapter. A serialized request cannot
// install this service or manufacture its Core-issued session capability.
struct SessionPlatform {
    authorization: ExtensionRuntimeAuthorization,
    instance_requested: bool,
}

impl CoreIpcPlatformServices for SessionPlatform {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError> {
        Ok(None)
    }
    fn authorize_extension_runtime_request(
        &self,
        request: &CoreIpcRequest,
    ) -> Result<ExtensionRuntimeAuthorization, PedelecError> {
        if request.r#type == "create_thread" && !self.instance_requested {
            return Err(PedelecError::new(
                error_codes::IPC_UNAUTHORIZED,
                "Instance did not request Full Access",
            ));
        }
        Ok(self.authorization.clone())
    }
}

#[derive(Default)]
struct Cancellations(Mutex<Vec<String>>);
impl DenoRuntimeDispatcher for Cancellations {
    fn dispatch(&self, _intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
        unreachable!()
    }
    fn cancel_thread(&self, id: &str) {
        self.0.lock().unwrap().push(id.into());
    }
}

fn request(origin: Option<&str>, full: bool) -> CoreIpcRequest {
    CoreIpcRequest {
        request_id: "runtime-create".into(),
        r#type: "create_thread".into(),
        caller_origin: origin.map(str::to_string),
        caller_sdk_version: None,
        payload: Some(serde_json::json!({
            "provider": "codex", "model": "test-model", "runtimeFullAccess": full,
            "runtimeFullAccessApproved": true, "trustedNative": true,
        })),
    }
}

fn dispatch(
    request: CoreIpcRequest,
    runtime: SharedCoreRuntime,
    platform: Arc<dyn CoreIpcPlatformServices>,
    deno: Arc<Cancellations>,
) -> CoreIpcResponse {
    handle_core_ipc_request_with_services(
        request,
        runtime,
        platform,
        Arc::new(RejectPersistentRuntimeDispatcher),
        deno,
    )
}

#[test]
fn ordinary_native_and_sdk_callers_cannot_self_assert_full_access() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspace")),
        ..CoreRuntime::default()
    }));
    // An existing Origin grant is still insufficient without the managed Session.
    runtime
        .lock()
        .unwrap()
        .grant_extension_runtime_full_access(ORIGIN)
        .unwrap();
    for origin in [None, Some(ORIGIN)] {
        let denied = handle_core_ipc_request(request(origin, true), runtime.clone());
        assert!(!denied.ok);
        assert!(denied.result.is_none());
    }
    let sandbox = handle_core_ipc_request(request(Some(ORIGIN), false), runtime);
    assert!(sandbox.ok);
    assert_eq!(sandbox.result.unwrap()["runtimeFullAccessApplied"], false);
}

#[test]
fn trusted_session_capability_is_required_and_revocation_cancels_only_its_origin() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspace")),
        ..CoreRuntime::default()
    }));
    let auth = runtime
        .lock()
        .unwrap()
        .grant_extension_runtime_full_access(ORIGIN)
        .unwrap();
    let other_auth = runtime
        .lock()
        .unwrap()
        .grant_extension_runtime_full_access("https://other.example.test")
        .unwrap();
    let platform = Arc::new(SessionPlatform {
        authorization: auth.clone(),
        instance_requested: true,
    });
    let deno = Arc::new(Cancellations::default());
    let denied_instance = dispatch(
        request(Some(ORIGIN), true),
        runtime.clone(),
        Arc::new(SessionPlatform {
            authorization: auth.clone(),
            instance_requested: false,
        }),
        deno.clone(),
    );
    assert!(!denied_instance.ok);
    let spoofed_origin = dispatch(
        request(Some("https://other.example.test"), true),
        runtime.clone(),
        platform.clone(),
        deno.clone(),
    );
    assert!(!spoofed_origin.ok);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = dispatch(
            request(Some(ORIGIN), true),
            runtime.clone(),
            platform.clone(),
            deno.clone(),
        );
        assert!(response.ok, "{:?}", response.error);
        let result = response.result.unwrap();
        assert_eq!(result["runtimeFullAccessApplied"], true);
        ids.push(result["threadId"].as_str().unwrap().to_string());
    }
    let other = dispatch(
        request(Some("https://other.example.test"), true),
        runtime.clone(),
        Arc::new(SessionPlatform {
            authorization: other_auth,
            instance_requested: true,
        }),
        deno.clone(),
    );
    assert!(other.ok);
    let mut revoke = request(Some(ORIGIN), false);
    revoke.r#type = "revoke_runtime_full_access".into();
    revoke.payload = Some(serde_json::json!({ "origin": "https://other.example.test" }));
    assert!(!handle_core_ipc_request(revoke.clone(), runtime.clone()).ok);
    let response = dispatch(revoke, runtime.clone(), platform.clone(), deno.clone());
    assert!(response.ok);
    ids.sort();
    let mut cancelled = deno.0.lock().unwrap().clone();
    cancelled.sort();
    assert_eq!(cancelled, ids);
    assert!(!dispatch(request(Some(ORIGIN), true), runtime, platform, deno).ok);
}

#[test]
fn workspace_full_access_route_requires_trusted_native_capability() {
    struct WorkspaceDispatcher;
    impl DenoRuntimeDispatcher for WorkspaceDispatcher {
        fn dispatch(&self, intent: DenoExecutionIntent) -> Result<DenoRunOutput, PedelecError> {
            assert!(intent.runtime_full_access);
            assert!(intent.runtime_authorization.as_ref().unwrap().is_active());
            assert!(matches!(
                intent.owner,
                pedelec_core::DenoExecutionOwner::Workspace { .. }
            ));
            Ok(DenoRunOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspace")),
        ..CoreRuntime::default()
    }));
    let auth = runtime
        .lock()
        .unwrap()
        .grant_extension_runtime_full_access(ORIGIN)
        .unwrap();
    let created = runtime.lock().unwrap().create_trusted_extension_thread(serde_json::from_value(serde_json::json!({"provider":"codex", "model":"test-model", "runtimeFullAccess":true})).unwrap(), &auth).unwrap();
    let run = CoreIpcRequest {
        request_id: "workspace-run".into(),
        r#type: "workspace_run".into(),
        caller_origin: Some(ORIGIN.into()),
        caller_sdk_version: None,
        payload: Some(
            serde_json::json!({"workspaceId":created.workspace_id, "script":"0", "runtimeFullAccess":true}),
        ),
    };
    assert!(!handle_core_ipc_request(run.clone(), runtime.clone()).ok);
    let response = handle_core_ipc_request_with_services(
        run,
        runtime,
        Arc::new(SessionPlatform {
            authorization: auth,
            instance_requested: true,
        }),
        Arc::new(RejectPersistentRuntimeDispatcher),
        Arc::new(WorkspaceDispatcher),
    );
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.unwrap()["runtimeFullAccessApplied"], true);
}
