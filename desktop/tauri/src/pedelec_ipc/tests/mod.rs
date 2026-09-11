use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use pedelec_cli::run_tool_cli_with_runtime_file_path;
    use pedelec_core::{
        workspace_assets_root, workspace_logs_root, CommandSpec, CoreRuntime, CreateThreadOutput,
        CreateThreadSkillsInput, CreateThreadToolInput, EffortLevel, EndThreadInput,
        PedelecSettings, PendingProviderOperation, PendingProviderOperationKind,
        PersistentRuntimeOperation, ProviderCode, ProviderRuntimeEvent, ProviderSessionState,
        ThreadOperationKind, ThreadSnapshot, ThreadState, ThreadStatus, ThreadSubscription,
        ToolRegistry, WorkspaceManager,
    };
    use serde_json::{json, Value};
    use std::env;
    use std::io::BufReader;
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_prefers_exe_over_cmd() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("codex.exe");
        let cmd = bin_dir.join("codex.cmd");
        std::fs::write(&exe, b"fake-exe").unwrap();
        std::fs::write(&cmd, b"fake-cmd").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::Direct(exe));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_uses_cmd_shim_when_exe_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let cmd = bin_dir.join("codex.cmd");
        std::fs::write(&cmd, b"fake-cmd").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("Path".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::CmdScript(cmd));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_program_resolution_reports_lookup_candidates_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let err = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap_err();

        assert_eq!(err.error, "program was not found in PATH");
        assert_eq!(
            err.candidates,
            vec![
                bin_dir.join("codex.exe"),
                bin_dir.join("codex.cmd"),
                bin_dir.join("codex.bat")
            ]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_provider_program_resolution_uses_bare_program_from_path() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let program = bin_dir.join("codex");
        std::fs::write(&program, b"fake-bin").unwrap();

        let resolved = resolve_provider_program(
            "codex",
            &[("PATH".into(), bin_dir.to_string_lossy().into())],
        )
        .unwrap();

        assert_eq!(resolved, ResolvedProviderProgram::Direct(program));
    }

    #[test]
    fn provider_process_command_uses_direct_program_with_original_args() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp
            .path()
            .join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let spec = test_command_spec("codex", temp.path(), vec!["exec".into(), "-".into()]);

        let command = build_provider_process_command(
            &spec,
            &ResolvedProviderProgram::Direct(program.clone()),
        );

        assert_eq!(command.get_program(), program.as_os_str());
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["exec", "-"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn provider_process_command_wraps_cmd_script_and_preserves_args() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex.cmd");
        let spec = test_command_spec(
            "codex",
            temp.path(),
            vec!["exec".into(), "--json".into(), "-".into()],
        );

        let command = build_provider_process_command(
            &spec,
            &ResolvedProviderProgram::CmdScript(script.clone()),
        );

        assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![
                "/d".to_string(),
                "/c".to_string(),
                "call".to_string(),
                script.to_string_lossy().to_string(),
                "exec".to_string(),
                "--json".to_string(),
                "-".to_string()
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn provider_process_command_runs_cmd_script_via_call() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("echo_args.cmd");
        std::fs::write(&script, b"@echo off\r\necho %1 %2\r\n").unwrap();
        let spec = test_command_spec(
            "echo_args",
            temp.path(),
            vec!["hello".into(), "world".into()],
        );

        let output =
            build_provider_process_command(&spec, &ResolvedProviderProgram::CmdScript(script))
                .output()
                .unwrap();

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello world"
        );
    }

    #[test]
    fn runtime_file_is_written_with_loopback_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let handle =
            start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json"))
                .unwrap();

        assert_eq!(handle.runtime_file.protocol, CORE_IPC_PROTOCOL);
        assert_eq!(handle.runtime_file.host, CORE_IPC_HOST);
        assert!(handle.runtime_file.endpoint.starts_with("127.0.0.1:"));
        assert!(handle.runtime_file_path.exists());
    }

    #[test]
    fn missing_runtime_file_maps_to_core_runtime_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let err = connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap_err();

        assert_eq!(err.code, error_codes::CORE_RUNTIME_UNAVAILABLE);
    }

    #[test]
    fn list_providers_core_ipc_returns_opencode_entry() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            ..CoreRuntime::default()
        }));
        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_ready_for_test();

        let response = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "providers".into(),
                r#type: "list_providers".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({})),
            },
            runtime,
        );

        assert!(response.ok);
        let providers = response.result.unwrap().as_array().unwrap().clone();
        assert!(providers.iter().any(|provider| {
            provider.get("code") == Some(&json!("opencode"))
                && provider.get("name") == Some(&json!("OpenCode"))
                && provider.get("available").is_some()
                && provider.get("isDefault") == Some(&json!(false))
        }));
    }

    #[test]
    fn list_providers_core_ipc_propagates_settings_read_errors() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        std::fs::write(&settings_path, "not-json").unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            settings_file_path: Some(settings_path),
            ..CoreRuntime::default()
        }));
        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_ready_for_test();

        let response = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "providers_settings_error".into(),
                r#type: "list_providers".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({})),
            },
            runtime,
        );

        assert!(!response.ok);
        assert_eq!(
            response.error.unwrap().code,
            error_codes::SETTINGS_READ_FAILED
        );
    }

    #[test]
    fn settings_core_ipc_returns_public_settings_and_rejects_updates() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(test_provider_path(temp.path(), "codex")),
            ..CoreRuntime::default()
        }));

        let initial = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "settings_get_initial".into(),
                r#type: "get_settings".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({})),
            },
            Arc::clone(&runtime),
        );
        assert!(initial.ok);
        assert_eq!(
            initial.result.unwrap(),
            json!({
                "defaultProvider": null
            })
        );

        let updated = handle_core_ipc_request(
            CoreIpcRequest {
                request_id: "settings_update".into(),
                r#type: "update_settings".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "defaultProvider": "codex",
                    "providerSettings": {
                        "codex": {
                            "effortsArgs": {
                                "default": ["--sandbox", "danger-full-access"],
                                "low": [],
                                "high": []
                            }
                        },
                        "ollama": {
                            "baseUrl": " http://127.0.0.1:11434/ ",
                            "timeoutMs": 120000,
                            "apiKey": "ollama"
                        }
                    }
                })),
            },
            Arc::clone(&runtime),
        );
        assert!(!updated.ok);
        assert_eq!(updated.error.unwrap().code, error_codes::IPC_UNAVAILABLE);

        assert_eq!(
            runtime.lock().unwrap().get_settings().unwrap(),
            PedelecSettings::default()
        );
    }

    #[test]
    fn request_id_is_echoed_for_unknown_request() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json")).unwrap();

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "req_1".into(),
                r#type: "missing".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: None,
            },
            temp.path().join("runtime.json"),
        )
        .unwrap();

        assert_eq!(response.request_id, "req_1");
        assert!(!response.ok);
    }

    #[test]
    fn oversized_message_returns_message_too_large() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(runtime, temp.path().join("runtime.json")).unwrap();
        let mut stream =
            connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap();
        let oversized = "x".repeat(MAX_CORE_IPC_MESSAGE_BYTES + 1);
        stream.write_all(oversized.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();

        let mut reader = BufReader::new(stream);
        let line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&line).unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::MESSAGE_TOO_LARGE);
    }

    #[test]
    fn subscribe_receives_later_thread_event() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(
            Arc::clone(&runtime),
            temp.path().join("runtime.json"),
        )
        .unwrap();

        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_sub",
            ThreadStatus::Idle,
            1000,
        );

        let mut stream =
            connect_core_ipc_with_runtime_path(temp.path().join("runtime.json")).unwrap();
        write_json_line(
            &mut stream,
            &CoreIpcRequest {
                request_id: "sub_1".into(),
                r#type: "subscribe_thread".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({ "threadId": "thread_sub" })),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let response_line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&response_line).unwrap();
        assert!(response.ok);
        assert_eq!(
            response.result.as_ref().unwrap()["snapshot"]["latestSeq"],
            json!(0)
        );

        runtime.lock().unwrap().event_bus.emit_operation_completed(
            "thread_sub",
            "test-operation",
            ThreadOperationKind::User,
            true,
            None,
        );
        let event_line = read_bounded_json_line(&mut reader).unwrap();
        let event: CoreIpcEventMessage = serde_json::from_slice(&event_line).unwrap();

        assert_eq!(event.r#type, "thread_event");
    }

    #[test]
    fn subscription_forwarder_filters_events_at_or_below_snapshot_cutoff() {
        let (event_tx, event_rx) = mpsc::channel();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(client);

        spawn_thread_subscription_forwarder(
            ThreadSubscription {
                events: event_rx,
                snapshot: ThreadSnapshot {
                    thread_id: "thread_cutoff".into(),
                    status: ThreadStatus::Idle,
                    latest_seq: 1,
                    usage: None,
                    active_operation: None,
                    last_completed_operation: None,
                    pending_tool_request: None,
                },
            },
            Arc::new(Mutex::new(server)),
        );
        reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        event_tx
            .send(ThreadEvent::StatusChanged {
                seq: 1,
                thread_id: "thread_cutoff".into(),
                operation_id: None,
                status: ThreadStatus::Idle,
            })
            .unwrap();
        assert!(read_bounded_json_line(&mut reader).is_err());

        event_tx
            .send(ThreadEvent::StatusChanged {
                seq: 2,
                thread_id: "thread_cutoff".into(),
                operation_id: Some("operation-b".into()),
                status: ThreadStatus::Running,
            })
            .unwrap();
        let forwarded = read_bounded_json_line(&mut reader).unwrap();
        let forwarded: CoreIpcEventMessage = serde_json::from_slice(&forwarded).unwrap();
        assert!(matches!(
            forwarded.event,
            ThreadEvent::StatusChanged {
                seq: 2,
                operation_id: Some(operation_id),
                ..
            } if operation_id == "operation-b"
        ));
    }

    #[test]
    fn send_text_rejects_busy_thread() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        start_core_ipc_server_with_runtime_path(
            Arc::clone(&runtime),
            temp.path().join("runtime.json"),
        )
        .unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_busy",
            ThreadStatus::Running,
            1000,
        );

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "send_1".into(),
                r#type: "send_text".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({ "threadId": "thread_busy", "message": "hello" })),
            },
            temp.path().join("runtime.json"),
        )
        .unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::THREAD_BUSY);
    }

    #[test]
    fn persistent_create_send_tool_result_complete_end_e2e() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_path = temp.path().join("runtime.json");
        let workspace_root = temp.path().join("workspace");
        let runtime = Arc::new(Mutex::new(CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        }));
        runtime
            .lock()
            .unwrap()
            .provider_readiness
            .mark_ready_for_test();
        let dispatcher = Arc::new(RecordingPersistentDispatcher::default());
        start_core_ipc_server_with_runtime_path_services_and_dispatcher(
            Arc::clone(&runtime),
            runtime_path.clone(),
            Arc::new(NoopCoreIpcPlatformServices),
            dispatcher.clone(),
        )
        .unwrap();

        let create = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "persistent_e2e_create".into(),
                r#type: "create_thread".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "provider": "codex",
                    "skills": phase09_skills_manifest()
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(create.ok);
        let output: CreateThreadOutput = serde_json::from_value(create.result.unwrap()).unwrap();
        let thread_id = output.thread_id.clone();
        assert_eq!(create.request_id, "persistent_e2e_create");
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Idle)
        );
        let workspace_path = runtime
            .lock()
            .unwrap()
            .thread_workspace_path(&thread_id)
            .unwrap();
        assert!(workspace_path.is_dir());
        assert_eq!(
            runtime.lock().unwrap().provider_session_state(&thread_id),
            Some(&ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            })
        );

        let mut subscription = subscribe_to_thread(&runtime_path, &thread_id);
        let send = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "persistent_e2e_send".into(),
                r#type: "send_text".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": thread_id,
                    "message": "call update_counter with delta 2"
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(send.ok);
        let send_result = send.result.unwrap();
        assert_eq!(send_result["threadId"], json!(thread_id));
        assert!(send_result["operationId"].as_str().is_some());

        let mut events = vec![read_thread_event(&mut subscription).event];
        assert!(matches!(
            events[0],
            ThreadEvent::StatusChanged {
                status: ThreadStatus::Running,
                ..
            }
        ));

        let start_turn = {
            let operations = dispatcher.operations.lock().unwrap();
            let Some(PersistentRuntimeOperation::StartTurn { turn }) = operations.first() else {
                panic!("send should dispatch a persistent start-turn operation");
            };
            assert_eq!(turn.thread_id, thread_id);
            assert_eq!(turn.session.thread_id, thread_id);
            assert_eq!(turn.message, "call update_counter with delta 2");
            assert_eq!(turn.session.provider, ProviderCode::Codex);
            turn.clone()
        };
        assert!(!start_turn.local_turn_id.is_empty());

        runtime
            .lock()
            .unwrap()
            .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                thread_id: thread_id.clone(),
                provider_session_id: "provider-session-e2e".into(),
            })
            .unwrap();
        events.push(read_thread_event(&mut subscription).event);
        assert!(matches!(
            events.last(),
            Some(ThreadEvent::ProviderSessionIdUpdated {
                provider_session_id,
                ..
            }) if provider_session_id == "provider-session-e2e"
        ));

        runtime
            .lock()
            .unwrap()
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnStarted {
                thread_id: thread_id.clone(),
                provider_turn_id: "provider-turn-e2e".into(),
            })
            .unwrap();
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .provider_session_state(&thread_id)
                .and_then(|state| state.active_provider_turn_id.as_deref()),
            Some("provider-turn-e2e")
        );

        runtime
            .lock()
            .unwrap()
            .reduce_provider_runtime_event(ProviderRuntimeEvent::AssistantMessage {
                thread_id: thread_id.clone(),
                provider_turn_id: Some("provider-turn-e2e".into()),
                text: "I will inspect the app state.".into(),
            })
            .unwrap();
        events.push(read_thread_event(&mut subscription).event);
        assert!(matches!(
            events.last(),
            Some(ThreadEvent::AssistantMessage { text, .. })
                if text == "I will inspect the app state."
        ));

        let first_tool_runtime_path = runtime_path.clone();
        let first_tool_thread_id = thread_id.clone();
        let first_tool_handle = thread::spawn(move || {
            run_tool_cli_with_runtime_file_path(
                vec![
                    "pedelec-cli".into(),
                    "--thread-id".into(),
                    first_tool_thread_id,
                    "tool-call".into(),
                    "update_counter".into(),
                    r#"{"delta":2}"#.into(),
                ],
                Some(&first_tool_runtime_path),
            )
        });

        let tool_events = collect_ipc_events_until(&mut subscription, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::ToolCall { .. }))
        });
        let first_request_id = tool_events
            .iter()
            .find_map(|event| match event {
                ThreadEvent::ToolCall { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .expect("tool call event should include a request id");
        assert!(tool_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::WaitingToolResult,
                    ..
                }
            )
        }));
        assert!(tool_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::ToolCall {
                    tool_name, args, ..
                } if tool_name == "update_counter" && args == &json!({ "delta": 2 })
            )
        }));
        events.extend(tool_events);

        let duplicate = run_tool_cli_with_runtime_file_path(
            vec![
                "pedelec-cli".into(),
                "--thread-id".into(),
                thread_id.clone(),
                "tool-call".into(),
                "get_app_state".into(),
                "{}".into(),
            ],
            Some(&runtime_path),
        );
        assert!(!duplicate.ok);
        assert_eq!(
            duplicate.error.unwrap().code,
            error_codes::PENDING_TOOL_REQUEST_EXISTS
        );

        submit_tool_result_over_ipc(
            &runtime_path,
            "persistent_e2e_submit",
            &thread_id,
            &first_request_id,
            json!({ "value": "persistent result" }),
        );
        let first_tool_response = first_tool_handle.join().unwrap();
        assert!(first_tool_response.ok);
        assert_eq!(
            first_tool_response.result,
            Some(json!({ "value": "persistent result" }))
        );

        let after_submit_events = collect_ipc_events_until(&mut subscription, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::ToolResult { .. }))
        });
        assert!(after_submit_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Running,
                    ..
                }
            )
        }));
        assert!(after_submit_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::ToolResult {
                    request_id,
                    result,
                    ..
                } if request_id == &first_request_id
                    && result == &json!({ "value": "persistent result" })
            )
        }));
        events.extend(after_submit_events);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Running)
        );

        runtime
            .lock()
            .unwrap()
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                thread_id: thread_id.clone(),
                provider_turn_id: Some("provider-turn-e2e".into()),
                success: true,
                error: None,
            })
            .unwrap();
        let completion_events = collect_ipc_events_until(&mut subscription, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::OperationCompleted { .. }))
        });
        assert!(completion_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Idle,
                    ..
                }
            )
        }));
        events.extend(completion_events);
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::OperationCompleted { .. })));
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            runtime.lock().unwrap().provider_session_state(&thread_id),
            Some(&ProviderSessionState {
                provider_session_id: Some("provider-session-e2e".into()),
                active_provider_turn_id: None,
            })
        );

        assert_thread_event_seq_is_strictly_increasing(&events);
        let running_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Running,
                        ..
                    }
                )
            })
            .unwrap();
        let tool_call_index = events
            .iter()
            .position(|event| matches!(event, ThreadEvent::ToolCall { .. }))
            .unwrap();
        let tool_result_index = events
            .iter()
            .position(|event| matches!(event, ThreadEvent::ToolResult { .. }))
            .unwrap();
        let done_index = events
            .iter()
            .position(|event| matches!(event, ThreadEvent::OperationCompleted { .. }))
            .unwrap();
        let idle_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Idle,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(running_index < tool_call_index);
        assert!(tool_call_index < tool_result_index);
        assert!(tool_result_index < done_index);
        assert!(tool_result_index < idle_index);
        assert!(idle_index < done_index);

        let sentinel_path = workspace_assets_root(&workspace_path).join("end-sentinel.txt");
        std::fs::write(&sentinel_path, "preserve me").unwrap();
        let end = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "persistent_e2e_end".into(),
                r#type: "end_thread".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({ "threadId": thread_id })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(end.ok);

        let end_events = collect_ipc_events_until(&mut subscription, |events| {
            events
                .iter()
                .any(|event| matches!(event, ThreadEvent::Ended { .. }))
        });
        assert!(end_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Stopping,
                    ..
                }
            )
        }));
        assert!(end_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Ended,
                    ..
                }
            )
        }));
        let stopping_index = end_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Stopping,
                        ..
                    }
                )
            })
            .unwrap();
        let ended_status_index = end_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Ended,
                        ..
                    }
                )
            })
            .unwrap();
        let ended_event_index = end_events
            .iter()
            .position(|event| matches!(event, ThreadEvent::Ended { .. }))
            .unwrap();
        assert!(stopping_index < ended_status_index);
        assert!(ended_status_index < ended_event_index);
        events.extend(end_events);
        assert_thread_event_seq_is_strictly_increasing(&events);
        assert_eq!(
            runtime.lock().unwrap().thread_status(&thread_id),
            Some(ThreadStatus::Ended)
        );
        assert!(workspace_path.is_dir());
        assert_eq!(
            std::fs::read_to_string(&sentinel_path).unwrap(),
            "preserve me"
        );

        let resume = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "persistent_e2e_resume".into(),
                r#type: "resume_thread".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({ "threadId": thread_id })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(resume.ok);
        assert_eq!(
            resume.result.as_ref().unwrap()["snapshot"]["status"],
            "idle"
        );

        let resume_events = collect_ipc_events_until(&mut subscription, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Idle,
                        ..
                    }
                )
            })
        });
        assert!(resume_events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::StatusChanged {
                    status: ThreadStatus::Idle,
                    ..
                }
            )
        }));
        events.extend(resume_events);
        assert_thread_event_seq_is_strictly_increasing(&events);

        let operations = dispatcher.operations.lock().unwrap();
        assert_eq!(operations.len(), 2);
        assert!(matches!(
            operations.first(),
            Some(PersistentRuntimeOperation::StartTurn { turn })
                if turn.thread_id == thread_id
                    && turn.message == "call update_counter with delta 2"
        ));
        assert!(matches!(
            operations.get(1),
            Some(PersistentRuntimeOperation::EndSession { session })
                if session.thread_id == thread_id
                    && session.provider == ProviderCode::Codex
                    && session.provider_session_id.as_deref() == Some("provider-session-e2e")
                    && session.active_provider_turn_id.is_none()
        ));
    }

    #[test]
    fn phase09_pedelec_tool_timeout_returns_fixed_json_shape_with_runtime_override() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_tool_timeout_cli",
            ThreadStatus::Running,
            20,
        );

        let response = run_tool_cli_with_runtime_file_path(
            vec![
                "pedelec-cli".into(),
                "--thread-id".into(),
                "thread_tool_timeout_cli".into(),
                "tool-call".into(),
                "get_app_state".into(),
                "{}".into(),
            ],
            Some(&runtime_path),
        );

        assert!(!response.ok);
        assert!(response.result.is_none());
        assert_eq!(response.error.unwrap().code, error_codes::TOOL_TIMEOUT);
    }

    #[test]
    fn phase09_demo_tools_fixture_matches_registry_contract() {
        let skills: CreateThreadSkillsInput =
            serde_json::from_value(phase09_skills_manifest()).unwrap();
        let registry = ToolRegistry::from_skills_input(Some(&skills)).unwrap();

        assert!(registry
            .validate_tool_call("get_app_state", &json!({}))
            .is_ok());
        assert!(registry
            .validate_tool_call("update_counter", &json!({ "delta": 1 }))
            .is_ok());
        assert_eq!(
            registry
                .validate_tool_call("update_counter", &json!({}))
                .unwrap_err()
                .code,
            error_codes::TOOL_ARGS_INVALID
        );
    }

    #[test]
    fn tool_call_success_resolves_after_submit_tool_result() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_tool",
            ThreadStatus::Running,
            1000,
        );
        let mut subscription = subscribe_to_thread(&runtime_path, "thread_tool");

        let tool_path = runtime_path.clone();
        let tool_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_1".into(),
                    r#type: "tool_call".into(),
                    caller_origin: None,
                    caller_sdk_version: None,
                    payload: Some(json!({
                        "threadId": "thread_tool",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                tool_path,
            )
            .unwrap()
        });

        let event = read_thread_event(&mut subscription);
        let request_id = match event.event {
            ThreadEvent::StatusChanged { .. } => match read_thread_event(&mut subscription).event {
                ThreadEvent::ToolCall { request_id, .. } => request_id,
                other => panic!("expected tool_call event, got {other:?}"),
            },
            ThreadEvent::ToolCall { request_id, .. } => request_id,
            other => panic!("expected status_changed/tool_call event, got {other:?}"),
        };

        let submit = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "submit_1".into(),
                r#type: "submit_tool_result".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_tool",
                    "requestId": request_id,
                    "result": { "value": 123 }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert!(submit.ok);

        let tool_response = tool_handle.join().unwrap();
        assert!(tool_response.ok);
        assert_eq!(tool_response.result.unwrap(), json!({ "value": 123 }));
        assert_replay_candidate_count_eventually(&runtime, 0);

        let second_path = runtime_path.clone();
        let second_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_2".into(),
                    r#type: "tool_call".into(),
                    caller_origin: None,
                    caller_sdk_version: None,
                    payload: Some(json!({
                        "threadId": "thread_tool",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                second_path,
            )
            .unwrap()
        });
        let second_request_id = loop {
            match read_thread_event(&mut subscription).event {
                ThreadEvent::ToolCall { request_id, .. } => break request_id,
                _ => continue,
            }
        };
        assert_ne!(second_request_id, request_id);
        send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "submit_2".into(),
                r#type: "submit_tool_result".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_tool",
                    "requestId": second_request_id,
                    "result": { "value": 456 }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(
            second_handle.join().unwrap().result,
            Some(json!({ "value": 456 }))
        );
    }

    #[test]
    fn ipc_failed_first_delivery_replays_completed_result_and_acknowledges_retry() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_ipc_replay_failure",
            ThreadStatus::Running,
            1000,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_ipc_replay_failure");

        force_tool_response_write_failure_for_test("ipc_original_failure");
        let first = spawn_tool_call(
            runtime_path.clone(),
            "ipc_original_failure",
            "thread_ipc_replay_failure",
            "get_app_state",
            json!({}),
        );
        let request_id = next_tool_call_request_id(&event_rx);
        submit_tool_result_over_ipc(
            &runtime_path,
            "ipc_submit_original_failure",
            "thread_ipc_replay_failure",
            &request_id,
            json!({ "value": 123 }),
        );

        assert!(first.join().unwrap().is_err());
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .replay_candidate_count(),
            1
        );

        let replay = send_core_ipc_request_with_runtime_path(
            &tool_call_request(
                "ipc_replay_success",
                "thread_ipc_replay_failure",
                "get_app_state",
                json!({}),
            ),
            &runtime_path,
        )
        .unwrap();
        assert_eq!(replay.result, Some(json!({ "value": 123 })));
        assert_replay_candidate_count_eventually(&runtime, 0);
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolCall { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn ipc_replay_delivery_failure_keeps_candidate_until_later_success() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_ipc_replay_retry",
            ThreadStatus::Running,
            1000,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_ipc_replay_retry");

        force_tool_response_write_failure_for_test("ipc_retry_original");
        let first = spawn_tool_call(
            runtime_path.clone(),
            "ipc_retry_original",
            "thread_ipc_replay_retry",
            "get_app_state",
            json!({}),
        );
        let request_id = next_tool_call_request_id(&event_rx);
        submit_tool_result_over_ipc(
            &runtime_path,
            "ipc_retry_submit_original",
            "thread_ipc_replay_retry",
            &request_id,
            json!({ "value": 456 }),
        );
        assert!(first.join().unwrap().is_err());

        force_tool_response_write_failure_for_test("ipc_retry_failed_replay");
        let failed_replay = send_core_ipc_request_with_runtime_path(
            &tool_call_request(
                "ipc_retry_failed_replay",
                "thread_ipc_replay_retry",
                "get_app_state",
                json!({}),
            ),
            &runtime_path,
        );
        assert!(failed_replay.is_err());
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .replay_candidate_count(),
            1
        );

        let successful_replay = send_core_ipc_request_with_runtime_path(
            &tool_call_request(
                "ipc_retry_successful_replay",
                "thread_ipc_replay_retry",
                "get_app_state",
                json!({}),
            ),
            &runtime_path,
        )
        .unwrap();
        assert_eq!(successful_replay.result, Some(json!({ "value": 456 })));
        assert_replay_candidate_count_eventually(&runtime, 0);
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolCall { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn ipc_multiple_waiters_clear_replay_after_any_successful_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_ipc_waiters",
            ThreadStatus::Running,
            1000,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_ipc_waiters");

        force_tool_response_write_failure_for_test("ipc_waiter_one");
        let first = spawn_tool_call(
            runtime_path.clone(),
            "ipc_waiter_one",
            "thread_ipc_waiters",
            "get_app_state",
            json!({}),
        );
        let request_id = next_tool_call_request_id(&event_rx);
        let second = spawn_tool_call(
            runtime_path.clone(),
            "ipc_waiter_two",
            "thread_ipc_waiters",
            "get_app_state",
            json!({}),
        );
        for _ in 0..1000 {
            if runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .waiter_count(&request_id)
                == Some(2)
            {
                break;
            }
            // Wait only for the second real IPC handler to register its
            // waiter; the response outcome itself is controlled by the
            // deterministic write-failure seam above.
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .waiter_count(&request_id),
            Some(2)
        );

        submit_tool_result_over_ipc(
            &runtime_path,
            "ipc_waiter_submit",
            "thread_ipc_waiters",
            &request_id,
            json!({ "value": 789 }),
        );
        assert!(first.join().unwrap().is_err());
        assert_eq!(
            second.join().unwrap().unwrap().result,
            Some(json!({ "value": 789 }))
        );
        assert_replay_candidate_count_eventually(&runtime, 0);

        let third = spawn_tool_call(
            runtime_path.clone(),
            "ipc_waiter_three",
            "thread_ipc_waiters",
            "get_app_state",
            json!({}),
        );
        let third_request_id = next_tool_call_request_id(&event_rx);
        assert_ne!(third_request_id, request_id);
        submit_tool_result_over_ipc(
            &runtime_path,
            "ipc_waiter_submit_three",
            "thread_ipc_waiters",
            &third_request_id,
            json!({ "value": 101 }),
        );
        assert_eq!(
            third.join().unwrap().unwrap().result,
            Some(json!({ "value": 101 }))
        );
    }

    #[test]
    fn tool_call_times_out_without_submit() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_timeout",
            ThreadStatus::Running,
            20,
        );

        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "tool_timeout".into(),
                r#type: "tool_call".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_timeout",
                    "toolName": "get_app_state",
                    "args": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();

        assert_eq!(response.error.unwrap().code, error_codes::TOOL_TIMEOUT);
    }

    #[test]
    fn ipc_failed_timeout_delivery_replays_formal_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_ipc_timeout_replay",
            ThreadStatus::Running,
            20,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_ipc_timeout_replay");

        force_tool_response_write_failure_for_test("ipc_timeout_original");
        let first = spawn_tool_call(
            runtime_path.clone(),
            "ipc_timeout_original",
            "thread_ipc_timeout_replay",
            "get_app_state",
            json!({}),
        );
        let original_request_id = next_tool_call_request_id(&event_rx);
        assert!(first.join().unwrap().is_err());
        assert_eq!(
            runtime.lock().unwrap().tool_request_broker.pending_count(),
            0
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .replay_candidate_count(),
            1
        );

        let replay = send_core_ipc_request_with_runtime_path(
            &tool_call_request(
                "ipc_timeout_replay",
                "thread_ipc_timeout_replay",
                "get_app_state",
                json!({}),
            ),
            &runtime_path,
        )
        .unwrap();
        assert!(!replay.ok);
        assert_eq!(replay.error.unwrap().code, error_codes::TOOL_TIMEOUT);
        assert_replay_candidate_count_eventually(&runtime, 0);
        assert_eq!(
            runtime.lock().unwrap().tool_request_broker.pending_count(),
            0
        );
        assert!(!original_request_id.is_empty());
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolCall { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn ipc_successful_timeout_delivery_allows_a_new_exact_call() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_ipc_timeout_new",
            ThreadStatus::Running,
            20,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_ipc_timeout_new");

        let first = spawn_tool_call(
            runtime_path.clone(),
            "ipc_timeout_success_original",
            "thread_ipc_timeout_new",
            "get_app_state",
            json!({}),
        );
        let first_request_id = next_tool_call_request_id(&event_rx);
        let first_response = first.join().unwrap().unwrap();
        assert!(!first_response.ok);
        assert_eq!(
            first_response.error.unwrap().code,
            error_codes::TOOL_TIMEOUT
        );
        assert_replay_candidate_count_eventually(&runtime, 0);

        let second = spawn_tool_call(
            runtime_path.clone(),
            "ipc_timeout_new_call",
            "thread_ipc_timeout_new",
            "get_app_state",
            json!({}),
        );
        let second_request_id = next_tool_call_request_id(&event_rx);
        assert_ne!(second_request_id, first_request_id);
        submit_tool_result_over_ipc(
            &runtime_path,
            "ipc_timeout_new_submit",
            "thread_ipc_timeout_new",
            &second_request_id,
            json!({ "recovered": true }),
        );
        assert_eq!(
            second.join().unwrap().unwrap().result,
            Some(json!({ "recovered": true }))
        );
    }

    #[test]
    fn exact_second_tool_call_joins_while_pending_exists() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_pending",
            ThreadStatus::Running,
            1000,
        );
        let mut subscription = subscribe_to_thread(&runtime_path, "thread_pending");

        let first_path = runtime_path.clone();
        let first_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_first".into(),
                    r#type: "tool_call".into(),
                    caller_origin: None,
                    caller_sdk_version: None,
                    payload: Some(json!({
                        "threadId": "thread_pending",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                first_path,
            )
            .unwrap()
        });

        let event = read_thread_event(&mut subscription);
        let request_id = match event.event {
            ThreadEvent::StatusChanged { .. } => match read_thread_event(&mut subscription).event {
                ThreadEvent::ToolCall { request_id, .. } => request_id,
                other => panic!("expected tool_call event, got {other:?}"),
            },
            ThreadEvent::ToolCall { request_id, .. } => request_id,
            other => panic!("expected status_changed/tool_call event, got {other:?}"),
        };

        let second_path = runtime_path.clone();
        let second_handle = thread::spawn(move || {
            send_core_ipc_request_with_runtime_path(
                &CoreIpcRequest {
                    request_id: "tool_second".into(),
                    r#type: "tool_call".into(),
                    caller_origin: None,
                    caller_sdk_version: None,
                    payload: Some(json!({
                        "threadId": "thread_pending",
                        "toolName": "get_app_state",
                        "args": {}
                    })),
                },
                second_path,
            )
            .unwrap()
        });
        for _ in 0..100 {
            if runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .waiter_count(&request_id)
                == Some(2)
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .waiter_count(&request_id),
            Some(2)
        );

        send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "submit_pending".into(),
                r#type: "submit_tool_result".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_pending",
                    "requestId": request_id,
                    "result": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(first_handle.join().unwrap().result, Some(json!({})));
        let second_response = second_handle.join().unwrap();
        assert!(
            second_response.ok,
            "unexpected second response: {second_response:?}"
        );
        assert_eq!(second_response.result, Some(json!({})));
    }

    #[test]
    fn tool_call_rejects_missing_tool_and_schema_invalid_args() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        let runtime_path = temp.path().join("runtime.json");
        start_core_ipc_server_with_runtime_path(Arc::clone(&runtime), &runtime_path).unwrap();
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_schema",
            ThreadStatus::Running,
            1000,
        );

        let missing = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "missing_tool".into(),
                r#type: "tool_call".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_schema",
                    "toolName": "missing",
                    "args": {}
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(missing.error.unwrap().code, error_codes::TOOL_NOT_FOUND);

        let invalid = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: "invalid_args".into(),
                r#type: "tool_call".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": "thread_schema",
                    "toolName": "update_counter",
                    "args": { "delta": "1" }
                })),
            },
            &runtime_path,
        )
        .unwrap();
        assert_eq!(invalid.error.unwrap().code, error_codes::TOOL_ARGS_INVALID);
    }

    #[test]
    fn create_thread_rolls_back_workspace_when_skill_load_fails() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let result = runtime.create_thread(CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: Some(EffortLevel::Default),
            skills: Some(CreateThreadSkillsInput {
                guidance: "bad".into(),
                tools: vec![CreateThreadToolInput {
                    name: "bad/name".into(),
                    description: "Bad.".into(),
                    args_schema: json!({ "type": "object" }),
                    timeout_ms: None,
                }],
            }),
            workspace: None,
        });

        assert_eq!(
            result.unwrap_err().code,
            error_codes::TOOLS_MANIFEST_INVALID
        );
        let entries = std::fs::read_dir(&workspace_root)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(entries, 0);
    }

    #[test]
    fn persistent_dispatch_receives_an_intent_and_user_turn_is_admitted_first() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_persistent_ipc",
            ThreadStatus::Idle,
            1000,
        );
        let dispatcher = Arc::new(RecordingPersistentDispatcher::default());

        let output = start_provider_turn_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher.clone(),
            SendTextInput {
                thread_id: "thread_persistent_ipc".into(),
                message: "hello".into(),
                operation_id: None,
            },
        )
        .unwrap();

        assert_eq!(output.thread_id, "thread_persistent_ipc");
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread_persistent_ipc"),
            Some(ThreadStatus::Running)
        );
        let operations = dispatcher.operations.lock().unwrap();
        assert!(matches!(
            operations.first(),
            Some(PersistentRuntimeOperation::StartTurn { .. })
        ));
    }

    #[test]
    fn debug_send_text_dispatches_a_persistent_turn() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_debug_persistent_ipc",
            ThreadStatus::Idle,
            1000,
        );
        let dispatcher = Arc::new(RecordingPersistentDispatcher::default());

        let output = start_debug_provider_turn_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher.clone(),
            SendTextInput {
                thread_id: "thread_debug_persistent_ipc".into(),
                message: "debug hello".into(),
                operation_id: None,
            },
        )
        .unwrap();

        assert_eq!(output.thread_id, "thread_debug_persistent_ipc");
        let operations = dispatcher.operations.lock().unwrap();
        let Some(PersistentRuntimeOperation::StartTurn { turn }) = operations.first() else {
            panic!("debug send should dispatch a persistent start-turn operation");
        };
        assert_eq!(turn.thread_id, "thread_debug_persistent_ipc");
        assert_eq!(turn.message, "debug hello");
    }

    #[test]
    fn prepare_dispatches_a_persistent_session_operation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_prepare_persistent_ipc",
            ThreadStatus::Idle,
            1000,
        );
        let dispatcher = Arc::new(RecordingPersistentDispatcher::default());

        let output = prepare_provider_session_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher.clone(),
            PrepareThreadInput {
                thread_id: "thread_prepare_persistent_ipc".into(),
                operation_id: None,
            },
        )
        .unwrap();

        assert!(output.prepared);
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread_prepare_persistent_ipc"),
            Some(ThreadStatus::Running)
        );
        let operations = dispatcher.operations.lock().unwrap();
        let Some(PersistentRuntimeOperation::EnsureSession { session }) = operations.first() else {
            panic!("prepare should dispatch a persistent ensure-session operation");
        };
        assert_eq!(session.thread_id, "thread_prepare_persistent_ipc");
    }

    #[test]
    fn end_thread_dispatches_persistent_session_end_and_finishes_thread() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_end_persistent_ipc",
            ThreadStatus::Idle,
            1000,
        );
        {
            let mut runtime = runtime.lock().unwrap();
            let session = runtime
                .thread_manager
                .provider_session_state_mut("thread_end_persistent_ipc")
                .unwrap();
            session.provider_session_id = Some("provider-session".into());
            session.active_provider_turn_id = Some("provider-turn".into());
        }
        let dispatcher = Arc::new(RecordingPersistentDispatcher::default());

        end_thread_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher.clone(),
            EndThreadInput {
                thread_id: "thread_end_persistent_ipc".into(),
            },
        )
        .unwrap();

        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread_end_persistent_ipc"),
            Some(ThreadStatus::Ended)
        );
        let operations = dispatcher.operations.lock().unwrap();
        let Some(PersistentRuntimeOperation::EndSession { session }) = operations.first() else {
            panic!("end thread should dispatch a persistent end-session operation");
        };
        assert_eq!(session.thread_id, "thread_end_persistent_ipc");
        assert_eq!(
            session.provider_session_id.as_deref(),
            Some("provider-session")
        );
        assert_eq!(
            session.active_provider_turn_id.as_deref(),
            Some("provider-turn")
        );
    }

    #[test]
    fn persistent_dispatch_failure_rolls_a_user_turn_into_error() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_persistent_dispatch_failure",
            ThreadStatus::Idle,
            1000,
        );
        let dispatcher = Arc::new(RecordingPersistentDispatcher {
            operations: Mutex::new(Vec::new()),
            error: true,
        });

        let error = start_provider_turn_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher,
            SendTextInput {
                thread_id: "thread_persistent_dispatch_failure".into(),
                message: "hello".into(),
                operation_id: None,
            },
        )
        .unwrap_err();

        assert_eq!(error.code, error_codes::PROVIDER_RUNTIME_START_FAILED);
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread_persistent_dispatch_failure"),
            Some(ThreadStatus::Error)
        );
    }

    #[test]
    fn persistent_prepare_dispatch_failure_returns_to_idle_with_provider_error() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(CoreRuntime::default()));
        insert_thread_with_registry(
            &runtime,
            temp.path(),
            "thread_persistent_prepare_dispatch_failure",
            ThreadStatus::Idle,
            1000,
        );
        let event_rx = runtime
            .lock()
            .unwrap()
            .event_bus
            .subscribe("thread_persistent_prepare_dispatch_failure");
        let dispatcher = Arc::new(RecordingPersistentDispatcher {
            operations: Mutex::new(Vec::new()),
            error: true,
        });

        let error = prepare_provider_session_with_dispatcher(
            Arc::clone(&runtime),
            dispatcher,
            PrepareThreadInput {
                thread_id: "thread_persistent_prepare_dispatch_failure".into(),
                operation_id: None,
            },
        )
        .unwrap_err();

        assert_eq!(error.code, error_codes::PROVIDER_RUNTIME_START_FAILED);
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_status("thread_persistent_prepare_dispatch_failure"),
            Some(ThreadStatus::Idle)
        );
        let events = collect_events_until(&event_rx, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    ThreadEvent::StatusChanged {
                        status: ThreadStatus::Idle,
                        ..
                    }
                )
            })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { error, .. }
                if error.code == error_codes::PROVIDER_RUNTIME_START_FAILED
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::StatusChanged {
                status: ThreadStatus::Idle,
                ..
            }
        )));
    }

    #[derive(Debug, Default)]
    struct RecordingPersistentDispatcher {
        operations: Mutex<Vec<PersistentRuntimeOperation>>,
        error: bool,
    }

    impl PersistentRuntimeDispatcher for RecordingPersistentDispatcher {
        fn dispatch(&self, operation: PersistentRuntimeOperation) -> Result<(), PedelecError> {
            self.operations.lock().unwrap().push(operation);
            if self.error {
                Err(PedelecError::new(
                    error_codes::PROVIDER_RUNTIME_START_FAILED,
                    "test dispatcher rejected operation",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn insert_thread_with_registry(
        runtime: &Arc<Mutex<CoreRuntime>>,
        temp: &Path,
        thread_id: &str,
        status: ThreadStatus,
        timeout_ms: u64,
    ) {
        let mut runtime = runtime.lock().unwrap();
        runtime.provider_readiness.mark_ready_for_test();
        let now = chrono::Utc::now();
        let workspace_root = temp.join("workspace");
        runtime.workspace_manager = WorkspaceManager::with_workspace_root(&workspace_root);
        let workspace_path = workspace_root.join(thread_id);
        std::fs::create_dir_all(workspace_logs_root(&workspace_path)).unwrap();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: vec![],
                workspace_path,
                skills: vec![],
                status: status.clone(),
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
            },
        );
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_tools_json_str(&format!(
                r#"{{
                    "tools": [
                        {{
                            "name": "get_app_state",
                            "description": "Read state.",
                            "argsSchema": {{
                                "type": "object",
                                "properties": {{}},
                                "additionalProperties": false
                            }},
                            "timeoutMs": {timeout_ms}
                        }},
                        {{
                            "name": "update_counter",
                            "description": "Update counter.",
                            "argsSchema": {{
                                "type": "object",
                                "properties": {{ "delta": {{ "type": "integer" }} }},
                                "required": ["delta"],
                                "additionalProperties": false
                            }},
                            "timeoutMs": {timeout_ms}
                        }}
                    ]
                }}"#
            ))
            .unwrap(),
        );
        if matches!(
            status,
            ThreadStatus::Running | ThreadStatus::WaitingToolResult
        ) {
            runtime.pending_provider_operations.insert(
                thread_id.to_string(),
                PendingProviderOperation {
                    operation_id: format!("test-operation-{thread_id}"),
                    kind: PendingProviderOperationKind::UserTurn,
                    started_at: now,
                },
            );
        }
    }

    fn collect_events_until(
        event_rx: &std::sync::mpsc::Receiver<ThreadEvent>,
        done: impl Fn(&[ThreadEvent]) -> bool,
    ) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        for _ in 0..20 {
            let event = event_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("timed out waiting for thread event");
            events.push(event);
            if done(&events) {
                return events;
            }
        }
        panic!("condition was not met by collected events: {events:?}");
    }

    fn collect_ipc_events_until(
        reader: &mut BufReader<TcpStream>,
        done: impl Fn(&[ThreadEvent]) -> bool,
    ) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        for _ in 0..20 {
            let event = read_thread_event(reader).event;
            events.push(event);
            if done(&events) {
                return events;
            }
        }
        panic!("condition was not met by collected IPC events: {events:?}");
    }

    fn assert_thread_event_seq_is_strictly_increasing(events: &[ThreadEvent]) {
        let mut previous = 0;
        for event in events {
            let seq = event.seq();
            assert!(seq > previous, "event seq did not increase: {events:?}");
            previous = seq;
        }
    }

    fn subscribe_to_thread(runtime_path: &Path, thread_id: &str) -> BufReader<TcpStream> {
        let mut stream = connect_core_ipc_with_runtime_path(runtime_path).unwrap();
        write_json_line(
            &mut stream,
            &CoreIpcRequest {
                request_id: "sub".into(),
                r#type: "subscribe_thread".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({ "threadId": thread_id })),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let response_line = read_bounded_json_line(&mut reader).unwrap();
        let response: CoreIpcResponse = serde_json::from_slice(&response_line).unwrap();
        assert!(response.ok);
        reader
    }

    fn tool_call_request(
        request_id: &str,
        thread_id: &str,
        tool_name: &str,
        args: Value,
    ) -> CoreIpcRequest {
        CoreIpcRequest {
            request_id: request_id.into(),
            r#type: "tool_call".into(),
            caller_origin: None,
            caller_sdk_version: None,
            payload: Some(json!({
                "threadId": thread_id,
                "toolName": tool_name,
                "args": args,
            })),
        }
    }

    fn spawn_tool_call(
        runtime_path: PathBuf,
        request_id: &str,
        thread_id: &str,
        tool_name: &str,
        args: Value,
    ) -> thread::JoinHandle<Result<CoreIpcResponse, pedelec_core::PedelecError>> {
        let request = tool_call_request(request_id, thread_id, tool_name, args);
        thread::spawn(move || send_core_ipc_request_with_runtime_path(&request, runtime_path))
    }

    fn next_tool_call_request_id(event_rx: &std::sync::mpsc::Receiver<ThreadEvent>) -> String {
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("timed out waiting for tool_call event");
            if let ThreadEvent::ToolCall { request_id, .. } = event {
                return request_id;
            }
        }
    }

    fn assert_replay_candidate_count_eventually(
        runtime: &Arc<Mutex<CoreRuntime>>,
        expected: usize,
    ) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let actual = runtime
                .lock()
                .unwrap()
                .tool_request_broker
                .replay_candidate_count();
            if actual == expected {
                return;
            }
            if Instant::now() >= deadline {
                assert_eq!(
                    actual, expected,
                    "timed out waiting for replay candidate count"
                );
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn submit_tool_result_over_ipc(
        runtime_path: &Path,
        request_id: &str,
        thread_id: &str,
        tool_request_id: &str,
        result: Value,
    ) {
        let response = send_core_ipc_request_with_runtime_path(
            &CoreIpcRequest {
                request_id: request_id.into(),
                r#type: "submit_tool_result".into(),
                caller_origin: None,
                caller_sdk_version: None,
                payload: Some(json!({
                    "threadId": thread_id,
                    "requestId": tool_request_id,
                    "result": result,
                })),
            },
            runtime_path,
        )
        .expect("submit_tool_result IPC request failed");
        assert!(response.ok, "unexpected submit response: {response:?}");
    }

    fn test_command_spec(program: &str, cwd: &Path, args: Vec<String>) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args,
            cwd: cwd.to_path_buf(),
            env: vec![("PATH".into(), cwd.to_string_lossy().into())],
            prompt: String::new(),
            stdin: String::new(),
        }
    }

    fn test_provider_path(root: &Path, program: &str) -> std::ffi::OsString {
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let program_name = format!("{program}.exe");
        #[cfg(not(windows))]
        let program_name = program.to_string();
        std::fs::write(bin_dir.join(program_name), b"fake provider").unwrap();
        env::join_paths([bin_dir]).unwrap()
    }

    fn read_thread_event(reader: &mut BufReader<TcpStream>) -> CoreIpcEventMessage {
        let event_line = read_bounded_json_line(reader).unwrap();
        serde_json::from_slice(&event_line).unwrap()
    }

    fn phase09_tools_json() -> &'static str {
        r#"{
            "tools": [
                {
                    "name": "get_app_state",
                    "description": "Read current app state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    },
                    "timeoutMs": 60000
                },
                {
                    "name": "update_counter",
                    "description": "Update counter by delta.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {
                            "delta": { "type": "number" }
                        },
                        "required": ["delta"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 60000
                }
            ]
        }"#
    }

    fn phase09_skills_manifest() -> Value {
        let registry: Value = serde_json::from_str(phase09_tools_json()).unwrap();
        json!({
            "guidance": "Use get_app_state to read app state. Use update_counter to update the counter.",
            "tools": registry["tools"].clone()
        })
    }
}
