use pedelec_lib::pedelec_core::SandboxManager;

#[test]
fn sandbox_manager_public_contract_creates_and_removes_a_thread_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let manager = SandboxManager::with_sandbox_root(temp.path());

    let path = manager
        .create_thread_sandbox("thread_public_contract")
        .unwrap();
    assert!(path.is_dir());
    assert!(manager
        .thread_sandbox_exists("thread_public_contract")
        .unwrap());

    manager.remove_thread_sandbox(&path).unwrap();
    assert!(!manager
        .thread_sandbox_exists("thread_public_contract")
        .unwrap());
}
