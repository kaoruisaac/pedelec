use super::*;

const ORIGIN: &str = "https://runtime.example.test";

fn input(full: bool) -> CreateThreadInput {
    serde_json::from_value(serde_json::json!({
        "provider": "codex", "model": "test-model", "runtimeFullAccess": full,
    }))
    .unwrap()
}

fn runtime(temp: &tempfile::TempDir) -> CoreRuntime {
    CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("workspaces")),
        settings_file_path: Some(temp.path().join("settings.json")),
        ..CoreRuntime::default()
    }
}

fn active_intent(runtime: &mut CoreRuntime, id: &str) -> DenoExecutionIntent {
    runtime.thread_manager.thread_mut(id).unwrap().status = ThreadStatus::Running;
    runtime
        .thread_manager
        .provider_state_mut(id)
        .unwrap()
        .active_provider_turn_id = Some("active-turn".into());
    runtime
        .prepare_deno_run_intent(DenoRunInput {
            thread_id: id.into(),
            target: DenoRunTarget::StdinSource {
                source: "console.log('ok')".into(),
            },
            args: vec!["--allow-all".into()],
        })
        .unwrap()
}

#[test]
fn legacy_calls_stay_sandbox_and_cannot_self_authorize() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = runtime(&temp);
    let legacy: CreateThreadInput = serde_json::from_value(serde_json::json!({
        "provider": "codex", "model": "test-model",
    }))
    .unwrap();
    assert!(!legacy.runtime_full_access);
    let output = runtime.create_sdk_thread(legacy, ORIGIN, None).unwrap();
    assert!(!output.runtime_full_access_applied);
    assert!(!active_intent(&mut runtime, &output.thread_id).runtime_full_access);
    let supplied_mode: DenoRunInput = serde_json::from_value(serde_json::json!({
        "threadId": output.thread_id, "target": { "kind": "stdinSource", "source": "0" },
        "runtimeFullAccess": true, "runtimeAuthorization": true,
    }))
    .unwrap();
    assert!(
        !runtime
            .prepare_deno_run_intent(supplied_mode)
            .unwrap()
            .runtime_full_access
    );
    assert!(runtime.create_thread(input(true)).is_err());
    assert!(runtime
        .create_sdk_thread(input(true), ORIGIN, None)
        .is_err());
}

#[test]
fn mixed_threads_and_workspace_runs_keep_their_own_modes_on_resume() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = runtime(&temp);
    let authorization = runtime.grant_extension_runtime_full_access(ORIGIN).unwrap();
    let full = runtime
        .create_trusted_extension_thread(input(true), &authorization)
        .unwrap();
    assert!(full.runtime_full_access_applied);
    let mut sandbox_input = input(false);
    sandbox_input.workspace_id = Some(full.workspace_id.clone());
    let sandbox = runtime
        .create_sdk_thread(sandbox_input, ORIGIN, None)
        .unwrap();
    assert!(!sandbox.runtime_full_access_applied);
    assert!(active_intent(&mut runtime, &full.thread_id).runtime_full_access);
    assert!(!active_intent(&mut runtime, &sandbox.thread_id).runtime_full_access);
    for (id, mode) in [(&full.thread_id, true), (&sandbox.thread_id, false)] {
        runtime.thread_manager.thread_mut(id).unwrap().status = ThreadStatus::Ended;
        let resumed = runtime
            .resume_thread(ResumeThreadInput {
                thread_id: id.clone(),
            })
            .unwrap();
        assert_eq!(resumed.snapshot.runtime_full_access, mode);
        assert_eq!(
            runtime
                .build_thread_snapshot(id)
                .unwrap()
                .runtime_full_access,
            mode
        );
    }
    let run = runtime
        .begin_workspace_run(WorkspaceRunInput {
            workspace_id: full.workspace_id,
            script: "console.log('workspace')".into(),
            timeout_ms: None,
            deno_modules: vec![],
        })
        .unwrap();
    assert!(!run.intent.runtime_full_access);
    assert!(run.intent.runtime_authorization.is_none());
}

#[test]
fn revocation_invalidates_prepared_intents_all_origin_threads_and_old_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = runtime(&temp);
    let authorization = runtime.grant_extension_runtime_full_access(ORIGIN).unwrap();
    let other_authorization = runtime
        .grant_extension_runtime_full_access("https://other.example.test")
        .unwrap();
    let first = runtime
        .create_trusted_extension_thread(input(true), &authorization)
        .unwrap();
    let second = runtime
        .create_trusted_extension_thread(input(true), &authorization)
        .unwrap();
    let other = runtime
        .create_trusted_extension_thread(input(true), &other_authorization)
        .unwrap();
    let prepared = active_intent(&mut runtime, &first.thread_id);
    active_intent(&mut runtime, &second.thread_id);
    let unaffected = active_intent(&mut runtime, &other.thread_id);
    let revoked = runtime.revoke_runtime_full_access(ORIGIN).unwrap();
    assert_eq!(revoked.len(), 2);
    assert!(!prepared.runtime_authorization.as_ref().unwrap().is_active());
    assert!(unaffected
        .runtime_authorization
        .as_ref()
        .unwrap()
        .is_active());
    assert!(runtime
        .create_trusted_extension_thread(input(true), &authorization)
        .is_err());
    for id in [&first.thread_id, &second.thread_id] {
        assert!(runtime
            .prepare_deno_run_intent(DenoRunInput {
                thread_id: id.clone(),
                target: DenoRunTarget::StdinSource { source: "0".into() },
                args: vec![],
            })
            .is_err());
        runtime.thread_manager.thread_mut(id).unwrap().status = ThreadStatus::Ended;
        assert!(runtime
            .resume_thread(ResumeThreadInput {
                thread_id: id.clone()
            })
            .is_err());
        assert_eq!(runtime.thread_status(id), Some(ThreadStatus::Ended));
    }
    // Regrant issues a fresh generation; neither old sessions nor old threads revive.
    let fresh = runtime.grant_extension_runtime_full_access(ORIGIN).unwrap();
    assert!(runtime
        .create_trusted_extension_thread(input(true), &authorization)
        .is_err());
    assert!(runtime
        .create_trusted_extension_thread(input(true), &fresh)
        .is_ok());
    assert!(!prepared.runtime_authorization.as_ref().unwrap().is_active());
    // Serialization is never a way to transfer/forge a live capability.
    let restored: DenoExecutionIntent =
        serde_json::from_value(serde_json::to_value(&unaffected).unwrap()).unwrap();
    assert!(restored.runtime_full_access);
    assert!(restored.runtime_authorization.is_none());
    let restored_thread: ThreadState = serde_json::from_value(
        serde_json::to_value(runtime.thread_manager.thread(&other.thread_id).unwrap()).unwrap(),
    )
    .unwrap();
    assert!(restored_thread.runtime_full_access);
    assert!(restored_thread.runtime_authorization.is_none());
    runtime
        .thread_manager
        .thread_mut(&other.thread_id)
        .unwrap()
        .runtime_authorization = None;
    runtime
        .thread_manager
        .thread_mut(&other.thread_id)
        .unwrap()
        .status = ThreadStatus::Ended;
    assert!(runtime
        .resume_thread(ResumeThreadInput {
            thread_id: other.thread_id
        })
        .is_err());
}
