use pedelec_core::{error_codes, CoreRuntime, PedelecError, SharedCoreRuntime};
use pedelec_ipc::{
    send_core_ipc_request_with_runtime_path, start_core_ipc_server_with_runtime_path_and_services,
    CoreIpcPlatformServices, CoreIpcRequest,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct FakePlatformServices {
    result: Result<Option<PathBuf>, PedelecError>,
}

impl CoreIpcPlatformServices for FakePlatformServices {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError> {
        self.result.clone()
    }
}

fn open_request(request_id: &str, payload: Option<serde_json::Value>) -> CoreIpcRequest {
    CoreIpcRequest {
        request_id: request_id.into(),
        r#type: "open_workspace".into(),
        caller_origin: Some("https://approved.example".into()),
        caller_sdk_version: Some("mock-sdk-version".into()),
        payload,
    }
}

#[test]
fn picker_open_workspace_initializes_and_returns_a_workspace_capability() {
    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("application-workspace");
    std::fs::create_dir_all(&selected).unwrap();
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    let runtime_path = temp.path().join("runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        Arc::new(FakePlatformServices {
            result: Ok(Some(selected.clone())),
        }),
    )
    .unwrap();

    let response = send_core_ipc_request_with_runtime_path(
        &open_request("open_picker", Some(json!({}))),
        &runtime_path,
    )
    .unwrap();

    assert!(response.ok, "open_workspace failed: {:?}", response.error);
    let workspace = response.result.as_ref().unwrap()["workspace"].clone();
    assert_eq!(workspace["path"], selected.to_string_lossy().to_string());
    assert!(workspace["workspaceId"]
        .as_str()
        .unwrap()
        .starts_with("ws_"));
    assert!(selected.join(".pedelec-workspace.json").exists());
    assert!(runtime
        .lock()
        .unwrap()
        .thread_manager
        .thread("thread_created")
        .is_err());
}

#[test]
fn open_workspace_explicit_path_bypasses_the_picker_and_cancellation_is_clean() {
    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("explicit-workspace");
    std::fs::create_dir_all(&selected).unwrap();
    let runtime_path = temp.path().join("runtime.json");
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        Arc::new(FakePlatformServices {
            result: Err(PedelecError::new(
                error_codes::DIRECTORY_PICKER_FAILED,
                "picker must not run",
            )),
        }),
    )
    .unwrap();

    let explicit = send_core_ipc_request_with_runtime_path(
        &open_request("open_explicit", Some(json!({ "path": selected }))),
        &runtime_path,
    )
    .unwrap();
    assert!(
        explicit.ok,
        "explicit open_workspace failed: {:?}",
        explicit.error
    );
    assert_eq!(
        explicit.result.as_ref().unwrap()["workspace"]["path"],
        selected.to_string_lossy().to_string()
    );

    let cancel_runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    let cancel_path = temp.path().join("cancel.runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        cancel_runtime,
        &cancel_path,
        Arc::new(FakePlatformServices { result: Ok(None) }),
    )
    .unwrap();
    let cancelled =
        send_core_ipc_request_with_runtime_path(&open_request("open_cancel", None), &cancel_path)
            .unwrap();
    assert_eq!(cancelled.result, Some(json!({ "workspace": null })));
}

#[test]
fn open_workspace_requires_a_trusted_caller_origin() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_path = temp.path().join("runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::new(Mutex::new(CoreRuntime::default())),
        &runtime_path,
        Arc::new(FakePlatformServices { result: Ok(None) }),
    )
    .unwrap();

    let mut request = open_request("open_unauthorized", None);
    request.caller_origin = None;
    let response = send_core_ipc_request_with_runtime_path(&request, &runtime_path).unwrap();
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, error_codes::IPC_UNAUTHORIZED);
}
