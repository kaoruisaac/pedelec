use pedelec_core::WorkspaceManager;

#[test]
fn workspace_manager_public_contract_creates_and_removes_a_thread_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let manager = WorkspaceManager::with_workspace_root(temp.path());

    let path = manager
        .create_thread_workspace("thread_public_contract")
        .unwrap();
    assert!(path.is_dir());
    assert!(manager
        .thread_workspace_exists("thread_public_contract")
        .unwrap());

    manager.remove_thread_workspace(&path).unwrap();
    assert!(!manager
        .thread_workspace_exists("thread_public_contract")
        .unwrap());
}
