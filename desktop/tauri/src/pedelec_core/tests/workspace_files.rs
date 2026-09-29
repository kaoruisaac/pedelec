use super::*;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

fn workspace_runtime(path: &Path, workspace_id: &str) -> CoreRuntime {
    fs::create_dir_all(path).unwrap();
    let mut runtime = CoreRuntime::new();
    runtime
        .register_workspace_for_test(
            workspace_id,
            path.canonicalize().unwrap(),
            WorkspaceKind::Custom,
        )
        .unwrap();
    runtime.set_asset_upload_port(9);
    runtime
}

fn insert_thread(
    runtime: &mut CoreRuntime,
    thread_id: &str,
    workspace_id: &str,
    status: ThreadStatus,
) {
    runtime.thread_manager.insert_thread(
        ThreadState {
            thread_id: thread_id.into(),
            workspace_id: workspace_id.into(),
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            effort_args: vec![],
            skills: vec![],
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            sdk_origin: None,
        },
        ProviderSessionState {
            provider_session_id: None,
            active_provider_turn_id: None,
        },
    );
}

fn upload_input(
    workspace_id: &str,
    filename: &str,
    target: Option<&str>,
    size: u64,
) -> CreateWorkspaceFileUploadInput {
    CreateWorkspaceFileUploadInput {
        workspace_id: workspace_id.into(),
        target_path: target.map(str::to_string),
        filename: filename.into(),
        size_bytes: size,
        mime_type: "application/octet-stream".into(),
    }
}

#[test]
fn workspace_upload_without_target_uses_the_original_filename() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_photo");
    let output = runtime
        .create_workspace_file_upload(upload_input("ws_photo", "photo.png", None, 4))
        .unwrap();
    let ticket = runtime.file_upload_tickets.get(&output.upload_id).unwrap();
    assert_eq!(ticket.response_path, "photo.png");
    assert_eq!(ticket.relative_path, PathBuf::from("photo.png"));
    assert!(!ticket.response_path.contains("upl_"));
    assert!(matches!(ticket.owner, FileTransferOwner::Workspace { .. }));
}

#[test]
fn workspace_nested_upload_round_trips_the_target_path() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_nested");
    let output = runtime
        .create_workspace_file_upload(upload_input(
            "ws_nested",
            "ignored.png",
            Some("references/photo.png"),
            4,
        ))
        .unwrap();
    let ticket = runtime.file_upload_tickets.get(&output.upload_id).unwrap();
    assert_eq!(ticket.response_path, "references/photo.png");
    assert_eq!(
        ticket.relative_path,
        PathBuf::from("references").join("photo.png")
    );
}

#[test]
fn workspace_uploads_to_different_paths_can_be_pending_together() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_parallel");
    let first = runtime
        .create_workspace_file_upload(upload_input("ws_parallel", "a.bin", Some("a.bin"), 1))
        .unwrap();
    let second = runtime
        .create_workspace_file_upload(upload_input("ws_parallel", "b.bin", Some("b.bin"), 1))
        .unwrap();
    assert_ne!(first.upload_id, second.upload_id);
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&first.upload_id)
            .unwrap()
            .state,
        FileUploadState::Pending
    );
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&second.upload_id)
            .unwrap()
            .state,
        FileUploadState::Pending
    );
}

#[test]
fn workspace_transfer_does_not_require_a_thread_and_survives_thread_end() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_life");
    insert_thread(&mut runtime, "thread_ended", "ws_life", ThreadStatus::Ended);
    let output = runtime
        .create_workspace_file_upload(upload_input("ws_life", "hello.txt", None, 5))
        .unwrap();
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&output.upload_id)
            .unwrap()
            .response_path,
        "hello.txt"
    );
    let asset_error = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_ended".into(),
            target_path: None,
            filename: "hello.txt".into(),
            size_bytes: 5,
            mime_type: "text/plain".into(),
        })
        .unwrap_err();
    assert_eq!(asset_error.code, error_codes::THREAD_ENDED);
}

#[test]
fn ending_a_thread_does_not_invalidate_an_independent_workspace_upload() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_invalidate");
    insert_thread(
        &mut runtime,
        "thread_live",
        "ws_invalidate",
        ThreadStatus::Idle,
    );
    let workspace_ticket = runtime
        .create_workspace_file_upload(upload_input("ws_invalidate", "keep.txt", None, 1))
        .unwrap();
    let asset_ticket = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_live".into(),
            target_path: Some("/report.txt".into()),
            filename: "report.txt".into(),
            size_bytes: 1,
            mime_type: "text/plain".into(),
        })
        .unwrap();
    runtime.invalidate_asset_uploads_for_thread("thread_live");
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&workspace_ticket.upload_id)
            .unwrap()
            .state,
        FileUploadState::Pending
    );
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&asset_ticket.upload_id)
            .unwrap()
            .state,
        FileUploadState::Failed
    );
}

#[test]
fn asset_upload_targets_the_assets_root_with_a_public_asset_path() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_asset");
    insert_thread(&mut runtime, "thread_asset", "ws_asset", ThreadStatus::Idle);
    let output = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_asset".into(),
            target_path: Some("/nested/report.json".into()),
            filename: "report.json".into(),
            size_bytes: 2,
            mime_type: "application/json".into(),
        })
        .unwrap();
    let ticket = runtime.file_upload_tickets.get(&output.upload_id).unwrap();
    assert_eq!(ticket.response_path, "/nested/report.json");
    assert_eq!(
        ticket.relative_path,
        PathBuf::from(".pedelec-runtime")
            .join("assets")
            .join("nested")
            .join("report.json")
    );
    let busy = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_asset".into(),
            target_path: None,
            filename: "photo.png".into(),
            size_bytes: 2,
            mime_type: "image/png".into(),
        })
        .unwrap_err();
    assert_eq!(busy.code, error_codes::THREAD_BUSY);

    let mut generated_runtime = workspace_runtime(&temp.path().join("generated"), "ws_generated");
    insert_thread(
        &mut generated_runtime,
        "thread_generated",
        "ws_generated",
        ThreadStatus::Idle,
    );
    let generated = generated_runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_generated".into(),
            target_path: None,
            filename: "photo.png".into(),
            size_bytes: 2,
            mime_type: "image/png".into(),
        })
        .unwrap();
    let generated_ticket = generated_runtime
        .file_upload_tickets
        .get(&generated.upload_id)
        .unwrap();
    assert!(generated_ticket.response_path.starts_with("/upl_"));
    assert!(generated_ticket.response_path.ends_with("-photo.png"));
    assert!(generated_ticket
        .relative_path
        .starts_with(Path::new(".pedelec-runtime").join("assets")));
}

#[test]
fn workspace_read_accepts_project_files_and_runtime_assets() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_read");
    let canonical = workspace.canonicalize().unwrap();
    fs::write(canonical.join("README.md"), "hello").unwrap();
    let readme = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_read".into(),
            path: "README.md".into(),
        })
        .unwrap();
    assert_eq!(readme.path, "README.md");
    assert_eq!(readme.name, "README.md");
    assert_eq!(readme.size_bytes, 5);
    assert_eq!(readme.mime_type, "text/plain");

    let asset = workspace_assets_root(&canonical)
        .join("nested")
        .join("report.json");
    fs::create_dir_all(asset.parent().unwrap()).unwrap();
    fs::write(&asset, b"{\"ok\":true}").unwrap();
    let nested = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_read".into(),
            path: ".pedelec-runtime/assets/nested/report.json".into(),
        })
        .unwrap();
    assert_eq!(nested.path, ".pedelec-runtime/assets/nested/report.json");
    assert_eq!(nested.name, "report.json");
    assert_eq!(nested.mime_type, "application/json");
}

#[test]
fn workspace_paths_reject_absolute_traversal_escape_directory_and_oversize_files() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_reject");
    let canonical = workspace.canonicalize().unwrap();
    for path in [
        "../secret.txt",
        "/src/App.tsx",
        "C:/secret.txt",
        "foo/../../secret.txt",
    ] {
        let error = runtime
            .create_workspace_file_upload(upload_input("ws_reject", "secret.txt", Some(path), 1))
            .unwrap_err();
        assert_eq!(error.code, error_codes::WORKSPACE_PATH_INVALID, "{path}");
    }
    fs::create_dir_all(canonical.join("folder")).unwrap();
    let directory = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_reject".into(),
            path: "folder".into(),
        })
        .unwrap_err();
    assert_eq!(directory.code, error_codes::WORKSPACE_FILE_NOT_REGULAR);

    let missing = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_reject".into(),
            path: "missing.txt".into(),
        })
        .unwrap_err();
    assert_eq!(missing.code, error_codes::WORKSPACE_FILE_NOT_FOUND);

    let huge = runtime
        .create_workspace_file_upload(upload_input(
            "ws_reject",
            "big.bin",
            None,
            MAX_ASSET_UPLOAD_BYTES + 1,
        ))
        .unwrap_err();
    assert_eq!(huge.code, error_codes::WORKSPACE_FILE_TOO_LARGE);

    let large_path = canonical.join("large.bin");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&large_path)
        .unwrap();
    file.set_len(MAX_ASSET_UPLOAD_BYTES + 1).unwrap();
    let too_large = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_reject".into(),
            path: "large.bin".into(),
        })
        .unwrap_err();
    assert_eq!(too_large.code, error_codes::WORKSPACE_FILE_READ_TOO_LARGE);
}

#[test]
fn workspace_transfer_rejects_symlink_targets_and_parents() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_link");
    let canonical = workspace.canonicalize().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"secret").unwrap();
    let link = canonical.join("linked.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.txt"), &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(outside.join("secret.txt"), &link).is_err() {
        return;
    }
    let target = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_link".into(),
            path: "linked.txt".into(),
        })
        .unwrap_err();
    assert_eq!(target.code, error_codes::WORKSPACE_FILE_NOT_REGULAR);

    let parent = canonical.join("linked-dir");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&outside, &parent).is_err() {
        return;
    }
    let parent_error = runtime
        .create_workspace_file_upload(upload_input(
            "ws_link",
            "secret.txt",
            Some("linked-dir/secret.txt"),
            6,
        ))
        .unwrap_err();
    assert_eq!(parent_error.code, error_codes::WORKSPACE_PATH_INVALID);
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");
}

#[test]
fn existing_regular_file_can_be_replaced_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    let relative = Path::new("nested").join("image.png");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join(&relative), b"old").unwrap();
    let staged = temp.path().join("staged");
    fs::write(&staged, b"new").unwrap();
    finalize_workspace_file_write(&staged, &root, &relative, true, "replace").unwrap();
    assert_eq!(fs::read(root.join(&relative)).unwrap(), b"new");
    assert!(!staged.exists());
}

#[test]
fn finalizer_does_not_recreate_a_removed_authoritative_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    fs::remove_dir_all(&root).unwrap();
    let staged = temp.path().join("staged");
    fs::write(&staged, b"data").unwrap();
    assert!(
        finalize_workspace_file_write(&staged, &root, Path::new("hello.txt"), true, "gone")
            .is_err()
    );
    assert!(!root.exists());
    assert!(staged.exists());
}

#[test]
fn finalizer_does_not_write_through_a_symlink_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&outside, &root).is_err() {
        return;
    }
    let staged = temp.path().join("staged");
    fs::write(&staged, b"data").unwrap();
    assert!(
        finalize_workspace_file_write(&staged, &root, Path::new("hello.txt"), true, "link")
            .is_err()
    );
    assert!(!outside.join("hello.txt").exists());
    assert!(staged.exists());
}

#[test]
fn admitted_uploading_ticket_commits_and_marks_completed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_commit_ok");
    let output = runtime
        .create_workspace_file_upload(upload_input("ws_commit_ok", "hello.txt", None, 5))
        .unwrap();
    let ticket = runtime
        .file_upload_tickets
        .get(&output.upload_id)
        .unwrap()
        .clone();
    runtime
        .file_upload_tickets
        .get_mut(&output.upload_id)
        .unwrap()
        .state = FileUploadState::Uploading;
    let staged = temp.path().join("staged");
    fs::write(&staged, b"hello").unwrap();
    runtime
        .commit_admitted_file_upload(
            &output.upload_id,
            &staged,
            &ticket.commit_root,
            &ticket.relative_path,
            ticket.audience,
        )
        .unwrap();
    assert_eq!(
        fs::read(ticket.commit_root.join(&ticket.relative_path)).unwrap(),
        b"hello"
    );
    assert!(!staged.exists());
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&output.upload_id)
            .unwrap()
            .state,
        FileUploadState::Completed
    );
}

#[test]
fn admitted_commit_does_not_recreate_a_removed_workspace_root() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_removed_root");
    let canonical = workspace.canonicalize().unwrap();
    let output = runtime
        .create_workspace_file_upload(upload_input("ws_removed_root", "hello.txt", None, 5))
        .unwrap();
    let ticket = runtime
        .file_upload_tickets
        .get(&output.upload_id)
        .unwrap()
        .clone();
    runtime
        .file_upload_tickets
        .get_mut(&output.upload_id)
        .unwrap()
        .state = FileUploadState::Uploading;
    fs::remove_dir_all(&canonical).unwrap();
    let staged = temp.path().join("staged");
    fs::write(&staged, b"hello").unwrap();
    assert!(runtime
        .commit_admitted_file_upload(
            &output.upload_id,
            &staged,
            &ticket.commit_root,
            &ticket.relative_path,
            ticket.audience,
        )
        .is_err());
    assert!(!canonical.exists());
    assert!(!canonical.join("hello.txt").exists());
    assert!(staged.exists());
    assert_ne!(
        runtime
            .file_upload_tickets
            .get(&output.upload_id)
            .unwrap()
            .state,
        FileUploadState::Completed
    );
}

#[test]
fn invalidated_uploading_ticket_is_not_admitted_for_commit() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_asset_commit");
    insert_thread(
        &mut runtime,
        "thread_live",
        "ws_asset_commit",
        ThreadStatus::Idle,
    );
    let output = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_live".into(),
            target_path: Some("/secret.txt".into()),
            filename: "secret.txt".into(),
            size_bytes: 4,
            mime_type: "text/plain".into(),
        })
        .unwrap();
    let ticket = runtime
        .file_upload_tickets
        .get(&output.upload_id)
        .unwrap()
        .clone();
    runtime
        .file_upload_tickets
        .get_mut(&output.upload_id)
        .unwrap()
        .state = FileUploadState::Uploading;
    let staged = temp.path().join("staged");
    fs::write(&staged, b"nope").unwrap();
    let destination = ticket.commit_root.join(&ticket.relative_path);

    assert!(runtime
        .commit_admitted_file_upload(
            &output.upload_id,
            &staged,
            &ticket.commit_root,
            Path::new("other.txt"),
            ticket.audience,
        )
        .is_err());
    assert!(!destination.exists());
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&output.upload_id)
            .unwrap()
            .state,
        FileUploadState::Uploading
    );

    runtime.invalidate_asset_uploads_for_thread("thread_live");
    assert!(runtime
        .commit_admitted_file_upload(
            &output.upload_id,
            &staged,
            &ticket.commit_root,
            &ticket.relative_path,
            ticket.audience,
        )
        .is_err());
    assert!(!destination.exists());
    assert!(staged.exists());
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&output.upload_id)
            .unwrap()
            .state,
        FileUploadState::Failed
    );
}

#[test]
fn removing_a_workspace_resource_clears_only_that_workspaces_file_tickets() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_a = temp.path().join("a");
    let workspace_b = temp.path().join("b");
    fs::create_dir_all(&workspace_b).unwrap();
    let mut runtime = workspace_runtime(&workspace_a, "ws_a");
    runtime
        .register_workspace_for_test(
            "ws_b",
            workspace_b.canonicalize().unwrap(),
            WorkspaceKind::Custom,
        )
        .unwrap();
    let canonical_a = workspace_a.canonicalize().unwrap();
    let canonical_b = workspace_b.canonicalize().unwrap();
    fs::write(canonical_a.join("a.txt"), b"a").unwrap();
    fs::write(canonical_b.join("b.txt"), b"b").unwrap();
    insert_thread(&mut runtime, "thread_asset", "ws_a", ThreadStatus::Idle);

    let upload_a = runtime
        .create_workspace_file_upload(upload_input("ws_a", "incoming.txt", None, 1))
        .unwrap();
    let download_a = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_a".into(),
            path: "a.txt".into(),
        })
        .unwrap();
    let upload_b = runtime
        .create_workspace_file_upload(upload_input("ws_b", "keep.txt", None, 1))
        .unwrap();
    let download_b = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_b".into(),
            path: "b.txt".into(),
        })
        .unwrap();
    let asset = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_asset".into(),
            target_path: Some("/report.txt".into()),
            filename: "report.txt".into(),
            size_bytes: 1,
            mime_type: "text/plain".into(),
        })
        .unwrap();
    let upload_a_ticket = runtime
        .file_upload_tickets
        .get(&upload_a.upload_id)
        .unwrap()
        .clone();
    runtime
        .file_upload_tickets
        .get_mut(&upload_a.upload_id)
        .unwrap()
        .state = FileUploadState::Uploading;
    runtime
        .file_download_tickets
        .get_mut(&download_a.download_id)
        .unwrap()
        .state = FileDownloadState::Downloading;

    runtime.remove_workspace_resource("ws_a");

    assert!(runtime
        .file_upload_tickets
        .get(&upload_a.upload_id)
        .is_none());
    assert!(runtime
        .file_download_tickets
        .get(&download_a.download_id)
        .is_none());
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&upload_b.upload_id)
            .unwrap()
            .state,
        FileUploadState::Pending
    );
    assert_eq!(
        runtime
            .file_download_tickets
            .get(&download_b.download_id)
            .unwrap()
            .state,
        FileDownloadState::Pending
    );
    assert_eq!(
        runtime
            .file_upload_tickets
            .get(&asset.upload_id)
            .unwrap()
            .state,
        FileUploadState::Pending
    );

    let staged = temp.path().join("staged");
    fs::write(&staged, b"x").unwrap();
    assert!(runtime
        .commit_admitted_file_upload(
            &upload_a.upload_id,
            &staged,
            &upload_a_ticket.commit_root,
            &upload_a_ticket.relative_path,
            upload_a_ticket.audience,
        )
        .is_err());
    assert!(!canonical_a.join("incoming.txt").exists());
    assert!(staged.exists());
}

#[test]
fn app_workspace_cleanup_drops_workspace_file_tickets_and_keeps_thread_tickets() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let mut runtime = workspace_runtime(&workspace, "ws_cleanup");
    runtime.workspace_manager = WorkspaceManager::with_workspace_root(temp.path().join("managed"));
    fs::create_dir_all(temp.path().join("managed")).unwrap();
    let canonical = workspace.canonicalize().unwrap();
    fs::write(canonical.join("keep.txt"), b"keep").unwrap();
    insert_thread(
        &mut runtime,
        "thread_cleanup",
        "ws_cleanup",
        ThreadStatus::Idle,
    );
    let upload = runtime
        .create_workspace_file_upload(upload_input("ws_cleanup", "incoming.txt", None, 1))
        .unwrap();
    let download = runtime
        .create_workspace_file_download(CreateWorkspaceFileDownloadInput {
            workspace_id: "ws_cleanup".into(),
            path: "keep.txt".into(),
        })
        .unwrap();
    let asset = runtime
        .create_asset_upload(CreateAssetUploadInput {
            thread_id: "thread_cleanup".into(),
            target_path: Some("/report.txt".into()),
            filename: "report.txt".into(),
            size_bytes: 1,
            mime_type: "text/plain".into(),
        })
        .unwrap();

    assert!(runtime.cleanup_stale_workspaces_for_app_start().is_empty());
    assert!(runtime.file_upload_tickets.get(&upload.upload_id).is_none());
    assert!(runtime
        .file_download_tickets
        .get(&download.download_id)
        .is_none());
    assert!(runtime.file_upload_tickets.contains_key(&asset.upload_id));
}
