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

#[test]
fn tcp_picker_request_returns_selected_path_without_creating_session_state() {
    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("application-workspace");
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    let services = Arc::new(FakePlatformServices {
        result: Ok(Some(selected.clone())),
    });
    let runtime_path = temp.path().join("runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        services,
    )
    .unwrap();

    let response = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_path".into(),
            r#type: "pick_directory".into(),
            caller_origin: Some("https://approved.example".into()),
            payload: Some(json!({})),
        },
        &runtime_path,
    )
    .unwrap();

    assert!(response.ok);
    assert_eq!(
        response.result,
        Some(json!({ "path": selected.to_string_lossy().to_string() }))
    );
    assert!(runtime
        .lock()
        .unwrap()
        .thread_manager
        .thread("thread_created")
        .is_err());
}

#[test]
fn tcp_picker_request_preserves_cancel_and_failure_and_requires_origin() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_path = temp.path().join("runtime.json");
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        Arc::new(FakePlatformServices { result: Ok(None) }),
    )
    .unwrap();

    let cancelled = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_cancel".into(),
            r#type: "pick_directory".into(),
            caller_origin: Some("https://approved.example".into()),
            payload: None,
        },
        &runtime_path,
    )
    .unwrap();
    assert!(cancelled.ok);
    assert_eq!(cancelled.result, Some(json!({ "path": null })));

    let missing_origin = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_unauthorized".into(),
            r#type: "pick_directory".into(),
            caller_origin: None,
            payload: None,
        },
        &runtime_path,
    )
    .unwrap();
    assert!(!missing_origin.ok);
    assert_eq!(
        missing_origin.error.unwrap().code,
        error_codes::IPC_UNAUTHORIZED
    );

    let failing_path = temp.path().join("failing-runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::new(Mutex::new(CoreRuntime::default())),
        &failing_path,
        Arc::new(FakePlatformServices {
            result: Err(PedelecError::new(
                error_codes::DIRECTORY_PICKER_FAILED,
                "dialog failed",
            )),
        }),
    )
    .unwrap();
    let failed = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_failed".into(),
            r#type: "pick_directory".into(),
            caller_origin: Some("https://approved.example".into()),
            payload: None,
        },
        &failing_path,
    )
    .unwrap();
    assert!(!failed.ok);
    assert_eq!(
        failed.error.unwrap().code,
        error_codes::DIRECTORY_PICKER_FAILED
    );
}
