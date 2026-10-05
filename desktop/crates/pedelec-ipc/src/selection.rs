use super::*;
use pedelec_core::{ProviderCode, ProviderRuntimeSelection};

#[derive(Debug)]
pub(crate) struct SelectionRouter<D> {
    state: Mutex<RoutingState<D>>,
}

#[derive(Debug)]
struct RoutingState<D> {
    bindings: HashMap<String, String>,
    generations: HashMap<String, Arc<D>>,
    in_flight: HashMap<String, usize>,
    current_generations: HashMap<String, String>,
    next_generation: u64,
}

impl<D> Default for SelectionRouter<D> {
    fn default() -> Self {
        Self {
            state: Mutex::new(RoutingState {
                bindings: HashMap::new(),
                generations: HashMap::new(),
                in_flight: HashMap::new(),
                current_generations: HashMap::new(),
                next_generation: 0,
            }),
        }
    }
}

impl<D> SelectionRouter<D> {
    // Only routing bookkeeping is serialized. Owned dispatchers and operation
    // counts keep generations alive while provider I/O runs without this lock.
    pub fn dispatch(
        &self,
        core: &SharedCoreRuntime,
        owner: &ProviderRuntimeOwner,
        provider: ProviderCode,
        operation: PersistentRuntimeOperation,
        create: impl FnOnce(ProviderRuntimeSelection, String) -> D,
        healthy: impl Fn(&D) -> bool,
        dispatch: impl FnOnce(&D, PersistentRuntimeOperation) -> Result<(), PedelecError>,
    ) -> Result<(), PedelecError> {
        let thread_id = operation.thread_id().to_owned();
        let ending = matches!(operation, PersistentRuntimeOperation::EndSession { .. });
        let current = core
            .lock()
            .map_err(|_| {
                PedelecError::new(
                    error_codes::CORE_RUNTIME_UNAVAILABLE,
                    "Core runtime mutex poisoned",
                )
            })?
            .provider_runtime_selection(&provider);
        let mut state = self.state.lock().map_err(|_| {
            PedelecError::new(
                error_codes::CORE_RUNTIME_UNAVAILABLE,
                "provider selection router mutex poisoned",
            )
        })?;
        let newly_bound = !state.bindings.contains_key(&thread_id);
        let key = if let Some(key) = state.bindings.get(&thread_id) {
            key.clone()
        } else if ending {
            return match operation {
                PersistentRuntimeOperation::EndSession { session }
                    if session.active_provider_turn_id.is_some() =>
                {
                    Err(PedelecError::with_details(
                        error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                        "active provider turn has no bound runtime generation",
                        serde_json::json!({"provider": provider, "threadId": thread_id, "operation": "end"}),
                    ))
                }
                _ => Ok(()),
            };
        } else {
            let selection = current.clone()?;
            let selection_key = selection.runtime_key();
            let existing = state
                .current_generations
                .get(&selection_key)
                .filter(|key| {
                    state
                        .generations
                        .get(*key)
                        .is_some_and(|dispatcher| healthy(dispatcher))
                })
                .cloned();
            let key = if let Some(key) = existing {
                key
            } else {
                state.next_generation += 1;
                let key = format!("{}:{}", selection_key, state.next_generation);
                state
                    .generations
                    .insert(key.clone(), Arc::new(create(selection, key.clone())));
                state.current_generations.insert(selection_key, key.clone());
                key
            };
            state.bindings.insert(thread_id.clone(), key.clone());
            key
        };
        let dispatcher = Arc::clone(state.generations.get(&key).expect("bound runtime exists"));
        *state.in_flight.entry(key.clone()).or_default() += 1;
        drop(state);
        let result = dispatch(&dispatcher, operation);
        // Re-read after setup/teardown: a scan may have completed while the
        // provider RPC was pending. Failed admissions must not leak that now
        // obsolete zero-user generation.
        let latest = core
            .lock()
            .map_err(|_| {
                PedelecError::new(
                    error_codes::CORE_RUNTIME_UNAVAILABLE,
                    "Core runtime mutex poisoned",
                )
            })
            .map(|core| core.provider_runtime_selection(&provider));
        let mut state = self.state.lock().map_err(|_| {
            PedelecError::new(
                error_codes::CORE_RUNTIME_UNAVAILABLE,
                "provider selection router mutex poisoned",
            )
        })?;
        let active = state.in_flight.get_mut(&key).expect("operation is tracked");
        *active -= 1;
        if *active == 0 {
            state.in_flight.remove(&key);
        }
        if (ending || (newly_bound && result.is_err()))
            && state.bindings.get(&thread_id) == Some(&key)
        {
            state.bindings.remove(&thread_id);
        }
        let latest = latest?;
        // Without a usable selection, only bound generations should stay alive.
        let current_key = latest.ok().and_then(|selection| {
            state
                .current_generations
                .get(&selection.runtime_key())
                .cloned()
        });
        let obsolete = state
            .generations
            .keys()
            .filter(|key| {
                Some(*key) != current_key.as_ref()
                    && !state.bindings.values().any(|bound| bound == *key)
                    && !state.in_flight.contains_key(*key)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut retired = Vec::new();
        for key in obsolete {
            let dispatcher = state.generations.remove(&key).expect("generation exists");
            state
                .current_generations
                .retain(|_, generation| generation != &key);
            retired.push((key, dispatcher));
        }
        drop(state);
        let mut retirement_result = Ok(());
        for (key, _dispatcher) in retired {
            if let Err(message) = owner.registry().retire(key) {
                retirement_result = Err(PedelecError::new(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    message,
                ));
            }
        }
        retirement_result?;
        result
    }

    pub fn for_each(&self, mut visit: impl FnMut(&D)) {
        let dispatchers = self
            .state
            .lock()
            .ok()
            .map(|state| state.generations.values().cloned().collect::<Vec<_>>());
        if let Some(dispatchers) = dispatchers {
            for dispatcher in dispatchers {
                visit(&dispatcher);
            }
        }
    }

    #[cfg(test)]
    pub fn state_for_test<R>(&self, thread_id: &str, read: impl FnOnce(&D) -> R) -> R {
        let state = self.state.lock().unwrap();
        read(&state.generations[&state.bindings[thread_id]])
    }

    #[cfg(test)]
    pub fn has_binding_for_test(&self, thread_id: &str) -> bool {
        self.state.lock().unwrap().bindings.contains_key(thread_id)
    }
}

pub(crate) fn fail_bound_runtime(
    core: &mut pedelec_core::CoreRuntime,
    members: &Mutex<HashSet<String>>,
    excluded: Option<&str>,
    error: PedelecError,
) {
    if let Ok(members) = members.lock() {
        for thread_id in members.iter().filter(|id| excluded != Some(id.as_str())) {
            core.fail_persistent_runtime_thread(thread_id, &error);
        }
    }
}

#[cfg(test)]
pub(crate) fn test_session(
    runtime: &SharedCoreRuntime,
    workspace: &Path,
    provider: ProviderCode,
    id: &str,
) -> pedelec_core::PersistentProviderSessionIntent {
    let mut core = runtime.lock().unwrap();
    core.set_core_ipc_runtime("127.0.0.1:1", workspace.join("runtime.json"));
    let workspace_id = "selection-workspace".to_string();
    if core.workspace(&workspace_id).is_err() {
        core.register_workspace_for_test(
            &workspace_id,
            workspace,
            pedelec_core::WorkspaceKind::Custom,
        )
        .unwrap();
    }
    core.thread_manager.insert_thread(
        pedelec_core::ThreadState {
            thread_id: id.into(),
            workspace_id,
            provider: provider.clone(),
            effort_level: None,
            effort_args: vec![],
            skills: vec![],
            status: pedelec_core::ThreadStatus::Idle,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            sdk_origin: None,
        },
        pedelec_core::ProviderSessionState {
            provider_session_id: (provider == ProviderCode::Codex).then(|| format!("native-{id}")),
            active_provider_turn_id: None,
        },
    );
    pedelec_core::PersistentProviderSessionIntent {
        thread_id: id.into(),
        provider: provider.clone(),
        provider_session_id: (provider == ProviderCode::Codex).then(|| format!("native-{id}")),
        workspace_path: workspace.into(),
        effort_level: None,
        model: None,
        cursor_settings: None,
        reasoning_effort: None,
        antigravity_reasoning_effort: None,
        claude_reasoning_effort: None,
        approval_policy: pedelec_core::PersistentApprovalPolicy::Never,
        sandbox_policy: pedelec_core::PersistentSandboxPolicy::ReadOnly,
        host_instructions: "test instructions".into(),
        config: HashMap::new(),
        core_ipc_runtime_file_path: workspace.join("runtime.json"),
        tools: vec![],
        guidance: None,
    }
}

#[cfg(test)]
pub(crate) fn end_operation(provider: ProviderCode, id: &str) -> PersistentRuntimeOperation {
    PersistentRuntimeOperation::EndSession {
        session: pedelec_core::PersistentProviderEndIntent {
            thread_id: id.into(),
            provider,
            provider_session_id: None,
            active_provider_turn_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    fn configured_core(workspace: &Path) -> (SharedCoreRuntime, PathBuf) {
        let path = workspace.join(if cfg!(windows) { "codex.cmd" } else { "codex" });
        std::fs::write(&path, "fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let core = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        core.lock().unwrap().set_provider_selection_for_test(
            ProviderCode::Codex,
            path.clone(),
            "0.147.0",
        );
        (core, path)
    }

    #[test]
    fn different_threads_dispatch_concurrently_with_and_without_refresh() {
        for refresh in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (core, path) = configured_core(temp.path());
            let a = test_session(&core, temp.path(), ProviderCode::Codex, "a");
            let b = test_session(&core, temp.path(), ProviderCode::Codex, "b");
            let router = Arc::new(SelectionRouter::<String>::default());
            let owner = ProviderRuntimeOwner::new();
            let stops = Arc::new(AtomicUsize::new(0));
            let (entered_a, waiting_a) = mpsc::channel();
            let (release_a, resume_a) = mpsc::channel();
            let (entered_b, waiting_b) = mpsc::channel();
            std::thread::scope(|scope| {
                let (router, core, owner, stops) = (&router, &core, &owner, &stops);
                let worker_a = scope.spawn(move || {
                    router.dispatch(
                        &core,
                        &owner,
                        ProviderCode::Codex,
                        PersistentRuntimeOperation::EnsureSession { session: a },
                        |_, key| key,
                        |_| true,
                        |key, _| {
                            owner
                                .get_or_init(key.clone(), || {
                                    Ok(Arc::new(Controller(stops.clone())))
                                })
                                .unwrap();
                            entered_a.send(key.clone()).unwrap();
                            resume_a.recv_timeout(Duration::from_secs(10)).unwrap();
                            Ok(())
                        },
                    )
                });
                let old_key = waiting_a.recv_timeout(Duration::from_secs(5)).unwrap();
                if refresh {
                    core.lock().unwrap().set_provider_selection_for_test(
                        ProviderCode::Codex,
                        path,
                        "0.160.0",
                    );
                }
                let worker_b = scope.spawn(move || {
                    router.dispatch(
                        &core,
                        &owner,
                        ProviderCode::Codex,
                        PersistentRuntimeOperation::EnsureSession { session: b },
                        |_, key| key,
                        |_| true,
                        |key, _| {
                            entered_b.send(key.clone()).unwrap();
                            Ok(())
                        },
                    )
                });
                // Always release A before asserting so a regression fails without
                // leaving a blocked worker behind.
                let b_entered = waiting_b.recv_timeout(Duration::from_secs(5));
                release_a.send(()).unwrap();
                worker_a.join().unwrap().unwrap();
                worker_b.join().unwrap().unwrap();
                let new_key = b_entered.expect("B must enter provider I/O while A is blocked");
                assert_eq!(old_key == new_key, !refresh);
                assert_eq!(router.state_for_test("a", Clone::clone), old_key);
                assert_eq!(router.state_for_test("b", Clone::clone), new_key);
                let state = router.state.lock().unwrap();
                assert_eq!(state.generations.len(), if refresh { 2 } else { 1 });
                assert!(state.in_flight.is_empty());
                assert_eq!(stops.load(Ordering::SeqCst), 0);
            });
            owner.shutdown();
        }
    }

    #[test]
    fn end_does_not_retire_generation_with_an_operation_in_flight() {
        let temp = tempfile::tempdir().unwrap();
        let (core, path) = configured_core(temp.path());
        let a = test_session(&core, temp.path(), ProviderCode::Codex, "a");
        let router = SelectionRouter::<String>::default();
        let owner = ProviderRuntimeOwner::new();
        let stops = Arc::new(AtomicUsize::new(0));
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        std::thread::scope(|scope| {
            let (router, core, owner, stops) = (&router, &core, &owner, &stops);
            let worker = scope.spawn(move || {
                router.dispatch(
                    &core,
                    &owner,
                    ProviderCode::Codex,
                    PersistentRuntimeOperation::EnsureSession { session: a },
                    |_, key| key,
                    |_| true,
                    |key, _| {
                        owner
                            .get_or_init(key.clone(), || Ok(Arc::new(Controller(stops.clone()))))
                            .unwrap();
                        entered.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    },
                )
            });
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            core.lock().unwrap().set_provider_selection_for_test(
                ProviderCode::Codex,
                path,
                "0.160.0",
            );
            router
                .dispatch(
                    &core,
                    &owner,
                    ProviderCode::Codex,
                    end_operation(ProviderCode::Codex, "a"),
                    |_, _| panic!("end must use the bound generation"),
                    |_| true,
                    |_, _| Ok(()),
                )
                .unwrap();
            assert!(!router.has_binding_for_test("a"));
            assert_eq!(router.state.lock().unwrap().generations.len(), 1);
            assert_eq!(stops.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            worker.join().unwrap().unwrap();
        });
        assert!(router.state.lock().unwrap().generations.is_empty());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        owner.shutdown();
    }

    #[derive(Debug)]
    struct Controller(Arc<AtomicUsize>);
    impl ProviderRuntimeController for Controller {
        fn shutdown(&self) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct BlockingShutdown {
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl ProviderRuntimeController for BlockingShutdown {
        fn shutdown(&self) -> Result<(), String> {
            self.entered.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| error.to_string())
        }
    }

    #[test]
    fn retirement_does_not_block_admission_or_resurrect_retiring_generation() {
        let temp = tempfile::tempdir().unwrap();
        let (core, path) = configured_core(temp.path());
        let a = test_session(&core, temp.path(), ProviderCode::Codex, "a");
        let b = test_session(&core, temp.path(), ProviderCode::Codex, "b");
        let router = SelectionRouter::<String>::default();
        let owner = ProviderRuntimeOwner::new();
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                PersistentRuntimeOperation::EnsureSession { session: a },
                |_, key| key,
                |_| true,
                |key, _| {
                    owner
                        .get_or_init(key.clone(), || {
                            Ok(Arc::new(BlockingShutdown {
                                entered,
                                resume: Mutex::new(resume),
                            }))
                        })
                        .unwrap();
                    Ok(())
                },
            )
            .unwrap();
        let old_key = router.state_for_test("a", Clone::clone);
        core.lock().unwrap().set_provider_selection_for_test(
            ProviderCode::Codex,
            path.clone(),
            "0.160.0",
        );
        let (entered_b, waiting_b) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker_end = scope.spawn(|| {
                router.dispatch(
                    &core,
                    &owner,
                    ProviderCode::Codex,
                    end_operation(ProviderCode::Codex, "a"),
                    |_, _| panic!("end uses the bound generation"),
                    |_| true,
                    |_, _| Ok(()),
                )
            });
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            // Restore exactly the old identity while its shutdown is blocked.
            core.lock().unwrap().set_provider_selection_for_test(
                ProviderCode::Codex,
                path,
                "0.147.0",
            );
            let worker_b = scope.spawn(|| {
                router.dispatch(
                    &core,
                    &owner,
                    ProviderCode::Codex,
                    PersistentRuntimeOperation::EnsureSession { session: b },
                    |_, key| key,
                    |_| true,
                    |key, _| {
                        entered_b.send(key.clone()).unwrap();
                        Ok(())
                    },
                )
            });
            let admitted = waiting_b.recv_timeout(Duration::from_secs(5));
            release.send(()).unwrap();
            worker_end.join().unwrap().unwrap();
            worker_b.join().unwrap().unwrap();
            let new_key = admitted.expect("admission must not wait for runtime shutdown");
            assert_ne!(old_key, new_key);
            assert_eq!(router.state_for_test("b", Clone::clone), new_key);
            assert!(!router.has_binding_for_test("a"));
            let state = router.state.lock().unwrap();
            assert_eq!(state.generations.len(), 1);
            assert!(!state.generations.contains_key(&old_key));
        });
        owner.shutdown();
    }

    #[test]
    fn unavailable_selection_keeps_bound_runtime_then_retires_it_before_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join(if cfg!(windows) { "codex.cmd" } else { "codex" });
        std::fs::write(&path, "fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let core = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        core.lock().unwrap().set_provider_selection_for_test(
            ProviderCode::Codex,
            path.clone(),
            "0.147.0",
        );
        let a = test_session(&core, temp.path(), ProviderCode::Codex, "a");
        let b = test_session(&core, temp.path(), ProviderCode::Codex, "b");
        let router = SelectionRouter::<String>::default();
        let owner = ProviderRuntimeOwner::new();
        let stops = Arc::new(AtomicUsize::new(0));
        let ensure = |session| PersistentRuntimeOperation::EnsureSession { session };
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                ensure(a.clone()),
                |_, key| key,
                |_| true,
                |key, _| {
                    owner
                        .get_or_init(key.clone(), || Ok(Arc::new(Controller(stops.clone()))))
                        .unwrap();
                    Ok(())
                },
            )
            .unwrap();
        let old_key = router.state_for_test("a", Clone::clone);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            core.lock()
                .unwrap()
                .provider_runtime_selection(&ProviderCode::Codex)
                .unwrap_err()
                .code,
            error_codes::PROVIDER_TERMINAL_UNAVAILABLE
        );
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                ensure(a),
                |_, _| panic!("bound session must keep its generation"),
                |_| true,
                |key, _| {
                    assert_eq!(key, &old_key);
                    Ok(())
                },
            )
            .unwrap();
        assert!(router.has_binding_for_test("a"));
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                end_operation(ProviderCode::Codex, "a"),
                |_, _| panic!("ending must use the bound generation"),
                |_| true,
                |key, _| {
                    assert_eq!(key, &old_key);
                    Ok(())
                },
            )
            .unwrap();
        assert!(!router.has_binding_for_test("a"));
        {
            let state = router.state.lock().unwrap();
            assert!(state.generations.is_empty());
            assert!(state.current_generations.is_empty());
        }
        // Retirement already shut down the controller, without owner shutdown.
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        let error = router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                ensure(b.clone()),
                |_, _| panic!("unavailable provider must fail admission"),
                |_| true,
                |_, _| panic!("unavailable provider must not dispatch a new session"),
            )
            .unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_TERMINAL_UNAVAILABLE);
        assert!(!router.has_binding_for_test("b"));
        assert!(router.state.lock().unwrap().generations.is_empty());

        std::fs::write(&path, "restored").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        core.lock()
            .unwrap()
            .set_provider_selection_for_test(ProviderCode::Codex, path, "0.160.0");
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                ensure(b),
                |selection, key| {
                    assert_eq!(selection.version, "0.160");
                    assert_ne!(key, old_key);
                    key
                },
                |_| true,
                |key, _| {
                    owner
                        .get_or_init(key.clone(), || Ok(Arc::new(Controller(stops.clone()))))
                        .unwrap();
                    Ok(())
                },
            )
            .unwrap();
        assert!(router.has_binding_for_test("b"));
        assert_eq!(router.state.lock().unwrap().generations.len(), 1);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        owner.shutdown();
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_admission_during_refresh_removes_binding_and_retires_obsolete_process() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join(if cfg!(windows) { "codex.cmd" } else { "codex" });
        std::fs::write(&path, "fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let core = Arc::new(Mutex::new(pedelec_core::CoreRuntime::new()));
        core.lock().unwrap().set_provider_selection_for_test(
            ProviderCode::Codex,
            path.clone(),
            "0.147.0",
        );
        let session = test_session(&core, temp.path(), ProviderCode::Codex, "failed");
        let router = SelectionRouter::<String>::default();
        let owner = ProviderRuntimeOwner::new();
        let stops = Arc::new(AtomicUsize::new(0));
        let result = router.dispatch(
            &core,
            &owner,
            ProviderCode::Codex,
            PersistentRuntimeOperation::EnsureSession {
                session: session.clone(),
            },
            |_, key| key,
            |_| true,
            |key, _| {
                owner
                    .get_or_init(key.clone(), || Ok(Arc::new(Controller(stops.clone()))))
                    .unwrap();
                core.lock().unwrap().set_provider_selection_for_test(
                    ProviderCode::Codex,
                    path.clone(),
                    "0.160.0",
                );
                Err(PedelecError::new(
                    error_codes::PROVIDER_PROTOCOL_ERROR,
                    "deterministic setup failure",
                ))
            },
        );
        assert!(result.is_err());
        assert!(!router.has_binding_for_test("failed"));
        assert!(router.state.lock().unwrap().generations.is_empty());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        router
            .dispatch(
                &core,
                &owner,
                ProviderCode::Codex,
                PersistentRuntimeOperation::EnsureSession { session },
                |selection, key| {
                    assert_eq!(selection.version, "0.160");
                    key
                },
                |_| true,
                |key, _| {
                    owner
                        .get_or_init(key.clone(), || Ok(Arc::new(Controller(stops.clone()))))
                        .unwrap();
                    Ok(())
                },
            )
            .unwrap();
        assert!(router.has_binding_for_test("failed"));
        owner.shutdown();
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }
}
