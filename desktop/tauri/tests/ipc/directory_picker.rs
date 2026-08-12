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
    std::fs::create_dir_all(&selected).unwrap();
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
            r#type: "pick_sandbox_folder".into(),
            caller_origin: Some("https://approved.example".into()),
            caller_sdk_version: None,
            payload: Some(json!({})),
        },
        &runtime_path,
    )
    .unwrap();

    assert!(response.ok);
    assert_eq!(
        response.result,
        Some(json!({
            "path": selected.to_string_lossy().to_string(),
            "isEmptyFolder": true,
            "hasSandboxConfig": false,
        }))
    );
    assert!(runtime
        .lock()
        .unwrap()
        .thread_manager
        .thread("thread_created")
        .is_err());
}

#[test]
fn ipc_picker_to_custom_session_to_picker_reports_the_created_marker() {
    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("application-workspace");
    std::fs::create_dir_all(&selected).unwrap();
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime {
        sandbox_manager: pedelec_core::SandboxManager::with_sandbox_root(
            temp.path().join("managed"),
        ),
        ..CoreRuntime::default()
    }));
    let runtime_path = temp.path().join("runtime.json");
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        Arc::new(FakePlatformServices {
            result: Ok(Some(selected.clone())),
        }),
    )
    .unwrap();

    let picker_request = |request_id: &str| CoreIpcRequest {
        request_id: request_id.into(),
        r#type: "pick_sandbox_folder".into(),
        caller_origin: Some("https://approved.example".into()),
        caller_sdk_version: None,
        payload: None,
    };
    let first = send_core_ipc_request_with_runtime_path(
        &picker_request("pick_before_create"),
        &runtime_path,
    )
    .unwrap();
    assert_eq!(
        first.result,
        Some(json!({
            "path": selected.to_string_lossy().to_string(),
            "isEmptyFolder": true,
            "hasSandboxConfig": false,
        }))
    );

    let created = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "create_custom".into(),
            r#type: "create_thread".into(),
            caller_origin: Some("https://Example.com:443".into()),
            caller_sdk_version: Some("mock-sdk-version".into()),
            payload: Some(json!({
                "provider": "codex",
                "skills": null,
                "sandbox": { "path": selected.to_string_lossy().to_string() },
            })),
        },
        &runtime_path,
    )
    .unwrap();
    assert!(created.ok);
    let marker = selected.join(".pedelec-sandbox.json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&marker).unwrap()).unwrap(),
        json!({
            "sdk-version": "mock-sdk-version",
            "origin": "https://example.com",
        })
    );

    let second = send_core_ipc_request_with_runtime_path(
        &picker_request("pick_after_create"),
        &runtime_path,
    )
    .unwrap();
    assert_eq!(
        second.result,
        Some(json!({
            "path": selected.to_string_lossy().to_string(),
            "isEmptyFolder": false,
            "hasSandboxConfig": true,
        }))
    );
}

#[test]
fn tcp_picker_request_reports_root_entries_and_regular_marker_only() {
    let temp = tempfile::tempdir().unwrap();
    let cases = [
        ("file", false, false),
        ("subdirectory", false, false),
        ("marker", false, true),
        ("invalid-marker", false, true),
        ("marker-directory", false, false),
        ("legacy-layout", false, false),
    ];

    for (name, expected_empty, expected_marker) in cases {
        let selected = temp.path().join(name);
        std::fs::create_dir_all(&selected).unwrap();
        match name {
            "file" => std::fs::write(selected.join("README.md"), "hello").unwrap(),
            "subdirectory" => std::fs::create_dir(selected.join("nested")).unwrap(),
            "marker" => std::fs::write(selected.join(".pedelec-sandbox.json"), "{}").unwrap(),
            "invalid-marker" => {
                std::fs::write(selected.join(".pedelec-sandbox.json"), "not json").unwrap()
            }
            "marker-directory" => {
                std::fs::create_dir(selected.join(".pedelec-sandbox.json")).unwrap()
            }
            "legacy-layout" => {
                for subdir in ["skills", "assets", "logs", "tmp"] {
                    std::fs::create_dir(selected.join(subdir)).unwrap();
                }
            }
            _ => unreachable!(),
        }

        let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join(format!("{name}.runtime.json"));
        start_core_ipc_server_with_runtime_path_and_services(
            Arc::clone(&runtime),
            &runtime_path,
            Arc::new(FakePlatformServices {
                result: Ok(Some(selected.clone())),
            }),
        )
        .unwrap();

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: format!("pick_{name}"),
                r#type: "pick_sandbox_folder".into(),
                caller_origin: Some("https://approved.example".into()),
                caller_sdk_version: None,
                payload: None,
            },
            &runtime_path,
        )
        .unwrap();

        assert!(response.ok);
        assert_eq!(
            response.result.as_ref().unwrap()["isEmptyFolder"],
            expected_empty
        );
        assert_eq!(
            response.result.as_ref().unwrap()["hasSandboxConfig"],
            expected_marker
        );
    }
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
            r#type: "pick_sandbox_folder".into(),
            caller_origin: Some("https://approved.example".into()),
            caller_sdk_version: None,
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
            r#type: "pick_sandbox_folder".into(),
            caller_origin: None,
            caller_sdk_version: None,
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
            r#type: "pick_sandbox_folder".into(),
            caller_origin: Some("https://approved.example".into()),
            caller_sdk_version: None,
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

#[test]
fn tcp_picker_request_rejects_inspection_io_failures_and_old_request_type() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_path = temp.path().join("runtime.json");
    let runtime: SharedCoreRuntime = Arc::new(Mutex::new(CoreRuntime::default()));
    start_core_ipc_server_with_runtime_path_and_services(
        Arc::clone(&runtime),
        &runtime_path,
        Arc::new(FakePlatformServices {
            result: Ok(Some(temp.path().join("deleted-folder"))),
        }),
    )
    .unwrap();

    let failed = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_inspection_failed".into(),
            r#type: "pick_sandbox_folder".into(),
            caller_origin: Some("https://approved.example".into()),
            caller_sdk_version: None,
            payload: None,
        },
        &runtime_path,
    )
    .unwrap();
    assert!(!failed.ok);
    assert_eq!(
        failed.error.unwrap().code,
        error_codes::DIRECTORY_PICKER_FAILED
    );

    let old = send_core_ipc_request_with_runtime_path(
        &CoreIpcRequest {
            request_id: "pick_old".into(),
            r#type: "pick_directory".into(),
            caller_origin: Some("https://approved.example".into()),
            caller_sdk_version: None,
            payload: None,
        },
        &runtime_path,
    )
    .unwrap();
    assert!(!old.ok);
}
