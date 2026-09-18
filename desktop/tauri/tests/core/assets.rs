use pedelec_core::WorkspaceManager;

#[test]
fn workspace_manager_public_contract_creates_and_removes_a_managed_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let manager = WorkspaceManager::with_workspace_root(temp.path());

    let path = manager
        .create_managed_workspace("thread_public_contract")
        .unwrap();
    assert!(path.is_dir());
    assert!(manager
        .managed_workspace_exists("thread_public_contract")
        .unwrap());

    manager.remove_managed_workspace(&path).unwrap();
    assert!(!manager
        .managed_workspace_exists("thread_public_contract")
        .unwrap());
}
