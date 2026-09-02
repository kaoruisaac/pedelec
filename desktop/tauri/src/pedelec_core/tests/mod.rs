use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn claude_raw_stdout(events: &[ThreadEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::RawStdout { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn emit_claude_in_chunks(
        runtime: &mut CoreRuntime,
        thread_id: &str,
        input: &str,
        chunk_size: usize,
    ) {
        for chunk in input.as_bytes().chunks(chunk_size) {
            runtime.emit_provider_stdout(thread_id, String::from_utf8(chunk.to_vec()).unwrap());
        }
    }

    #[test]
    fn bootstrap_instruction_explains_exact_retry_and_formal_timeout() {
        let instruction = build_pedelec_bootstrap_instruction();
        assert!(instruction.contains("ambiguous transport failure"));
        assert!(instruction.contains("exact same tool name"));
        assert!(
            instruction.contains("complete structured Pedelec response, including `TOOL_TIMEOUT`")
        );
        assert!(instruction.contains("do not retry indefinitely"));
        assert!(instruction
            .contains("`.pedelec-runtime/assets/` is the shared App and Agent file directory"));
        assert!(!instruction.contains("`assets/` is the shared App and Agent file directory"));
        assert!(instruction
            .contains("pedelec-cli --thread-id <pedelec_thread_id> tool-spec <tool-name>"));
        assert!(instruction.contains(
            "pedelec-cli --thread-id <pedelec_thread_id> tool-call <tool-name> '<json_args>'"
        ));
    }

    #[test]
    fn provider_readiness_wakes_all_waiters_after_snapshot_install() {
        let readiness = ProviderReadiness::new_uninitialized();
        readiness.mark_initial_scanning();
        let (tx, rx) = mpsc::channel();
        let barrier = Arc::new(Barrier::new(5));

        for _ in 0..4 {
            let readiness = readiness.clone();
            let tx = tx.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                tx.send(readiness.wait()).unwrap();
            });
        }

        barrier.wait();

        readiness.mark_ready();

        for _ in 0..4 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
        }
    }

    #[test]
    fn provider_readiness_failure_releases_waiters_with_diagnostic_error() {
        let readiness = ProviderReadiness::new_uninitialized();
        readiness.mark_initial_scanning();
        let (tx, rx) = mpsc::channel();
        let waiter_readiness = readiness.clone();
        let barrier = Arc::new(Barrier::new(2));
        let waiter_barrier = barrier.clone();
        thread::spawn(move || {
            waiter_barrier.wait();
            tx.send(waiter_readiness.wait()).unwrap();
        });
        barrier.wait();

        readiness.mark_failed(PedelecError::new(
            error_codes::PROVIDER_SCAN_FAILED,
            "scan failed in test",
        ));

        let error = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_SCAN_FAILED);
        assert_eq!(error.message, "scan failed in test");
    }

    #[test]
    fn later_refresh_returns_the_ready_snapshot_while_scan_is_in_progress() {
        let runtime = Arc::new(Mutex::new(CoreRuntime::new()));
        runtime.lock().unwrap().provider_path_value_override = Some(OsString::new());
        runtime.lock().unwrap().refresh_providers();
        let previous_snapshot = runtime.lock().unwrap().list_providers();
        runtime.lock().unwrap().provider_refresh_in_progress = true;

        let during_refresh = refresh_shared_providers(&runtime);

        assert_eq!(during_refresh, previous_snapshot);
    }

    #[test]
    fn provider_info_exposes_scanned_version_without_exposing_ollama_version() {
        let scan = HashMap::from([(
            ProviderCode::Codex,
            ProviderCli {
                path: Some(PathBuf::from("C:/providers/codex.cmd")),
                version: Some(ProviderVersion(vec![1, 2, 3])),
                error: None,
                bootstrap_capabilities: None,
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            },
        )]);

        let codex = provider_info_for(ProviderCode::Codex, &scan, None);
        assert!(codex.scanned);
        assert_eq!(codex.version.as_deref(), Some("1.2.3"));
        assert_eq!(
            serde_json::to_value(&codex).unwrap()["version"],
            json!("1.2.3")
        );

        let ollama = provider_info_for(ProviderCode::Ollama, &scan, Some(&OsString::from("")));
        assert_eq!(ollama.version, None);
        assert!(serde_json::to_value(&ollama)
            .unwrap()
            .get("version")
            .is_none());

        let antigravity = provider_info_for(ProviderCode::Antigravity, &scan, None);
        assert!(!antigravity.scanned);
    }

    #[test]
    fn codex_without_app_server_capability_is_unavailable_without_a_legacy_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_codex_path(temp.path(), false);
        let providers = list_provider_infos(Some(provider_path.clone()));
        let codex = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Codex)
            .unwrap();

        assert!(codex.scanned);
        assert_eq!(codex.version.as_deref(), Some("9.9.9"));
        assert!(!codex.available);
        assert!(codex
            .error
            .as_deref()
            .is_some_and(|error| error.contains("app-server")));

        let mut runtime = CoreRuntime {
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::new_for_application()
        };
        runtime.refresh_providers();
        let error = runtime
            .provider_executable_path(&ProviderCode::Codex)
            .unwrap_err();
        assert_eq!(error.code, error_codes::PROVIDER_TERMINAL_UNAVAILABLE);
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("appServerCapability")),
            Some(&json!(false))
        );
        assert_eq!(
            runtime.provider_execution_family(&ProviderCode::Codex),
            ProviderExecutionFamily::PersistentRuntime
        );
    }

    #[test]
    fn application_runtime_routes_migrated_providers_persistently() {
        let runtime = CoreRuntime::new_for_application();
        for provider in [
            ProviderCode::Codex,
            ProviderCode::Antigravity,
            ProviderCode::OpenCode,
            ProviderCode::Cursor,
        ] {
            assert_eq!(
                runtime.provider_execution_family(&provider),
                ProviderExecutionFamily::PersistentRuntime,
                "provider={provider:?}"
            );
        }
        for provider in [ProviderCode::Claude, ProviderCode::Ollama] {
            assert_eq!(
                runtime.provider_execution_family(&provider),
                ProviderExecutionFamily::LegacyCommand,
                "provider={provider:?}"
            );
        }
    }

    #[test]
    fn antigravity_persistent_intent_uses_selected_native_model_and_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_typed_settings";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Antigravity);
        let thread = runtime.thread_manager.thread_mut(thread_id).unwrap();
        thread.effort_level = EffortLevel::High;
        thread.effort_args = vec![
            "--model".into(),
            "agy-native-model".into(),
            "--effort".into(),
            "low".into(),
        ];

        let session = runtime.build_persistent_session_intent(thread_id).unwrap();
        assert_eq!(session.model.as_deref(), Some("agy-native-model"));
        assert_eq!(
            session.antigravity_reasoning_effort,
            Some(AntigravityReasoningEffort::Low)
        );
        assert_eq!(session.effort_level, EffortLevel::High);
        assert_eq!(session.reasoning_effort, None);
    }

    #[test]
    fn antigravity_persistent_intent_keeps_empty_native_settings_optional() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_empty_settings";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Antigravity);
        runtime
            .thread_manager
            .thread_mut(thread_id)
            .unwrap()
            .effort_args
            .clear();

        let session = runtime.build_persistent_session_intent(thread_id).unwrap();
        assert_eq!(session.model, None);
        assert_eq!(session.antigravity_reasoning_effort, None);
    }

    #[test]
    fn provider_runtime_diagnostics_are_serialized_and_bounded() {
        let mut runtime = CoreRuntime::new();
        let receiver = runtime.subscribe_provider_runtime_diagnostics();

        for generation in 0..520 {
            runtime.record_provider_runtime_diagnostic(
                ProviderRuntimeDiagnostic::ProviderRuntimeStarted {
                    provider: ProviderCode::Codex,
                    runtime_generation: generation,
                    process_id: generation as u32,
                },
            );
        }

        let history = runtime.provider_runtime_diagnostic_history();
        assert_eq!(history.len(), 512);
        assert_eq!(history.first().unwrap().thread_id(), None);
        assert_eq!(history.last().unwrap().thread_id(), None);
        assert_eq!(
            serde_json::to_value(history.last().unwrap()).unwrap()["type"],
            json!("provider_runtime_started")
        );
        assert_eq!(
            serde_json::to_value(history.last().unwrap()).unwrap()["runtimeGeneration"],
            json!(519)
        );
        assert_eq!(receiver.try_iter().count(), 520);
    }

    #[test]
    fn provider_bootstrap_modes_cover_internal_defaults_and_probe_fallbacks() {
        let runtime = CoreRuntime::default();
        assert_eq!(
            runtime.provider_bootstrap_mode(&ProviderCode::Ollama),
            ProviderBootstrapMode::NativeSystemPrompt
        );
        assert_eq!(
            runtime.provider_bootstrap_mode(&ProviderCode::Cursor),
            ProviderBootstrapMode::UserPromptFallback
        );

        let scan = ProviderCli {
            path: Some(PathBuf::from("provider")),
            version: Some(ProviderVersion(vec![1])),
            error: None,
            bootstrap_capabilities: None,
            app_server_capability: None,
            acp_capability: None,
            stream_json_capability: None,
        };
        assert_eq!(
            provider_bootstrap_capability_from_probe(
                &ProviderCode::Claude,
                Some(&scan),
                true,
                false
            )
            .privileged_bootstrap,
            ProviderBootstrapMode::ClaudeAppendSystemPrompt
        );
        assert_eq!(
            provider_bootstrap_capability_from_probe(
                &ProviderCode::Claude,
                Some(&scan),
                false,
                false
            )
            .privileged_bootstrap,
            ProviderBootstrapMode::UserPromptFallback
        );
        assert_eq!(
            provider_bootstrap_capability_from_probe(
                &ProviderCode::OpenCode,
                Some(&scan),
                false,
                true
            )
            .privileged_bootstrap,
            ProviderBootstrapMode::OpenCodeInlineAgent
        );
        assert_eq!(
            provider_bootstrap_capability_from_probe(
                &ProviderCode::OpenCode,
                Some(&scan),
                false,
                false
            )
            .privileged_bootstrap,
            ProviderBootstrapMode::UserPromptFallback
        );
        assert_eq!(
            provider_bootstrap_capability_from_probe(
                &ProviderCode::Cursor,
                Some(&scan),
                true,
                true
            )
            .privileged_bootstrap,
            ProviderBootstrapMode::UserPromptFallback
        );
    }

    #[test]
    fn antigravity_persistent_readiness_requires_stream_json_and_custom_agent_support() {
        for (version, stream_json_supported, expected_available, expected_error) in [
            ("1.1.6", false, false, Some("stream-json")),
            ("1.1.5", true, false, Some("workspace custom agent")),
            ("1.1.6", true, true, None),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let provider_path = test_antigravity_path(temp.path(), version, stream_json_supported);
            let mut runtime = CoreRuntime {
                provider_path_value_override: Some(provider_path),
                ..CoreRuntime::new_for_application()
            };
            runtime.refresh_providers();

            let provider = runtime
                .list_providers()
                .into_iter()
                .find(|provider| provider.code == ProviderCode::Antigravity)
                .unwrap();
            assert!(provider.scanned);
            assert_eq!(provider.version.as_deref(), Some(version));
            assert_eq!(provider.available, expected_available);
            if let Some(expected_error) = expected_error {
                assert!(provider
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected_error)));
                let error = runtime
                    .provider_executable_path(&ProviderCode::Antigravity)
                    .unwrap_err();
                assert_eq!(error.code, error_codes::PROVIDER_TERMINAL_UNAVAILABLE);
                assert_eq!(
                    error
                        .details
                        .as_ref()
                        .and_then(|details| details.get("runtimeCapabilityAvailable")),
                    Some(&json!(false))
                );
            } else {
                assert!(provider.error.is_none());
                assert!(runtime
                    .provider_executable_path(&ProviderCode::Antigravity)
                    .is_ok());
            }
        }
    }

    #[test]
    fn antigravity_custom_agent_version_gate_maps_supported_versions() {
        for (version, expected) in [
            (vec![1, 1, 5], ProviderBootstrapMode::UserPromptFallback),
            (
                vec![1, 1, 6],
                ProviderBootstrapMode::AntigravityWorkspaceAgent,
            ),
            (
                vec![1, 2, 0],
                ProviderBootstrapMode::AntigravityWorkspaceAgent,
            ),
        ] {
            let scan = ProviderCli {
                path: Some(PathBuf::from("agy")),
                version: Some(ProviderVersion(version)),
                error: None,
                bootstrap_capabilities: None,
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            };
            assert_eq!(
                provider_bootstrap_capability_from_probe(
                    &ProviderCode::Antigravity,
                    Some(&scan),
                    false,
                    false
                )
                .privileged_bootstrap,
                expected
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn provider_version_command_runs_script_wrappers_through_headless_cmd() {
        let command = provider_version_command(Path::new("C:/providers/codex.cmd"), None);

        assert_eq!(command.get_program(), "cmd.exe");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["/d", "/c", "call", "C:/providers/codex.cmd"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn merged_provider_path_preserves_paths_usable_by_cmd_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().to_path_buf();
        let merged = merged_provider_path(Some(env::join_paths([&original]).unwrap()));

        assert!(env::split_paths(&merged).any(|path| path == original));
        assert!(!env::split_paths(&merged).any(|path| path.to_string_lossy().starts_with(r"\\?\")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_fallback_paths_include_claude_native_bin() {
        let home = dirs::home_dir().expect("Windows test environment should have a home directory");

        assert!(provider_fallback_paths().contains(&home.join(".local/bin")));
    }

    #[test]
    fn merge_provider_paths_keeps_process_login_and_existing_fallback_directories() {
        let temp = tempfile::tempdir().unwrap();
        let process_dir = temp.path().join("process");
        let login_dir = temp.path().join("login");
        let fallback_dir = temp.path().join("fallback");
        let missing_dir = temp.path().join("missing");
        fs::create_dir_all(&process_dir).unwrap();
        fs::create_dir_all(&login_dir).unwrap();
        fs::create_dir_all(&fallback_dir).unwrap();

        let process_path = env::join_paths([&process_dir, &login_dir]).unwrap();
        let login_path = env::join_paths([&login_dir]).unwrap();
        let merged = merge_provider_paths(
            Some(&process_path),
            Some(&login_path),
            vec![fallback_dir.clone(), missing_dir],
        );
        let paths = env::split_paths(&merged).collect::<Vec<_>>();

        assert_eq!(paths, vec![process_dir, login_dir, fallback_dir]);
    }

    #[test]
    fn login_shell_path_parser_handles_noise_and_rejects_invalid_output() {
        let parsed = parse_login_shell_path_output(
            b"startup noise\nprompt/plugin warning\n__PEDELEC_PATH_START__/one:/two__PEDELEC_PATH_END__\nmore noise",
        )
        .unwrap();
        assert_eq!(parsed, OsString::from("/one:/two"));

        assert!(parse_login_shell_path_output(b"__PEDELEC_PATH_END__/one").is_none());
        assert!(parse_login_shell_path_output(b"__PEDELEC_PATH_START__/one").is_none());
        assert!(
            parse_login_shell_path_output(b"__PEDELEC_PATH_START____PEDELEC_PATH_END__").is_none()
        );
        assert!(parse_login_shell_path_output(&[0xff, 0xfe]).is_none());
        assert_eq!(
            parse_login_shell_path_output(
                b"\xffstartup\n__PEDELEC_PATH_START__/one:/two__PEDELEC_PATH_END__"
            )
            .unwrap(),
            OsString::from("/one:/two")
        );
    }

    #[test]
    fn macos_shell_strategy_uses_interactive_login_for_zsh() {
        assert_eq!(
            macos_shell_probe_arguments(Path::new("/opt/homebrew/bin/zsh")),
            vec![vec!["-l", "-i", "-c", MACOS_PATH_MARKER_COMMAND]]
        );
    }

    #[test]
    fn macos_shell_strategy_probes_bash_login_and_interactive_separately() {
        assert_eq!(
            macos_shell_probe_arguments(Path::new("/usr/local/bin/bash")),
            vec![
                vec!["-l", "-c", MACOS_PATH_MARKER_COMMAND],
                vec!["-i", "-c", MACOS_PATH_MARKER_COMMAND],
            ]
        );
    }

    #[cfg(unix)]
    fn executable_shell_fixture(temp: &tempfile::TempDir, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let shell = temp.path().join("bash");
        fs::write(&shell, body).unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        shell
    }

    #[cfg(unix)]
    #[test]
    fn bash_login_success_is_preserved_when_interactive_probe_fails() {
        let temp = tempfile::tempdir().unwrap();
        let shell = executable_shell_fixture(
            &temp,
            "#!/bin/sh\nif [ \"$1\" = \"-l\" ]; then\n  printf 'warning\\n%s/login/bin%s\\n' '__PEDELEC_PATH_START__' '__PEDELEC_PATH_END__'\n  exit 0\nfi\nexit 1\n",
        );

        let path = resolve_macos_shell_path_with_strategies(
            &shell,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(path, OsString::from("/login/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn bash_interactive_success_is_preserved_when_login_probe_fails() {
        let temp = tempfile::tempdir().unwrap();
        let shell = executable_shell_fixture(
            &temp,
            "#!/bin/sh\nif [ \"$1\" = \"-i\" ]; then\n  printf '%s/interactive/bin%s\\n' '__PEDELEC_PATH_START__' '__PEDELEC_PATH_END__'\n  exit 0\nfi\nexit 1\n",
        );

        let path = resolve_macos_shell_path_with_strategies(
            &shell,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(path, OsString::from("/interactive/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn bash_successful_probes_are_merged_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let shell = executable_shell_fixture(
            &temp,
            "#!/bin/sh\nif [ \"$1\" = \"-l\" ]; then\n  printf '%s/login/bin:/shared/bin%s\\n' '__PEDELEC_PATH_START__' '__PEDELEC_PATH_END__'\nelse\n  printf '%s/interactive/bin:/shared/bin%s\\n' '__PEDELEC_PATH_START__' '__PEDELEC_PATH_END__'\nfi\n",
        );

        let path = resolve_macos_shell_path_with_strategies(
            &shell,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(
            env::split_paths(&path).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/login/bin"),
                PathBuf::from("/shared/bin"),
                PathBuf::from("/interactive/bin"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_probe_timeout_is_bounded_and_reaps_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let shell = executable_shell_fixture(&temp, "#!/bin/sh\nwhile true; do :; done\n");
        let started = Instant::now();

        let result = probe_shell_path(
            &shell,
            &["-c", MACOS_PATH_MARKER_COMMAND],
            Instant::now() + Duration::from_millis(100),
        );

        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn provider_version_scan_passes_resolved_path_to_env_runtime() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let provider_dir = temp.path().join("provider");
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&provider_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();

        let runtime = runtime_dir.join("fake-runtime");
        fs::write(&runtime, "#!/bin/sh\nprintf '9.8.7\\n'\n").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = provider_dir.join("codex");
        fs::write(&provider, "#!/usr/bin/env fake-runtime\n").unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let resolved_path = env::join_paths([&provider_dir, &runtime_dir]).unwrap();
        let scan = scan_provider_cli("codex", Some(&resolved_path));

        assert_eq!(scan.path, Some(provider));
        assert_eq!(scan.version, Some(ProviderVersion(vec![9, 8, 7])));
    }

    #[test]
    fn codex_new_command_uses_workspace_args_env_prompt_and_no_generated_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_new",
            ProviderCode::Codex,
            None,
            Some("gpt-5".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_codex_new".into(),
                message: "hello".into(),
            })
            .unwrap();

        assert_eq!(start.command.program, "codex");
        let workspace_path = temp.path().join("workspace").join("thread_codex_new");
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["-c", "skills.include_instructions=false"]));
        let bootstrap = start
            .command
            .args
            .windows(2)
            .find(|args| args[0] == "-c" && args[1].starts_with("developer_instructions="))
            .expect("Codex developer bootstrap");
        assert!(bootstrap[1].contains("Pedelec is the host application"));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--cd", workspace_path.to_str().unwrap()]));
        assert!(start.command.args.ends_with(&["gpt-5".into(), "-".into()]));
        assert_eq!(start.command.cwd, workspace_path);
        assert!(!start.command.args.iter().any(|arg| arg == "--last"));
        assert_provider_instruction_present(&start.command);
        assert!(start.command.stdin.ends_with("hello"));
        assert_eq!(env_value(&start.command, "PEDELEC_THREAD_ID"), None);
        assert_env(&start.command, "PEDELEC_PROVIDER", "codex");
        assert_env(
            &start.command,
            "PEDELEC_CORE_IPC_ENDPOINT",
            "127.0.0.1:12345",
        );
        assert!(env_value(&start.command, "PATH")
            .unwrap()
            .contains(".pedelec"));
    }

    #[test]
    fn provider_command_uses_saved_resolved_path_and_pedelec_dir() {
        let temp = tempfile::tempdir().unwrap();
        let resolved_dir = temp.path().join("login-bin");
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_resolved_path",
            ProviderCode::Codex,
            None,
            Some("gpt-5".into()),
        );
        runtime.provider_resolved_path = Some(env::join_paths([&resolved_dir]).unwrap());
        let selected_provider = temp.path().join("scanned-codex");
        runtime.provider_scan.insert(
            ProviderCode::Codex,
            ProviderCli {
                path: Some(selected_provider.clone()),
                version: Some(ProviderVersion(vec![1, 2, 3])),
                error: None,
                bootstrap_capabilities: None,
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            },
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_codex_resolved_path".into(),
                message: "hello".into(),
            })
            .unwrap();
        let command_path = OsString::from(env_value(&start.command, "PATH").unwrap());
        let paths = env::split_paths(&command_path).collect::<Vec<_>>();

        assert_eq!(start.command.program, selected_provider.to_string_lossy());
        assert_eq!(
            paths.first(),
            dirs::home_dir().map(|home| home.join(".pedelec")).as_ref()
        );
        assert!(paths.contains(&resolved_dir));
    }

    #[test]
    fn refresh_stores_the_exact_override_path_used_for_the_scan() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "codex");
        let mut runtime = CoreRuntime {
            provider_path_value_override: Some(provider_path.clone()),
            ..CoreRuntime::default()
        };

        runtime.refresh_providers();

        assert_eq!(runtime.provider_resolved_path, Some(provider_path));
    }

    #[test]
    fn codex_resume_uses_explicit_session_id_and_not_last() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_resume",
            ProviderCode::Codex,
            Some("123e4567-e89b-12d3-a456-426614174000".into()),
            None,
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_codex_resume".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["-c", "skills.include_instructions=false"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-c" && args[1].starts_with("developer_instructions=")));
        assert!(start.command.args.windows(2).any(|args| {
            args == [
                "--cd",
                temp.path()
                    .join("workspace")
                    .join("thread_codex_resume")
                    .to_str()
                    .unwrap(),
            ]
        }));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| { args == ["resume", "123e4567-e89b-12d3-a456-426614174000"] }));
        assert!(!start.command.args.iter().any(|arg| arg == "--last"));
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert_provider_instruction_absent(&start.command);
    }

    #[cfg(windows)]
    #[test]
    fn provider_run_commands_externalize_verbatim_workspace_paths_and_keep_canonical_cwd() {
        let cases = [
            ("codex", ProviderCode::Codex, Some("gpt-5")),
            (
                "antigravity",
                ProviderCode::Antigravity,
                Some("antigravity-2.5-pro"),
            ),
            (
                "opencode",
                ProviderCode::OpenCode,
                Some("ollama/qwen2.5-coder:14b"),
            ),
            ("cursor", ProviderCode::Cursor, Some("gpt-5")),
            ("claude", ProviderCode::Claude, Some("sonnet")),
            ("ollama", ProviderCode::Ollama, Some("qwen3:8b")),
        ];
        let verbatim = PathBuf::from(r"\\?\C:\Users\kaoru\OneDrive\桌面\test");
        let external = r"C:\Users\kaoru\OneDrive\桌面\test";

        for (name, provider, model) in cases {
            let temp = tempfile::tempdir().unwrap();
            let thread_id = format!("thread_externalize_run_{name}");
            let mut runtime = runtime_with_provider_thread(
                temp.path(),
                &thread_id,
                provider.clone(),
                None,
                model.map(str::to_string),
            );
            runtime
                .thread_manager
                .thread_mut(&thread_id)
                .unwrap()
                .workspace_path = verbatim.clone();

            let start = runtime
                .begin_send_text(SendTextInput {
                    thread_id,
                    message: "hello".into(),
                })
                .unwrap();

            assert_eq!(start.command.cwd, verbatim);
            assert_eq!(
                env_value(&start.command, "PEDELEC_WORKSPACE_PATH"),
                Some(external)
            );
            assert_eq!(env_value(&start.command, "PEDELEC_SANDBOX_PATH"), None);
            assert!(!start.command.args.iter().any(|arg| arg.contains(r"\\?\")));
            assert!(!start.command.prompt.contains(r"\\?\"));
            assert!(start.command.prompt.contains(external));

            let path_flag = match provider {
                ProviderCode::Codex => Some("--cd"),
                ProviderCode::OpenCode => Some("--dir"),
                ProviderCode::Cursor => Some("--workspace"),
                ProviderCode::Ollama => Some("--sandbox"),
                ProviderCode::Antigravity | ProviderCode::Claude => None,
            };
            if let Some(path_flag) = path_flag {
                assert!(start
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == [path_flag, external]));
            } else {
                assert!(!start.command.args.iter().any(|arg| arg == external));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn provider_resume_commands_externalize_verbatim_workspace_paths_and_keep_canonical_cwd() {
        let cases = [
            ("codex", ProviderCode::Codex, Some("gpt-5"), "codex-session"),
            (
                "opencode",
                ProviderCode::OpenCode,
                Some("ollama/qwen2.5-coder:14b"),
                "opencode-session",
            ),
            (
                "cursor",
                ProviderCode::Cursor,
                Some("gpt-5"),
                "cursor-session",
            ),
            (
                "ollama",
                ProviderCode::Ollama,
                Some("qwen3:8b"),
                "ollama-session",
            ),
        ];
        let verbatim = PathBuf::from(r"\\?\C:\Users\kaoru\OneDrive\桌面\test");
        let external = r"C:\Users\kaoru\OneDrive\桌面\test";

        for (name, provider, model, provider_session_id) in cases {
            let temp = tempfile::tempdir().unwrap();
            let thread_id = format!("thread_externalize_resume_{name}");
            let mut runtime = runtime_with_provider_thread(
                temp.path(),
                &thread_id,
                provider.clone(),
                Some(provider_session_id.into()),
                model.map(str::to_string),
            );
            runtime
                .thread_manager
                .thread_mut(&thread_id)
                .unwrap()
                .workspace_path = verbatim.clone();

            let start = runtime
                .begin_send_text(SendTextInput {
                    thread_id,
                    message: "continue".into(),
                })
                .unwrap();

            assert_eq!(start.command.cwd, verbatim);
            assert_eq!(
                env_value(&start.command, "PEDELEC_WORKSPACE_PATH"),
                Some(external)
            );
            assert_eq!(env_value(&start.command, "PEDELEC_SANDBOX_PATH"), None);
            assert!(!start.command.args.iter().any(|arg| arg.contains(r"\\?\")));
            assert!(!start.command.prompt.contains(r"\\?\"));

            let path_flag = match provider {
                ProviderCode::Codex => "--cd",
                ProviderCode::OpenCode => "--dir",
                ProviderCode::Cursor => "--workspace",
                ProviderCode::Ollama => "--sandbox",
                ProviderCode::Antigravity | ProviderCode::Claude => unreachable!(),
            };
            assert!(start
                .command
                .args
                .windows(2)
                .any(|args| args == [path_flag, external]));
        }
    }

    #[test]
    fn antigravity_new_command_passes_prompt_as_an_argument() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_new",
            ProviderCode::Antigravity,
            None,
            Some("antigravity-2.5-pro".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_antigravity_new".into(),
                message: "hello".into(),
            })
            .unwrap();

        assert_eq!(start.command.program, "agy");
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--model", "antigravity-2.5-pro"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--output-format", "stream-json"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1] == start.command.prompt));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--mode", "accept-edits"]));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "--disable-slash-commands"));
        assert!(!start.command.args.iter().any(|arg| arg == "--conversation"));
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == "User message: hello"));
        assert_provider_instruction_present(&start.command);
        assert!(start.command.prompt.ends_with("hello"));
        assert!(start.command.stdin.is_empty());
    }

    #[test]
    fn antigravity_resume_passes_prompt_and_uses_explicit_conversation_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_resume",
            ProviderCode::Antigravity,
            Some("123e4567-e89b-12d3-a456-426614174000".into()),
            None,
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_antigravity_resume".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| { args == ["--conversation", "123e4567-e89b-12d3-a456-426614174000"] }));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--output-format", "stream-json"]));
        assert!(!start.command.args.iter().any(|arg| arg == "latest"));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1] == "continue"));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "--disable-slash-commands"));
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == "User message: continue"));
        assert_eq!(start.command.prompt, "continue");
        assert!(start.command.stdin.is_empty());
        assert_provider_instruction_absent(&start.command);
    }

    #[test]
    fn antigravity_run_preserves_multiline_markdown_json_and_quotes_in_prompt_argument() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_special",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let message =
            "line 1\n\n```json\n{\"quote\":\"hello \\\"world\\\"\",\"markdown\":\"**bold**\"}\n```";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_antigravity_special".into(),
                message: message.into(),
            })
            .unwrap();

        assert!(start.command.stdin.is_empty());
        assert!(start.command.prompt.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1].ends_with(message)));
    }

    #[test]
    fn antigravity_resume_preserves_multiline_markdown_json_and_quotes_in_prompt_argument() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_resume_special",
            ProviderCode::Antigravity,
            Some("123e4567-e89b-12d3-a456-426614174000".into()),
            Some("antigravity-2.5-pro".into()),
        );
        let message =
            "line 1\n\n```json\n{\"quote\":\"hello \\\"world\\\"\",\"markdown\":\"**bold**\"}\n```";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_antigravity_resume_special".into(),
                message: message.into(),
            })
            .unwrap();

        assert!(start.command.stdin.is_empty());
        assert_eq!(start.command.prompt, message);
        assert_provider_instruction_absent(&start.command);
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--conversation", "123e4567-e89b-12d3-a456-426614174000"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--model", "antigravity-2.5-pro"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1] == message));
    }

    #[test]
    fn antigravity_run_allows_prompt_at_utf16_boundary_and_counts_emoji_as_two_units() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_prompt_boundary";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        let message = antigravity_run_message_with_final_utf16_length(
            &runtime,
            thread_id,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS,
            true,
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap();

        assert_eq!(
            start.command.prompt.encode_utf16().count(),
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS
        );
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1] == start.command.prompt));
    }

    #[test]
    fn antigravity_run_rejects_prompt_over_utf16_limit_and_keeps_thread_idle() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_prompt_too_large";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        let message = antigravity_run_message_with_final_utf16_length(
            &runtime,
            thread_id,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1,
            true,
        );

        let error = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap_err();

        assert_antigravity_prompt_too_large_error(
            error,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1,
        );
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
    }

    #[test]
    fn antigravity_run_checks_final_prompt_instead_of_raw_user_message() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_final_prompt_limit";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        let message = antigravity_run_message_with_final_utf16_length(
            &runtime,
            thread_id,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1,
            false,
        );
        assert!(message.encode_utf16().count() < ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS);

        let error = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap_err();

        assert_antigravity_prompt_too_large_error(
            error,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1,
        );
    }

    #[test]
    fn antigravity_resume_allows_prompt_at_utf16_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_resume_prompt_boundary";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            Some("conversation_boundary".into()),
            None,
        );
        let message = format!(
            "{}😀",
            "a".repeat(ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS - 2)
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap();

        assert_eq!(
            start.command.prompt.encode_utf16().count(),
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS
        );
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-p" && args[1] == start.command.prompt));
    }

    #[test]
    fn antigravity_resume_rejects_prompt_over_utf16_limit() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_resume_prompt_too_large";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            Some("conversation_too_large".into()),
            None,
        );
        let message = "a".repeat(ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1);

        let error = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap_err();

        assert_antigravity_prompt_too_large_error(
            error,
            ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1,
        );
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
    }

    #[test]
    fn antigravity_prepare_rejects_an_overlong_final_prompt_before_running() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_prepare_prompt_too_large";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        let oversized_guidance = "g".repeat(ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1);
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_skills_input(Some(&CreateThreadSkillsInput {
                guidance: oversized_guidance,
                tools: vec![],
            }))
            .unwrap(),
        );

        let error = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap_err();

        assert!(error.code == error_codes::PROVIDER_PROMPT_TOO_LARGE);
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
    }

    #[test]
    fn codex_large_stdin_prompt_is_not_rejected_by_antigravity_limit() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_codex_large_prompt";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        let message = "c".repeat(ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS + 1);

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message,
            })
            .unwrap();

        assert!(
            start.command.stdin.encode_utf16().count() > ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS
        );
        assert_eq!(start.command.prompt, start.command.stdin);
        assert_eq!(start.command.args.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn opencode_new_command_uses_json_dir_model_and_stdin_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_opencode_new",
            ProviderCode::OpenCode,
            None,
            Some("ollama/qwen2.5-coder:14b".into()),
        );
        let message = "line 1\n{\"quote\":\"hello \\\"world\\\"\"}\n中文";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_opencode_new".into(),
                message: message.into(),
            })
            .unwrap();

        let workspace_path = temp.path().join("workspace").join("thread_opencode_new");
        assert_eq!(start.command.program, "opencode");
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| { args == ["--agent", PEDELEC_OPENCODE_AGENT] }));
        assert_eq!(start.command.args.last().map(String::as_str), Some("-"));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--dir", workspace_path.to_str().unwrap()]));
        let config: Value =
            serde_json::from_str(env_value(&start.command, OPENCODE_CONFIG_CONTENT_ENV).unwrap())
                .unwrap();
        assert_eq!(config["agent"][PEDELEC_OPENCODE_AGENT]["mode"], "primary");
        assert!(config["agent"][PEDELEC_OPENCODE_AGENT]["prompt"]
            .as_str()
            .unwrap()
            .contains("Pedelec is the host application"));
        assert_eq!(start.command.cwd, workspace_path);
        assert!(start.command.stdin.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == &start.command.stdin));
        assert_env(&start.command, "PEDELEC_PROVIDER", "opencode");
        assert_opencode_native_skills_policy(&start.command);
    }

    #[test]
    fn opencode_resume_uses_explicit_session_id_and_model() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_opencode_resume",
            ProviderCode::OpenCode,
            Some("ses_123".into()),
            Some("anthropic/claude-sonnet-4".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_opencode_resume".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--session", "ses_123"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--model", "anthropic/claude-sonnet-4"]));
        assert!(!start.command.args.iter().any(|arg| arg == "--last"));
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert_provider_instruction_absent(&start.command);
        assert_opencode_native_skills_policy(&start.command);
    }

    #[test]
    fn opencode_permission_overlay_only_denies_skill_and_preserves_existing_permissions() {
        let merged = build_opencode_permission_overlay(Some(
            r#"{"read":"allow","edit":"allow","bash":"ask","skill":{"*":"allow"}}"#,
        ))
        .unwrap();
        let merged: Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(merged.get("read").and_then(Value::as_str), Some("allow"));
        assert_eq!(merged.get("edit").and_then(Value::as_str), Some("allow"));
        assert_eq!(merged.get("bash").and_then(Value::as_str), Some("ask"));
        assert_eq!(merged.get("skill").and_then(Value::as_str), Some("deny"));
        assert_ne!(merged.get("read").and_then(Value::as_str), Some("deny"));
        assert_ne!(merged.get("edit").and_then(Value::as_str), Some("deny"));

        let defaults: Value =
            serde_json::from_str(&build_opencode_permission_overlay(None).unwrap()).unwrap();
        assert_eq!(defaults.get("skill").and_then(Value::as_str), Some("deny"));
        assert!(build_opencode_permission_overlay(Some("not-json")).is_none());
        assert!(build_opencode_permission_overlay(Some(r#"["allow"]"#)).is_none());
    }

    #[test]
    fn opencode_bootstrap_merges_inline_config_and_rejects_invalid_json() {
        let command = CommandSpec {
            program: "opencode".into(),
            args: vec![],
            cwd: PathBuf::from("."),
            env: vec![(
                OPENCODE_CONFIG_CONTENT_ENV.into(),
                r#"{"theme":"dark","agent":{"custom":{"mode":"subagent"}}}"#.into(),
            )],
            prompt: String::new(),
            stdin: String::new(),
        };
        let merged: Value =
            serde_json::from_str(&merge_opencode_runtime_agent_config(&command).unwrap()).unwrap();
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["agent"]["custom"]["mode"], "subagent");
        assert_eq!(merged["agent"][PEDELEC_OPENCODE_AGENT]["mode"], "primary");
        assert!(merged["agent"][PEDELEC_OPENCODE_AGENT]["prompt"]
            .as_str()
            .unwrap()
            .contains("Pedelec is the host application"));

        let invalid = CommandSpec {
            env: vec![(OPENCODE_CONFIG_CONTENT_ENV.into(), "not-json".into())],
            ..command
        };
        assert_eq!(
            merge_opencode_runtime_agent_config(&invalid)
                .unwrap_err()
                .code,
            error_codes::PROVIDER_BOOTSTRAP_CONFIG_INVALID
        );
    }

    #[test]
    fn opencode_agent_selector_has_pedelec_precedence_over_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_opencode_agent_precedence",
            ProviderCode::OpenCode,
            None,
            Some("model".into()),
        );
        runtime
            .thread_manager
            .thread_mut("thread_opencode_agent_precedence")
            .unwrap()
            .effort_args
            .extend([
                "--agent".into(),
                "user-agent".into(),
                "--agent=other".into(),
            ]);

        let command = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_opencode_agent_precedence".into(),
                message: "hello".into(),
            })
            .unwrap()
            .command;
        assert_eq!(
            command
                .args
                .windows(2)
                .filter(|args| args[0] == "--agent")
                .count(),
            1
        );
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--agent", PEDELEC_OPENCODE_AGENT]));
    }

    #[test]
    fn antigravity_supported_version_ensures_reserved_workspace_agent_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_agent_supported";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        runtime.provider_scan.insert(
            ProviderCode::Antigravity,
            ProviderCli {
                path: None,
                version: Some(ProviderVersion(vec![1, 1, 6])),
                error: None,
                bootstrap_capabilities: Some(ProviderBootstrapCapabilities {
                    privileged_bootstrap: ProviderBootstrapMode::AntigravityWorkspaceAgent,
                }),
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            },
        );
        let workspace = runtime.thread_workspace_path(thread_id).unwrap();
        let agent_path = workspace
            .join(PEDELEC_ANTIGRAVITY_AGENT_DIR)
            .join(PEDELEC_ANTIGRAVITY_AGENT_FILE);
        fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        fs::write(&agent_path, "stale").unwrap();

        let first = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "hello".into(),
            })
            .unwrap()
            .command;
        let expected = fs::read_to_string(&agent_path).unwrap();
        assert!(expected.contains("name: pedelec-runtime"));
        assert!(expected.contains("mainAgent: true"));
        assert!(expected.contains("subagent: false"));
        assert!(expected.contains("Pedelec is the host application"));
        assert!(first
            .args
            .windows(2)
            .any(|args| args == ["--agent", PEDELEC_OPENCODE_AGENT]));

        runtime.thread_manager.thread_mut(thread_id).unwrap().status = ThreadStatus::Idle;
        let prepare = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap()
            .command
            .unwrap();
        assert!(prepare
            .args
            .windows(2)
            .any(|args| args == ["--agent", PEDELEC_OPENCODE_AGENT]));
        runtime.thread_manager.thread_mut(thread_id).unwrap().status = ThreadStatus::Idle;
        runtime
            .thread_manager
            .provider_state_mut(thread_id)
            .unwrap()
            .provider_session_id = Some("agy-session".into());
        let second = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "again".into(),
            })
            .unwrap()
            .command;
        assert_eq!(fs::read_to_string(&agent_path).unwrap(), expected);
        assert_eq!(
            second
                .args
                .windows(2)
                .filter(|args| args[0] == "--agent")
                .count(),
            1
        );
    }

    #[test]
    fn antigravity_supported_version_writes_agent_into_custom_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let custom_workspace = temp.path().join("custom-project");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };
        let thread_id = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Antigravity,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom_workspace.clone(),
                }),
            })
            .unwrap()
            .thread_id;
        runtime.provider_scan.insert(
            ProviderCode::Antigravity,
            ProviderCli {
                path: None,
                version: Some(ProviderVersion(vec![1, 1, 6])),
                error: None,
                bootstrap_capabilities: Some(ProviderBootstrapCapabilities {
                    privileged_bootstrap: ProviderBootstrapMode::AntigravityWorkspaceAgent,
                }),
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            },
        );

        let command = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.clone(),
                message: "hello".into(),
            })
            .unwrap()
            .command;
        let thread = runtime.thread_manager.thread(&thread_id).unwrap();
        let agent_path = thread
            .workspace_path
            .join(PEDELEC_ANTIGRAVITY_AGENT_DIR)
            .join(PEDELEC_ANTIGRAVITY_AGENT_FILE);

        assert_eq!(
            thread.workspace_path,
            custom_workspace.canonicalize().unwrap()
        );
        assert!(agent_path.is_file());
        assert!(fs::read_to_string(agent_path)
            .unwrap()
            .contains("Pedelec is the host application"));
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--agent", PEDELEC_OPENCODE_AGENT]));
    }

    #[test]
    fn antigravity_old_version_falls_back_without_workspace_agent_asset() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_antigravity_agent_old";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        runtime.provider_scan.insert(
            ProviderCode::Antigravity,
            ProviderCli {
                path: None,
                version: Some(ProviderVersion(vec![1, 1, 5])),
                error: None,
                bootstrap_capabilities: None,
                app_server_capability: None,
                acp_capability: None,
                stream_json_capability: None,
            },
        );
        let command = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "hello".into(),
            })
            .unwrap()
            .command;
        assert!(!command.args.iter().any(|arg| arg == "--agent"));
        assert!(!runtime
            .thread_workspace_path(thread_id)
            .unwrap()
            .join(PEDELEC_ANTIGRAVITY_AGENT_DIR)
            .exists());
        assert!(command.prompt.contains("[Pedelec Host Bootstrap]"));
        assert!(command.prompt.contains("[Pedelec Host Context]"));
        assert!(command.prompt.contains("[User Message]"));
        assert!(command.prompt.contains("PEDELEC_PREPARED"));
        assert!(!command.prompt.contains("[Pedelec Runtime Rules]"));

        runtime.thread_manager.thread_mut(thread_id).unwrap().status = ThreadStatus::Idle;
        let prepare = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap()
            .command
            .unwrap();
        assert!(!prepare.args.iter().any(|arg| arg == "--agent"));
        assert!(prepare.prompt.contains("[Pedelec Host Bootstrap]"));
        assert!(prepare.prompt.contains("[Pedelec Host Context]"));
        assert!(prepare.prompt.contains("PEDELEC_PREPARED"));
        assert!(prepare.prompt.ends_with("[Session Preparation]"));
        assert!(!prepare.prompt.contains("After preparation is complete"));
    }

    #[test]
    fn antigravity_sessions_sharing_a_workspace_can_ensure_the_agent_asset() {
        let temp = tempfile::tempdir().unwrap();
        let first_id = "thread_antigravity_shared_first";
        let second_id = "thread_antigravity_shared_second";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            first_id,
            ProviderCode::Antigravity,
            None,
            None,
        );
        let first = runtime.thread_manager.thread(first_id).unwrap().clone();
        let mut second = first.clone();
        second.thread_id = second_id.into();
        second.status = ThreadStatus::Idle;
        second.process_id = None;
        runtime.thread_manager.insert_thread(
            second,
            ProviderAdapterState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime.tool_registry.insert(
            second_id,
            ToolRegistry::from_skills_input(Some(&sample_skills_input())).unwrap(),
        );

        runtime
            .begin_send_text(SendTextInput {
                thread_id: first_id.into(),
                message: "first".into(),
            })
            .unwrap();
        runtime.thread_manager.thread_mut(first_id).unwrap().status = ThreadStatus::Idle;
        runtime
            .begin_send_text(SendTextInput {
                thread_id: second_id.into(),
                message: "second".into(),
            })
            .unwrap();

        let agent_path = first
            .workspace_path
            .join(PEDELEC_ANTIGRAVITY_AGENT_DIR)
            .join(PEDELEC_ANTIGRAVITY_AGENT_FILE);
        assert!(agent_path.is_file());
        assert!(fs::read_to_string(agent_path)
            .unwrap()
            .contains("Pedelec is the host application"));
    }

    #[test]
    fn opencode_parser_updates_session_and_assistant_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_opencode_parse",
            ProviderCode::OpenCode,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_opencode_parse");

        runtime.emit_provider_stdout(
            "thread_opencode_parse",
            r#"{"type":"session.created","id":"ses_123"}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_opencode_parse",
            r#"{"type":"assistant.text.delta","delta":"hello"}"#.to_string() + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_opencode_parse")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("ses_123")
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
    }

    #[test]
    fn opencode_invalid_json_emits_structured_error_without_panic() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_opencode_invalid_json",
            ProviderCode::OpenCode,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_opencode_invalid_json");

        runtime.emit_provider_stdout("thread_opencode_invalid_json", "{not-json}\n".into());

        assert_eq!(
            runtime.thread_status("thread_opencode_invalid_json"),
            Some(ThreadStatus::Error)
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error {
                source: ThreadErrorSource::Provider { provider: ProviderCode::OpenCode }, error, ..
            } if error.code == error_codes::PROVIDER_COMMAND_FAILED)));
    }

    #[test]
    fn cursor_new_command_uses_cursor_agent_workspace_model_json_and_stdin_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_cursor_new",
            ProviderCode::Cursor,
            None,
            Some("gpt-5".into()),
        );
        let message = "line 1\n{\"quote\":\"hello \\\"world\\\"\"}\n中文";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_cursor_new".into(),
                message: message.into(),
            })
            .unwrap();

        let workspace_path = temp.path().join("workspace").join("thread_cursor_new");
        assert_eq!(start.command.program, "cursor-agent");
        assert_eq!(
            start.command.args,
            vec![
                "--workspace",
                workspace_path.to_str().unwrap(),
                "--output-format",
                "stream-json",
                "--force",
                "--trust",
                "--model",
                "gpt-5",
            ]
        );
        assert_eq!(start.command.cwd, workspace_path);
        assert!(start.command.stdin.ends_with(message));
        assert!(start.command.prompt.contains("[Pedelec Host Bootstrap]"));
        assert!(start.command.prompt.contains("[Pedelec Host Context]"));
        assert!(start.command.prompt.contains("[User Message]"));
        assert!(start.command.prompt.contains("PEDELEC_PREPARED"));
        assert!(!start.command.prompt.contains("[Pedelec Runtime Rules]"));
        assert!(!workspace_path.join(".cursor").exists());
        assert!(!workspace_path.join("AGENTS.md").exists());
        assert!(!workspace_path.join("CLAUDE.md").exists());
        assert_provider_instruction_present(&start.command);
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == &start.command.stdin));
        assert_env(&start.command, "PEDELEC_PROVIDER", "cursor");
    }

    #[test]
    fn cursor_resume_uses_explicit_session_id_and_omits_provider_instruction() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_cursor_resume",
            ProviderCode::Cursor,
            Some("cur_123".into()),
            Some("gpt-5".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_cursor_resume".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert_eq!(start.command.program, "cursor-agent");
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--resume", "cur_123"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--model", "gpt-5"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--output-format", "stream-json"]));
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert!(!start.command.prompt.contains("[Pedelec Host Bootstrap]"));
        assert!(!start.command.prompt.contains("[Pedelec Host Context]"));
        assert_provider_instruction_absent(&start.command);
    }

    #[test]
    fn cursor_prepare_uses_fallback_bootstrap_and_short_prepare_task() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_cursor_prepare",
            ProviderCode::Cursor,
            None,
            None,
        );

        let command = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: "thread_cursor_prepare".into(),
            })
            .unwrap()
            .command
            .unwrap();

        assert!(command.prompt.contains("[Pedelec Host Bootstrap]"));
        assert!(command.prompt.contains("[Pedelec Host Context]"));
        assert!(command.prompt.contains("PEDELEC_PREPARED"));
        assert!(!command.prompt.contains("[Pedelec Runtime Rules]"));
        assert!(command.prompt.ends_with("[Session Preparation]"));
        assert!(!command.prompt.contains("After preparation is complete"));
        assert!(!runtime
            .thread_workspace_path("thread_cursor_prepare")
            .unwrap()
            .join(".cursor")
            .exists());
        assert!(!runtime
            .thread_workspace_path("thread_cursor_prepare")
            .unwrap()
            .join("AGENTS.md")
            .exists());
        assert!(!runtime
            .thread_workspace_path("thread_cursor_prepare")
            .unwrap()
            .join("CLAUDE.md")
            .exists());
    }

    #[test]
    fn cursor_parser_updates_session_and_assistant_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_cursor_parse",
            ProviderCode::Cursor,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_cursor_parse");

        runtime.emit_provider_stdout(
            "thread_cursor_parse",
            r#"{"type":"assistant","subtype":"delta","text":"hello","session_id":"cur_123"}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_cursor_parse",
            r#"{"message":{"type":"assistant","content":[{"type":"text","text":" world"}]},"conversationId":"cur_123"}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_cursor_parse",
            r#"{"role":"assistant","text":"role only"}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_cursor_parse",
            r#"{"type":"text","text":"text only"}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_cursor_parse",
            r#"{"subtype":"delta","text":"delta only"}"#.to_string() + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_cursor_parse")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("cur_123")
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "world")
        ));
        assert!(events.iter().all(
            |event| !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "role only" || text == "text only" || text == "delta only")
        ));
    }

    #[test]
    fn cursor_invalid_json_emits_structured_error_without_panic() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_cursor_invalid_json",
            ProviderCode::Cursor,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_cursor_invalid_json");

        runtime.emit_provider_stdout("thread_cursor_invalid_json", "{not-json}\n".into());

        assert_eq!(
            runtime.thread_status("thread_cursor_invalid_json"),
            Some(ThreadStatus::Error)
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error {
                source: ThreadErrorSource::Provider { provider: ProviderCode::Cursor }, error, ..
            } if error.code == error_codes::PROVIDER_COMMAND_FAILED)));
    }

    #[test]
    fn claude_new_command_uses_stream_json_permissions_model_and_stdin_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_new",
            ProviderCode::Claude,
            None,
            Some("sonnet".into()),
        );
        let message = "line 1\n{\"quote\":\"hello \\\"world\\\"\"}\n中文";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_claude_new".into(),
                message: message.into(),
            })
            .unwrap();

        let workspace_path = temp.path().join("workspace").join("thread_claude_new");
        assert_eq!(start.command.program, "claude");
        assert!(start.command.args.windows(2).any(|args| {
            args[0] == "--append-system-prompt" && args[1] == build_pedelec_bootstrap_instruction()
        }));
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == "--system-prompt"));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--model", "sonnet"]));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "--disable-slash-commands"));
        assert_eq!(start.command.cwd, workspace_path);
        assert!(start.command.stdin.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(!start.command.args.iter().any(|arg| arg == "--session-id"));
        assert!(!start.command.args.iter().any(|arg| arg == "--continue"));
        assert!(!start.command.args.iter().any(|arg| arg == "--add-dir"));
        assert_env(&start.command, "PEDELEC_PROVIDER", "claude");
    }

    #[test]
    fn claude_resume_uses_explicit_session_id_and_omits_provider_instruction() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_resume",
            ProviderCode::Claude,
            Some("4fab02ca-67b9-489d-8b89-0b1f0b9550e6".into()),
            Some("sonnet".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_claude_resume".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert!(start.command.args.windows(2).any(|args| {
            args[0] == "--append-system-prompt" && args[1] == build_pedelec_bootstrap_instruction()
        }));
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == "--system-prompt"));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["--resume", "4fab02ca-67b9-489d-8b89-0b1f0b9550e6"]));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "--disable-slash-commands"));
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert_provider_instruction_absent(&start.command);
        assert!(!start.command.args.iter().any(|arg| arg == "--continue"));
        assert!(!start.command.args.iter().any(|arg| arg == "--session-id"));
        assert!(!start.command.args.iter().any(|arg| arg == "--add-dir"));
    }

    #[test]
    fn claude_parser_updates_session_from_init_and_assistant_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_parse",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_claude_parse");

        runtime.emit_provider_stdout(
            "thread_claude_parse",
            r#"{"type":"system","subtype":"init","cwd":"C:\\Users\\kaoru","session_id":"4fab02ca-67b9-489d-8b89-0b1f0b9550e6","tools":[]}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_claude_parse",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_claude_parse",
            r#"{"type":"user","session_id":"do-not-use","message":{"role":"user","content":[{"type":"text","text":"ignore"}]}}"#
                .to_string()
                + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_claude_parse")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("4fab02ca-67b9-489d-8b89-0b1f0b9550e6")
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
        assert!(events.iter().all(
            |event| !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "ignore")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::RawStdout { text, .. } if text.contains("do-not-use"))
        ));
    }

    #[test]
    fn claude_stdout_filter_drops_large_chunked_user_tool_result_and_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_tool_result",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_claude_tool_result");
        let payload = "BASE64_PAYLOAD_MARKER".repeat(8 * 1024);
        let assistant =
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"after image"}]}}"#
                .to_string()
                + "\n";

        runtime.emit_provider_stdout("thread_claude_tool_result", r#"{"ty"#.into());
        runtime.emit_provider_stdout(
            "thread_claude_tool_result",
            r#"pe":"user","message":{"role":"user","content":[{"ty"#.into(),
        );
        runtime.emit_provider_stdout("thread_claude_tool_result", r#"pe":"tool_res"#.into());
        runtime.emit_provider_stdout(
            "thread_claude_tool_result",
            r#"ult","content":[{"type":"image","source":{"type":"base64","data":""#.into(),
        );
        let midpoint = payload.len() / 2;
        runtime.emit_provider_stdout("thread_claude_tool_result", payload[..midpoint].to_string());
        runtime.emit_provider_stdout(
            "thread_claude_tool_result",
            payload[midpoint..].to_string() + r#""}}]}]}]}}"# + "\r\n" + &assistant,
        );

        assert_eq!(
            runtime.thread_status("thread_claude_tool_result"),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(
            runtime
                .provider_state("thread_claude_tool_result")
                .unwrap()
                .provider_session_id,
            None
        );
        let events = collect_available_core_events(&event_rx);
        let raw_stdout = events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::RawStdout { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(raw_stdout, assistant);
        assert!(!raw_stdout.contains("BASE64_PAYLOAD_MARKER"));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "after image")
        ));
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn claude_stdout_filter_redacts_large_chunked_thinking_signature_and_keeps_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_thinking_signature",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_thinking_signature");
        let payload = "SIGNATURE_PAYLOAD_MARKER".repeat(8 * 1024);
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"","signature":"{payload}"}},{{"type":"text","text":"visible answer"}}]}}}}"#
        ) + "\n";

        emit_claude_in_chunks(&mut runtime, "thread_claude_thinking_signature", &line, 257);

        assert_eq!(
            runtime.thread_status("thread_claude_thinking_signature"),
            Some(ThreadStatus::Idle)
        );
        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert!(!raw_stdout.contains("SIGNATURE_PAYLOAD_MARKER"));
        assert!(raw_stdout.contains(r#""signature":"[omitted]""#));
        assert!(serde_json::from_str::<serde_json::Value>(raw_stdout.trim()).is_ok());
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "visible answer")
        ));
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn claude_stdout_filter_redacts_signature_key_split_across_chunks() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_signature_key_split",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_signature_key_split");
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","signature":"SIGNATURE_KEY_SPLIT_MARKER","thinking":""}]}}"#.to_string() + "\n";
        let key_start = line.find(r#""signature""#).unwrap();
        let split = key_start + 4;
        runtime.emit_provider_stdout(
            "thread_claude_signature_key_split",
            line[..split].to_string(),
        );
        runtime.emit_provider_stdout(
            "thread_claude_signature_key_split",
            line[split..].to_string(),
        );

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert!(!raw_stdout.contains("SIGNATURE_KEY_SPLIT_MARKER"));
        assert!(raw_stdout.contains(r#""signature":"[omitted]""#));
        assert!(serde_json::from_str::<serde_json::Value>(raw_stdout.trim()).is_ok());
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn claude_stdout_filter_redacts_signature_when_value_quotes_split_across_chunks() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_signature_value_split",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_signature_value_split");
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","signature":"SIGNATURE_VALUE_SPLIT_MARKER","thinking":""}]}}"#.to_string() + "\n";
        let key_start = line.find(r#""signature":"#).unwrap();
        let value_quote = key_start + r#""signature":"#.len();
        let payload_start = value_quote + 1;
        let payload_end = payload_start + "SIGNATURE_VALUE_SPLIT_MARKER".len();
        for part in [
            &line[..value_quote],
            &line[value_quote..payload_end],
            &line[payload_end..],
        ] {
            runtime
                .emit_provider_stdout("thread_claude_signature_value_split", (*part).to_string());
        }

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert!(!raw_stdout.contains("SIGNATURE_VALUE_SPLIT_MARKER"));
        assert!(raw_stdout.contains(r#""signature":"[omitted]""#));
        assert!(serde_json::from_str::<serde_json::Value>(raw_stdout.trim()).is_ok());
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn claude_stdout_filter_handles_escaped_signature_characters() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_signature_escapes",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_signature_escapes");
        let signature = r#"prefix \ and " quote SIGNATURE_ESCAPED_MARKER suffix"#;
        let encoded_signature = serde_json::to_string(signature).unwrap();
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"thinking","signature":{encoded_signature},"thinking":""}},{{"type":"text","text":"visible after escape"}}]}}}}"#
        ) + "\n";
        emit_claude_in_chunks(&mut runtime, "thread_claude_signature_escapes", &line, 3);

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert!(!raw_stdout.contains("SIGNATURE_ESCAPED_MARKER"));
        assert!(raw_stdout.contains(r#""signature":"[omitted]""#));
        assert!(serde_json::from_str::<serde_json::Value>(raw_stdout.trim()).is_ok());
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "visible after escape")
        ));
    }

    #[test]
    fn claude_stdout_filter_preserves_literal_signature_text_and_non_thinking_signature() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_signature_false_positives",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_signature_false_positives");
        let literal_text = r#"example: "signature":"foo""#;
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":{}}},{{"type":"text","signature":"SHOULD_BE_KEPT","text":"hello"}}]}}}}"#,
            serde_json::to_string(literal_text).unwrap()
        ) + "\n";
        runtime.emit_provider_stdout("thread_claude_signature_false_positives", line.clone());

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert_eq!(raw_stdout, line);
        assert!(raw_stdout.contains("SHOULD_BE_KEPT"));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == literal_text)
        ));
    }

    #[test]
    fn claude_stdout_filter_redacts_multiple_events_in_one_chunk() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_multiple_signature_events",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_multiple_signature_events");
        let first = r#"{"type":"assistant","message":{"content":[{"type":"thinking","signature":"SIGNATURE_FIRST_MARKER","thinking":""}]}}"#;
        let second =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"second event"}]}}"#;
        runtime.emit_provider_stdout(
            "thread_claude_multiple_signature_events",
            format!("{first}\n{second}\n"),
        );

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = claude_raw_stdout(&events);
        assert!(!raw_stdout.contains("SIGNATURE_FIRST_MARKER"));
        assert!(raw_stdout.contains(r#""signature":"[omitted]""#));
        assert!(raw_stdout.contains(second));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "second event")
        ));
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn claude_stdout_filter_handles_multiple_crlf_events_in_one_chunk() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_multi_filter",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_claude_multi_filter");
        let init = r#"{"type":"system","subtype":"init","session_id":"4fab02ca-67b9-489d-8b89-0b1f0b9550e6"}"#;
        let dropped =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"DROP_ME"}]}}"#;
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"kept"}]}}"#;

        runtime.emit_provider_stdout(
            "thread_claude_multi_filter",
            format!("{init}\r\n{dropped}\r\n{assistant}\r\n"),
        );

        assert_eq!(
            runtime
                .provider_state("thread_claude_multi_filter")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("4fab02ca-67b9-489d-8b89-0b1f0b9550e6")
        );
        let events = collect_available_core_events(&event_rx);
        let raw_stdout = events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::RawStdout { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(raw_stdout, format!("{init}\r\n{assistant}\r\n"));
        assert!(!raw_stdout.contains("DROP_ME"));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "kept")
        ));
    }

    #[test]
    fn claude_stdout_filter_keeps_user_and_non_user_false_positives() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_filter_false_positive",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_claude_filter_false_positive");
        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"ordinary"}]}}"#;
        let assistant_text = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"literal \"type\":\"tool_result\""}]}}"#;
        let assistant_structured = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_result","text":"structured but kept"}]}}"#;
        let input = format!("{user}\n{assistant_text}\n{assistant_structured}\n");

        runtime.emit_provider_stdout("thread_claude_filter_false_positive", input.clone());

        let events = collect_available_core_events(&event_rx);
        let raw_stdout = events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::RawStdout { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(raw_stdout, input);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "literal \"type\":\"tool_result\"")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "structured but kept")
        ));
    }

    #[test]
    fn claude_stdout_filter_does_not_affect_stderr_or_other_providers() {
        let temp = tempfile::tempdir().unwrap();
        let matching = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"keep outside claude stdout"}]}}"#
            .to_string()
            + "\n";
        let mut claude = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_stderr_filter",
            ProviderCode::Claude,
            None,
            None,
        );
        let claude_rx = claude.event_bus.subscribe("thread_claude_stderr_filter");
        claude.emit_provider_stderr("thread_claude_stderr_filter", matching.clone());
        let claude_events = collect_available_core_events(&claude_rx);
        assert!(claude_events.iter().any(
            |event| matches!(event, ThreadEvent::RawStderr { text, .. } if text == &matching)
        ));

        let mut codex = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_filter_passthrough",
            ProviderCode::Codex,
            None,
            None,
        );
        let codex_rx = codex.event_bus.subscribe("thread_codex_filter_passthrough");
        codex.emit_provider_stdout("thread_codex_filter_passthrough", matching.clone());
        let codex_events = collect_available_core_events(&codex_rx);
        assert!(codex_events.iter().any(
            |event| matches!(event, ThreadEvent::RawStdout { text, .. } if text == &matching)
        ));
    }

    #[test]
    fn claude_invalid_json_emits_structured_error_without_panic() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_claude_invalid_json",
            ProviderCode::Claude,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_claude_invalid_json");

        runtime.emit_provider_stdout("thread_claude_invalid_json", "{not-json}\n".into());

        assert_eq!(
            runtime.thread_status("thread_claude_invalid_json"),
            Some(ThreadStatus::Error)
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::RawStdout { text, .. } if text == "{not-json}\n")
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error {
                source: ThreadErrorSource::Provider { provider: ProviderCode::Claude }, error, ..
            } if error.code == error_codes::PROVIDER_COMMAND_FAILED)));
    }

    #[test]
    fn ollama_new_command_uses_pedelec_agent_model_workspace_and_stdin_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_ollama_new",
            ProviderCode::Ollama,
            None,
            Some("qwen3-14b-32k:latest".into()),
        );
        let message = "line 1\n{\"quote\":\"hello \\\"world\\\"\"}\n中文";

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_ollama_new".into(),
                message: message.into(),
            })
            .unwrap();

        let workspace_path = temp.path().join("workspace").join("thread_ollama_new");
        assert_eq!(start.command.program, "pedelec-agent");
        assert_eq!(
            start.command.args,
            vec![
                "--provider",
                "ollama",
                "--model",
                "qwen3-14b-32k:latest",
                "--sandbox",
                workspace_path.to_str().unwrap(),
            ]
        );
        assert_eq!(start.command.cwd, workspace_path);
        assert!(start.command.stdin.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(!start.command.args.iter().any(|arg| arg == message));
        assert!(!start.command.args.iter().any(|arg| arg == "--session-id"));
        assert_eq!(env_value(&start.command, "PEDELEC_THREAD_ID"), None);
        assert_env(&start.command, "PEDELEC_PROVIDER", "ollama");
        assert_env(&start.command, "OLLAMA_API_KEY", "ollama_test_key");
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == "ollama_test_key"));
        assert!(!start.command.prompt.contains("ollama_test_key"));
        assert!(env_value(&start.command, "PATH")
            .unwrap()
            .contains(".pedelec"));
    }

    #[test]
    fn ollama_provider_command_started_event_does_not_include_api_key() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_ollama_command_event",
            ProviderCode::Ollama,
            None,
            Some("qwen3:8b".into()),
        );
        let event_rx = runtime.event_bus.subscribe("thread_ollama_command_event");
        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_ollama_command_event".into(),
                message: "hello".into(),
            })
            .unwrap();

        runtime.emit_provider_command_started("thread_ollama_command_event", 123, &start.command);
        let events = collect_available_core_events(&event_rx);
        let payload = serde_json::to_string(&events).unwrap();

        assert!(payload.contains("pedelec-agent"));
        assert!(!payload.contains("ollama_test_key"));
        assert!(!payload.contains("OLLAMA_API_KEY"));
    }

    #[cfg(windows)]
    #[test]
    fn provider_command_started_event_externalizes_verbatim_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_externalize_command_event";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Ollama,
            None,
            Some("qwen3:8b".into()),
        );
        let verbatim = PathBuf::from(r"\\?\C:\Users\kaoru\OneDrive\桌面\test");
        let external = r"C:\Users\kaoru\OneDrive\桌面\test";
        runtime
            .thread_manager
            .thread_mut(thread_id)
            .unwrap()
            .workspace_path = verbatim.clone();
        let event_rx = runtime.event_bus.subscribe(thread_id);
        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "hello".into(),
            })
            .unwrap();

        runtime.emit_provider_command_started(thread_id, 123, &start.command);
        let events = collect_available_core_events(&event_rx);
        let event = events
            .iter()
            .find_map(|event| match event {
                ThreadEvent::ProviderCommandStarted { cwd, args, .. } => Some((cwd, args)),
                _ => None,
            })
            .expect("provider command event should be emitted");
        assert_eq!(event.0, external);
        assert!(event.1.iter().any(|arg| arg == external));
        assert!(!serde_json::to_string(&events).unwrap().contains(r"\\?\"));
    }

    #[test]
    fn ollama_resume_uses_provider_session_id_without_thread_routing_env() {
        let temp = tempfile::tempdir().unwrap();
        let provider_session_id = "0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_outer",
            ProviderCode::Ollama,
            Some(provider_session_id.into()),
            Some("model-a".into()),
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_outer".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert_eq!(
            start.command.args,
            vec![
                "--provider",
                "ollama",
                "--model",
                "model-a",
                "--sandbox",
                temp.path()
                    .join("workspace")
                    .join("thread_outer")
                    .to_str()
                    .unwrap(),
                "--session-id",
                provider_session_id,
            ]
        );
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert_provider_instruction_absent(&start.command);
        assert_eq!(env_value(&start.command, "PEDELEC_THREAD_ID"), None);
        assert_env(&start.command, "OLLAMA_API_KEY", "ollama_test_key");
        assert_ne!(provider_session_id, "thread_outer");
    }

    #[test]
    fn ollama_requires_explicit_model_before_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_ollama_no_model",
            ProviderCode::Ollama,
            None,
            Some("   ".into()),
        );

        let err = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_ollama_no_model".into(),
                message: "hello".into(),
            })
            .unwrap_err();

        assert_eq!(err.code, error_codes::MODEL_REQUIRED);
        assert_eq!(err.message, "Ollama provider requires a model.");
        assert_eq!(err.details.unwrap()["provider"], "ollama");
        assert_eq!(
            runtime.thread_status("thread_ollama_no_model"),
            Some(ThreadStatus::Idle)
        );
    }

    #[test]
    fn ollama_parser_maps_only_pedelec_agent_public_events() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_ollama_parse",
            ProviderCode::Ollama,
            None,
            Some("model-a".into()),
        );
        let event_rx = runtime.event_bus.subscribe("thread_ollama_parse");

        runtime.emit_provider_stdout(
            "thread_ollama_parse",
            concat!(
                "{\"type\":\"session\",\"sessionId\":\"0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176\",\"resumed\":false}\r\n",
                "{\"type\":\"assistant_message\",\"text\":\"hello\"}\n",
                "{\"type\":\"status\",\"status\":\"running\"}\n",
                "{\"type\":\"tool_call\",\"tool\":\"pedelec_cli.tool_call\",\"args\":{\"message\":\"ignore\"}}\n",
                "{\"type\":\"tool_result\",\"tool\":\"pedelec_cli.tool_call\",\"ok\":true,\"result\":{\"message\":\"ignore\"}}\n",
                "{\"type\":\"done\"}\n"
            )
            .into(),
        );
        runtime.emit_provider_stdout(
            "thread_ollama_parse",
            "{\"type\":\"assistant_message\",\"text\":\"chunk".into(),
        );
        runtime.emit_provider_stdout("thread_ollama_parse", "ed\"}\n".into());

        assert_eq!(
            runtime
                .provider_state("thread_ollama_parse")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176")
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "chunked")
        ));
        assert!(events.iter().all(|event| {
            !matches!(
                event,
                ThreadEvent::ToolCall { .. } | ThreadEvent::ToolResult { .. }
            )
        }));
        assert!(events.iter().all(
            |event| !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "ignore")
        ));
    }

    #[test]
    fn ollama_parser_preserves_structured_error_and_rejects_invalid_json() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_ollama_error",
            ProviderCode::Ollama,
            None,
            Some("model-a".into()),
        );
        let event_rx = runtime.event_bus.subscribe("thread_ollama_error");

        runtime.emit_provider_stdout(
            "thread_ollama_error",
            r#"{"type":"error","error":{"code":"OLLAMA_UNAVAILABLE","message":"Ollama request failed","details":{"status":500,"message":"body message"}}}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout("thread_ollama_error", "{not-json}\n".into());

        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::Error {
                    source: ThreadErrorSource::Provider { provider: ProviderCode::Ollama }, error, ..
                }
                    if error.code == "OLLAMA_UNAVAILABLE"
                        && error.message == "Ollama request failed"
                        && error.details.as_ref().and_then(|details| details.get("status")) == Some(&json!(500))
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::Error {
                    source: ThreadErrorSource::Provider { provider: ProviderCode::Ollama }, error, ..
                }
                    if error.code == error_codes::PROVIDER_COMMAND_FAILED
                        && error.message == "pedelec-agent emitted invalid JSON"
                        && error.details.as_ref().and_then(|details| details.get("line")) == Some(&json!("{not-json}"))
            )
        }));
        assert!(events.iter().all(
            |event| !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "body message")
        ));
    }

    #[test]
    fn list_providers_includes_opencode_unavailable_without_panic() {
        let providers = list_provider_infos(Some(OsString::from("")));
        let opencode = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::OpenCode)
            .unwrap();

        assert_eq!(opencode.name, "OpenCode");
        assert!(!opencode.available);
        assert_eq!(opencode.path, None);
        assert!(opencode.error.as_deref().unwrap().contains("PATH"));
    }

    #[test]
    fn list_providers_includes_cursor_unavailable_without_panic() {
        let providers = list_provider_infos(Some(OsString::from("")));
        let cursor = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Cursor)
            .unwrap();

        assert_eq!(cursor.name, "Cursor");
        assert!(!cursor.available);
        assert_eq!(cursor.path, None);
        assert!(cursor.error.as_deref().unwrap().contains("PATH"));
    }

    #[test]
    fn cursor_is_unavailable_when_path_only_contains_legacy_agent_alias() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "agent");
        let providers = list_provider_infos(Some(provider_path));
        let cursor = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Cursor)
            .unwrap();

        assert!(!cursor.available);
        assert_eq!(cursor.path, None);
    }

    #[test]
    fn cursor_scan_requires_the_acp_entrypoint_without_legacy_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_cursor_path(temp.path(), false);
        let providers = list_provider_infos(Some(provider_path));
        let cursor = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Cursor)
            .unwrap();

        assert!(cursor.scanned);
        assert_eq!(cursor.version.as_deref(), Some("9.9.9"));
        assert!(!cursor.available);
        assert!(cursor
            .error
            .as_deref()
            .is_some_and(|error| error.contains("acp")));
    }

    #[test]
    fn list_providers_includes_claude_unavailable_without_panic() {
        let providers = list_provider_infos(Some(OsString::from("")));
        let claude = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Claude)
            .unwrap();

        assert_eq!(claude.name, "Claude Code");
        assert!(!claude.available);
        assert_eq!(claude.path, None);
        assert!(claude.error.as_deref().unwrap().contains("PATH"));
    }

    #[test]
    fn list_providers_includes_ollama_using_pedelec_agent_binary() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let providers = list_provider_infos(Some(provider_path));
        let ollama = providers
            .iter()
            .find(|provider| provider.code == ProviderCode::Ollama)
            .unwrap();

        assert_eq!(ollama.name, "Ollama");
        assert!(ollama.available);
        assert!(ollama.path.as_deref().unwrap().contains("pedelec-agent"));
        assert_eq!(ollama.error, None);
    }

    #[test]
    fn list_providers_uses_expected_order() {
        let providers = list_provider_infos(Some(OsString::from("")));
        let codes = providers
            .into_iter()
            .map(|provider| provider.code)
            .collect::<Vec<_>>();

        assert_eq!(
            codes,
            vec![
                ProviderCode::Codex,
                ProviderCode::Antigravity,
                ProviderCode::OpenCode,
                ProviderCode::Cursor,
                ProviderCode::Claude,
                ProviderCode::Ollama,
            ]
        );
    }

    #[test]
    fn settings_missing_file_returns_initial_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            ..CoreRuntime::default()
        };

        let settings = runtime.get_settings().unwrap();

        assert_eq!(settings, PedelecSettings::default());
    }

    #[test]
    fn settings_new_json_shape_round_trips() {
        let mut settings = PedelecSettings {
            default_provider: Some(ProviderCode::Ollama),
            provider_settings: ProviderSettings::default(),
            wizard_metadata: EffortWizardMetadata::default(),
        };
        settings.provider_settings.ollama.base_url = "http://127.0.0.1:11434".into();
        settings.provider_settings.ollama.timeout_ms = 120_000;
        settings.provider_settings.ollama.api_key = "ollama_xxx".into();
        settings.provider_settings.ollama.efforts_args.default =
            vec!["--model".into(), "qwen3:8b".into()];

        let value = serde_json::to_value(&settings).unwrap();

        assert_eq!(value["defaultProvider"], json!("ollama"));
        assert_eq!(
            value["providerSettings"]["ollama"]["effortsArgs"]["default"],
            json!(["--model", "qwen3:8b"])
        );
        assert!(value.get("defaultModels").is_none());
        assert_eq!(
            serde_json::from_value::<PedelecSettings>(value).unwrap(),
            settings
        );
    }

    #[test]
    fn settings_legacy_ollama_shape_defaults_missing_api_key() {
        let settings = serde_json::from_value::<PedelecSettings>(json!({
            "defaultProvider": "ollama",
            "defaultModels": {
                "ollama": "qwen3:8b"
            },
            "providerSettings": {
                "ollama": {
                    "baseUrl": "http://127.0.0.1:11434",
                    "timeoutMs": 120000
                }
            }
        }))
        .unwrap();

        assert_eq!(settings.provider_settings.ollama.api_key, "");
        assert!(settings
            .provider_settings
            .ollama
            .efforts_args
            .default
            .is_empty());
    }

    #[test]
    fn update_settings_persists_provider_and_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let settings_path = temp.path().join("settings.json");
        let mut provider_settings = ProviderSettingsInput::default();
        provider_settings.ollama.efforts_args.default =
            vec!["--model".into(), "qwen3-14b-32k:latest".into()];
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path.clone()),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings,
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Ollama));
        assert_eq!(saved.provider_settings.ollama.api_key, "ollama");
        assert_eq!(
            saved.provider_settings.ollama.efforts_args.default,
            vec!["--model", "qwen3-14b-32k:latest"]
        );
        assert_eq!(read_settings_file(&settings_path).unwrap(), saved);
    }

    #[test]
    fn update_settings_persists_and_normalizes_ollama_provider_settings() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let settings_path = temp.path().join("settings.json");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path.clone()),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(" https://ollama.example.test/ ".into()),
                        timeout_ms: Some(250_000),
                        api_key: Some(" ollama_cloud_key ".into()),
                        tavily_api_key: None,
                        efforts_args: EffortsArgs {
                            default: vec!["--model".into(), "qwen3:8b".into()],
                            ..EffortsArgs::default()
                        },
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap();

        assert_eq!(
            saved.provider_settings.ollama.base_url,
            "https://ollama.example.test"
        );
        assert_eq!(saved.provider_settings.ollama.timeout_ms, 250_000);
        assert_eq!(saved.provider_settings.ollama.api_key, "ollama_cloud_key");
        assert_eq!(
            saved.provider_settings.ollama.efforts_args.default,
            vec!["--model", "qwen3:8b"]
        );
        assert_eq!(read_settings_file(&settings_path).unwrap(), saved);
    }

    #[test]
    fn update_settings_defaults_blank_base_url_and_missing_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some("   ".into()),
                        timeout_ms: None,
                        api_key: Some("ollama".into()),
                        tavily_api_key: None,
                        efforts_args: EffortsArgs {
                            default: vec!["--model".into(), "qwen3:8b".into()],
                            ..EffortsArgs::default()
                        },
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap();

        assert_eq!(
            saved.provider_settings.ollama.base_url,
            DEFAULT_OLLAMA_BASE_URL
        );
        assert_eq!(
            saved.provider_settings.ollama.timeout_ms,
            DEFAULT_OLLAMA_TIMEOUT_MS
        );
        assert_eq!(saved.provider_settings.ollama.api_key, "ollama");
    }

    #[test]
    fn update_settings_rejects_invalid_ollama_provider_settings() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };
        let valid_efforts = EffortsArgs {
            default: vec!["--model".into(), "qwen3:8b".into()],
            ..EffortsArgs::default()
        };

        let invalid_url = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some("ftp://127.0.0.1:11434".into()),
                        timeout_ms: Some(120_000),
                        api_key: Some("ollama".into()),
                        tavily_api_key: None,
                        efforts_args: valid_efforts.clone(),
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap_err();
        assert_eq!(invalid_url.code, error_codes::OLLAMA_BASE_URL_INVALID);

        let invalid_timeout = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                        timeout_ms: Some(0),
                        api_key: Some("ollama".into()),
                        tavily_api_key: None,
                        efforts_args: valid_efforts.clone(),
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap_err();
        assert_eq!(invalid_timeout.code, error_codes::OLLAMA_REQUEST_FAILED);

        let missing_api_key = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                        timeout_ms: Some(120_000),
                        api_key: Some("   ".into()),
                        tavily_api_key: None,
                        efforts_args: valid_efforts,
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap_err();
        assert_eq!(missing_api_key.code, error_codes::OLLAMA_API_KEY_REQUIRED);
    }

    #[test]
    fn update_settings_accepts_empty_non_ollama_effort_tiers() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "codex");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Codex,
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap();

        assert!(saved
            .provider_settings
            .codex
            .efforts_args
            .default
            .is_empty());
        assert!(saved.provider_settings.codex.efforts_args.low.is_empty());
        assert!(saved.provider_settings.codex.efforts_args.high.is_empty());
    }

    #[test]
    fn update_settings_rejects_unavailable_provider() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(OsString::from("")),
            ..CoreRuntime::default()
        };

        let err = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Codex,
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap_err();

        assert_eq!(err.code, error_codes::DEFAULT_PROVIDER_UNAVAILABLE);
        assert!(!temp.path().join("settings.json").exists());
    }

    #[test]
    fn update_settings_allows_unavailable_ollama_provider() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider_settings = ProviderSettingsInput::default();
        provider_settings.ollama.efforts_args.default = vec!["--model".into(), "qwen3:8b".into()];
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(OsString::from("")),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings,
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Ollama));
        assert!(temp.path().join("settings.json").exists());
    }

    #[test]
    fn update_settings_allows_unavailable_non_default_provider_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "codex");
        let mut provider_settings = ProviderSettingsInput::default();
        provider_settings.codex.efforts_args.default = vec!["-m".into(), "gpt-5".into()];
        provider_settings.antigravity.efforts_args.high =
            vec!["--model".into(), "antigravity-2.5-pro".into()];
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Codex,
                provider_settings,
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Codex));
        assert_eq!(
            saved.provider_settings.antigravity.efforts_args.high,
            vec!["--model", "antigravity-2.5-pro"]
        );
    }

    #[test]
    fn update_settings_for_non_ollama_provider_does_not_require_ollama_credentials_or_model() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "codex");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Codex,
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: None,
                        timeout_ms: None,
                        api_key: Some("   ".into()),
                        tavily_api_key: None,
                        ..OllamaProviderSettingsInput::default()
                    },
                    ..ProviderSettingsInput::default()
                },
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Codex));
        assert_eq!(saved.provider_settings.ollama.api_key, "");
        assert!(saved
            .provider_settings
            .ollama
            .efforts_args
            .default
            .is_empty());
    }

    #[test]
    fn update_settings_requires_ollama_model_when_ollama_is_default() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "pedelec-agent");
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let error = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap_err();

        assert_eq!(error.code, error_codes::MODEL_REQUIRED);
        assert_eq!(error.details.unwrap()["provider"], "ollama");
    }

    #[test]
    fn update_settings_for_other_provider_preserves_existing_ollama_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let provider_path = test_provider_path(temp.path(), "codex");
        let mut provider_settings = ProviderSettingsInput::default();
        provider_settings.ollama.efforts_args.default = vec!["--model".into(), "qwen3:8b".into()];
        provider_settings.ollama.api_key = Some(String::new());
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(provider_path),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Codex,
                provider_settings,
            })
            .unwrap();

        assert_eq!(
            saved.provider_settings.ollama.efforts_args.default,
            vec!["--model", "qwen3:8b"]
        );
        assert_eq!(saved.provider_settings.ollama.api_key, "");
    }

    #[test]
    fn effort_validation_rejects_malformed_duplicate_and_unsafe_args() {
        let odd = normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                default: vec!["-m".into()],
                ..EffortsArgs::default()
            },
        )
        .unwrap_err();
        assert_eq!(odd.code, error_codes::INVALID_INPUT);

        let duplicate = normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                default: vec![
                    "-m".into(),
                    "gpt-5".into(),
                    "-m".into(),
                    "gpt-5-mini".into(),
                ],
                ..EffortsArgs::default()
            },
        )
        .unwrap_err();
        assert_eq!(duplicate.code, error_codes::INVALID_INPUT);

        let unsafe_flag = normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                default: vec!["--sandbox".into(), "unsafe".into()],
                ..EffortsArgs::default()
            },
        )
        .unwrap_err();
        assert_eq!(unsafe_flag.code, error_codes::INVALID_INPUT);
    }

    #[test]
    fn effort_validation_checks_provider_native_values_without_matching_tier_names() {
        assert!(normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                low: vec![
                    "-m".into(),
                    "gpt-5".into(),
                    "-c".into(),
                    "model_reasoning_effort=\"xhigh\"".into(),
                ],
                ..EffortsArgs::default()
            },
        )
        .is_ok());

        assert!(normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                default: vec!["-c".into(), "model_reasoning_effort=banana".into()],
                ..EffortsArgs::default()
            },
        )
        .is_err());
        assert!(normalize_efforts_args(
            ProviderCode::Codex,
            EffortsArgs {
                default: vec!["-c".into(), "skills.include_instructions=true".into()],
                ..EffortsArgs::default()
            },
        )
        .is_err());

        assert!(normalize_efforts_args(
            ProviderCode::Antigravity,
            EffortsArgs {
                default: vec![
                    "--model".into(),
                    "agy-model".into(),
                    "--effort".into(),
                    "high".into()
                ],
                ..EffortsArgs::default()
            },
        )
        .is_ok());
        assert!(normalize_efforts_args(
            ProviderCode::Antigravity,
            EffortsArgs {
                default: vec!["--effort".into(), "xhigh".into()],
                ..EffortsArgs::default()
            },
        )
        .is_err());

        assert!(normalize_efforts_args(
            ProviderCode::Claude,
            EffortsArgs {
                default: vec!["--effort".into(), "xhigh".into()],
                ..EffortsArgs::default()
            },
        )
        .is_ok());
        assert!(normalize_efforts_args(
            ProviderCode::Claude,
            EffortsArgs {
                default: vec!["--effort".into(), "banana".into()],
                ..EffortsArgs::default()
            },
        )
        .is_err());

        for provider in [
            ProviderCode::OpenCode,
            ProviderCode::Cursor,
            ProviderCode::Ollama,
        ] {
            assert!(normalize_efforts_args(
                provider,
                EffortsArgs {
                    default: vec!["--effort".into(), "high".into()],
                    ..EffortsArgs::default()
                },
            )
            .is_err());
        }
    }

    #[test]
    fn create_thread_normalizes_default_and_does_not_fallback_empty_tiers() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let mut settings = PedelecSettings::default();
        settings.provider_settings.codex.efforts_args.default =
            vec!["-m".into(), "gpt-default".into()];
        write_settings_file(&settings_path, &settings).unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path),
            workspace_manager: WorkspaceManager::with_workspace_root(
                temp.path().join("workspaces"),
            ),
            ..CoreRuntime::default()
        };

        let default_thread = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();
        let default_state = runtime
            .thread_manager
            .thread(&default_thread.thread_id)
            .unwrap();
        assert_eq!(default_state.effort_level, EffortLevel::Default);
        assert_eq!(default_state.effort_args, vec!["-m", "gpt-default"]);

        let low_thread = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Low),
                skills: None,
                workspace: None,
            })
            .unwrap();
        let low_state = runtime
            .thread_manager
            .thread(&low_thread.thread_id)
            .unwrap();
        assert_eq!(low_state.effort_level, EffortLevel::Low);
        assert!(low_state.effort_args.is_empty());
    }

    #[test]
    fn ollama_empty_selected_tier_requires_a_model_without_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let mut settings = PedelecSettings::default();
        settings.provider_settings.ollama.efforts_args.default =
            vec!["--model".into(), "ollama-default".into()];
        write_settings_file(&settings_path, &settings).unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path),
            workspace_manager: WorkspaceManager::with_workspace_root(
                temp.path().join("workspaces"),
            ),
            ..CoreRuntime::default()
        };

        let error = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Ollama,
                effort_level: Some(EffortLevel::Low),
                skills: None,
                workspace: None,
            })
            .unwrap_err();
        assert_eq!(error.code, error_codes::MODEL_REQUIRED);
    }

    #[test]
    fn create_thread_snapshots_selected_effort_args() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let mut settings = PedelecSettings::default();
        settings.provider_settings.codex.efforts_args.high = vec!["-m".into(), "gpt-5-high".into()];
        write_settings_file(&settings_path, &settings).unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path.clone()),
            workspace_manager: WorkspaceManager::with_workspace_root(
                temp.path().join("workspaces"),
            ),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::High),
                skills: None,
                workspace: None,
            })
            .unwrap();
        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        assert_eq!(thread.effort_level, EffortLevel::High);
        assert_eq!(thread.effort_args, vec!["-m", "gpt-5-high"]);

        settings.provider_settings.codex.efforts_args.high =
            vec!["-m".into(), "gpt-5-changed".into()];
        write_settings_file(&settings_path, &settings).unwrap();
        assert_eq!(thread.effort_args, vec!["-m", "gpt-5-high"]);
    }

    #[test]
    fn provider_commands_use_snapshotted_effort_args_for_first_run_and_resume() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let mut settings = PedelecSettings::default();
        settings.provider_settings.codex.efforts_args.default =
            vec!["-m".into(), "gpt-snapshot".into()];
        write_settings_file(&settings_path, &settings).unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(settings_path.clone()),
            workspace_manager: WorkspaceManager::with_workspace_root(
                temp.path().join("workspaces"),
            ),
            ..CoreRuntime::default()
        };
        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();
        let thread_id = output.thread_id.clone();

        settings.provider_settings.codex.efforts_args.default =
            vec!["-m".into(), "gpt-changed".into()];
        write_settings_file(&settings_path, &settings).unwrap();

        let first = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.clone(),
                message: "first".into(),
            })
            .unwrap();
        assert!(first
            .command
            .args
            .windows(2)
            .any(|pair| pair == ["-m", "gpt-snapshot"]));
        assert!(!first
            .command
            .args
            .windows(2)
            .any(|pair| pair == ["-m", "gpt-changed"]));

        runtime
            .thread_manager
            .thread_mut(&thread_id)
            .unwrap()
            .status = ThreadStatus::Idle;
        runtime
            .thread_manager
            .provider_state_mut(&thread_id)
            .unwrap()
            .provider_session_id = Some("session-snapshot".into());
        let resume = runtime
            .begin_send_text(SendTextInput {
                thread_id,
                message: "resume".into(),
            })
            .unwrap();
        assert!(resume
            .command
            .args
            .windows(2)
            .any(|pair| pair == ["-m", "gpt-snapshot"]));
        assert!(!resume
            .command
            .args
            .windows(2)
            .any(|pair| pair == ["-m", "gpt-changed"]));
    }

    #[test]
    fn check_ollama_connection_accepts_valid_tags_response_without_authorization() {
        let (base_url, handle) = start_single_response_server(200, r#"{"models":[]}"#);

        let output = CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
            base_url: Some(format!("{base_url}/")),
        });
        let request = handle.join().unwrap();

        assert!(output.connected);
        assert!(request.starts_with("GET /api/tags "));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
    }

    #[test]
    fn check_ollama_connection_rejects_http_status_invalid_json_and_invalid_shape() {
        let (base_url, http_handle) = start_single_response_server(500, "nope");
        let http_output =
            CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
                base_url: Some(base_url),
            });
        http_handle.join().unwrap();
        assert!(!http_output.connected);

        let (base_url, json_handle) = start_single_response_server(200, "{not-json");
        let json_output =
            CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
                base_url: Some(base_url),
            });
        json_handle.join().unwrap();
        assert!(!json_output.connected);

        let (base_url, shape_handle) = start_single_response_server(200, r#"{"models":{}}"#);
        let shape_output =
            CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
                base_url: Some(base_url),
            });
        shape_handle.join().unwrap();
        assert!(!shape_output.connected);
    }

    #[test]
    fn check_ollama_connection_rejects_invalid_base_url_connection_refused_and_timeout() {
        let invalid_output =
            CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
                base_url: Some("not-a-url".into()),
            });
        assert!(!invalid_output.connected);

        let refused_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let refused_url = format!("http://{}", refused_listener.local_addr().unwrap());
        drop(refused_listener);
        let refused_output =
            CoreRuntime::default().check_ollama_connection(CheckOllamaConnectionInput {
                base_url: Some(refused_url),
            });
        assert!(!refused_output.connected);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let timeout_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let timeout_output = check_ollama_connection_with_timeout(
            CheckOllamaConnectionInput {
                base_url: Some(timeout_url),
            },
            1,
        );
        handle.join().unwrap();
        assert!(!timeout_output.connected);
    }

    #[test]
    fn list_ollama_models_parses_tags_response() {
        let (base_url, handle) = start_single_response_server(
            200,
            r#"{"models":[{"model":"qwen3:8b","name":"Qwen 3"},{"model":"llama3"},{"model":""},{"name":"missing-model"}]}"#,
        );
        let runtime = CoreRuntime::default();

        let models = runtime
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(format!("{base_url}/")),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap();
        let request = handle.join().unwrap();

        assert!(request.starts_with("GET /api/tags "));
        assert!(request.contains("authorization: Bearer ollama_test_key"));
        assert_eq!(
            models,
            vec![
                OllamaModelOption {
                    value: "qwen3:8b".into(),
                    label: "Qwen 3".into(),
                },
                OllamaModelOption {
                    value: "llama3".into(),
                    label: "llama3".into(),
                },
            ]
        );
    }

    #[test]
    fn list_ollama_models_allows_empty_model_list() {
        let (base_url, handle) = start_single_response_server(200, r#"{"models":[]}"#);
        let models = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(base_url),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap();
        handle.join().unwrap();

        assert!(models.is_empty());
    }

    #[test]
    fn list_ollama_models_reports_http_and_invalid_json_errors() {
        let (base_url, http_handle) = start_single_response_server(500, "nope");
        let http_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(base_url),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        http_handle.join().unwrap();
        assert_eq!(http_err.code, error_codes::OLLAMA_REQUEST_FAILED);

        let (base_url, json_handle) = start_single_response_server(200, "{not-json");
        let json_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(base_url),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        json_handle.join().unwrap();
        assert_eq!(json_err.code, error_codes::OLLAMA_RESPONSE_INVALID);
    }

    #[test]
    fn list_ollama_models_maps_cloud_http_status_errors() {
        let cases = [
            (401, "bad key", error_codes::OLLAMA_AUTH_FAILED),
            (403, "forbidden", error_codes::OLLAMA_AUTH_FAILED),
            (
                404,
                "model was not found",
                error_codes::OLLAMA_MODEL_NOT_FOUND,
            ),
            (
                429,
                "quota exceeded",
                error_codes::OLLAMA_CLOUD_LIMIT_EXCEEDED,
            ),
        ];

        for (status, body, expected_code) in cases {
            let (base_url, handle) = start_single_response_server(status, body);
            let err = CoreRuntime::default()
                .list_ollama_models(ListOllamaModelsInput {
                    base_url: Some(base_url),
                    timeout_ms: Some(120_000),
                    api_key: Some("ollama_test_key".into()),
                })
                .unwrap_err();
            handle.join().unwrap();
            assert_eq!(err.code, expected_code);
        }
    }

    #[test]
    fn list_ollama_models_reports_invalid_shape_and_bad_input() {
        let (base_url, handle) = start_single_response_server(200, r#"{"models":{}}"#);
        let shape_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(base_url),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(shape_err.code, error_codes::OLLAMA_RESPONSE_INVALID);

        let url_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some("not-a-url".into()),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        assert_eq!(url_err.code, error_codes::OLLAMA_BASE_URL_INVALID);

        let url_with_api_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some("https://ollama.com/api".into()),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        assert_eq!(url_with_api_err.code, error_codes::OLLAMA_BASE_URL_INVALID);

        let timeout_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                timeout_ms: Some(0),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        assert_eq!(timeout_err.code, error_codes::OLLAMA_REQUEST_FAILED);

        let missing_key_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                timeout_ms: Some(120_000),
                api_key: Some(" ".into()),
            })
            .unwrap_err();
        assert_eq!(missing_key_err.code, error_codes::OLLAMA_API_KEY_REQUIRED);
    }

    #[test]
    fn list_ollama_models_reports_connection_refused_and_timeout() {
        let refused_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let refused_url = format!("http://{}", refused_listener.local_addr().unwrap());
        drop(refused_listener);

        let refused_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(refused_url),
                timeout_ms: Some(120_000),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        assert_eq!(refused_err.code, error_codes::OLLAMA_UNAVAILABLE);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let timeout_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let timeout_err = CoreRuntime::default()
            .list_ollama_models(ListOllamaModelsInput {
                base_url: Some(timeout_url),
                timeout_ms: Some(1),
                api_key: Some("ollama_test_key".into()),
            })
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(timeout_err.code, error_codes::OLLAMA_UNAVAILABLE);
    }

    #[test]
    fn legacy_default_model_settings_shape_is_not_supported() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        fs::write(
            &settings_path,
            r#"{"defaultProvider":"codex","defaultModel":"gpt-5"}"#,
        )
        .unwrap();

        let err = read_settings_file(&settings_path).unwrap_err();

        assert_eq!(err.code, error_codes::SETTINGS_READ_FAILED);
    }

    #[test]
    fn provider_binary_lookup_candidates_include_windows_opencode_names() {
        let dirs = vec![PathBuf::from("C:/bin")];
        let candidates = provider_binary_lookup_candidates("opencode", &dirs);

        #[cfg(windows)]
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("C:/bin/opencode"),
                PathBuf::from("C:/bin/opencode.exe"),
                PathBuf::from("C:/bin/opencode.cmd"),
                PathBuf::from("C:/bin/opencode.bat"),
            ]
        );

        #[cfg(not(windows))]
        assert_eq!(candidates, vec![PathBuf::from("C:/bin").join("opencode")]);
    }

    #[test]
    fn provider_binary_lookup_candidates_include_windows_cursor_agent_names() {
        let dirs = vec![PathBuf::from("C:/bin")];
        let candidates = provider_binary_lookup_candidates("cursor-agent", &dirs);

        #[cfg(windows)]
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("C:/bin/cursor-agent"),
                PathBuf::from("C:/bin/cursor-agent.exe"),
                PathBuf::from("C:/bin/cursor-agent.cmd"),
                PathBuf::from("C:/bin/cursor-agent.bat"),
            ]
        );

        #[cfg(not(windows))]
        assert_eq!(
            candidates,
            vec![PathBuf::from("C:/bin").join("cursor-agent")]
        );
    }

    #[test]
    fn cursor_binary_lookup_candidates_do_not_include_legacy_agent_aliases() {
        let dirs = vec![PathBuf::from("C:/bin")];
        let candidates = provider_binary_lookup_candidates("cursor-agent", &dirs);

        assert!(candidates.iter().all(|candidate| {
            !matches!(
                candidate.file_name().and_then(|name| name.to_str()),
                Some("agent") | Some("agent.exe") | Some("agent.cmd") | Some("agent.bat")
            )
        }));
    }

    #[test]
    fn provider_parser_buffers_jsonl_and_updates_session_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_parse",
            ProviderCode::Codex,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_parse");

        runtime.emit_provider_stdout("thread_parse", r#"{"sessionId":"123e4567-e89b"#.into());
        runtime.emit_provider_stdout(
            "thread_parse",
            r#"-12d3-a456-426614174000","text":"hello"}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_parse",
            r#"{"sessionId":"123e4567-e89b-12d3-a456-426614174000"}"#.to_string() + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_parse")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
        let events = collect_available_core_events(&event_rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ThreadEvent::ProviderSessionIdUpdated { .. }))
                .count(),
            1
        );
        assert!(matches!(events[0], ThreadEvent::RawStdout { .. }));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
    }

    #[test]
    fn codex_assistant_message_parser_does_not_require_role() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_text",
            ProviderCode::Codex,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_codex_text");

        runtime.emit_provider_stdout(
            "thread_codex_text",
            r#"{"text":"hello"}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_codex_text",
            r#"{"role":"user","text":"still codex"}"#.to_string() + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "still codex")
        ));
    }

    #[test]
    fn antigravity_assistant_step_update_deltas_do_not_emit_assistant_messages() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_role",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_antigravity_role");

        runtime.emit_provider_stdout(
            "thread_antigravity_role",
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hello"}}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_antigravity_role",
            r#"{"event":"step_update","step_update":{"step_type":"user_input","text_delta":"ignore user"}}"#.to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_antigravity_role",
            r#"{"event":"step_update","step_update":{"step_type":"tool","text_delta":"ignore missing role"}}"#.to_string() + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        assert!(!events
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantMessage { .. })));
    }

    #[test]
    fn antigravity_assistant_uses_clean_final_response_after_corrupted_deltas() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_final_response",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_antigravity_final_response");

        runtime.emit_provider_stdout(
            "thread_antigravity_final_response",
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"眼前�"}}"#.to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_antigravity_final_response",
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"��火光"}}"#.to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_antigravity_final_response",
            r#"{"event":"result","result":{"status":"SUCCESS","response":"眼前的火光"}}"#
                .to_string()
                + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        let assistant_messages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::AssistantMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_messages, vec!["眼前的火光"]);
    }

    #[test]
    fn antigravity_assistant_final_response_preserves_whitespace() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_final_whitespace",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_antigravity_final_whitespace");

        runtime.emit_provider_stdout(
            "thread_antigravity_final_whitespace",
            r#"{"event":"result","result":{"status":"SUCCESS","response":"  nested hello\n"}}"#
                .to_string()
                + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        let assistant_messages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ThreadEvent::AssistantMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_messages, vec!["  nested hello\n"]);
    }

    #[test]
    fn antigravity_assistant_success_with_empty_final_response_does_not_emit_assistant_message() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_empty_final",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_antigravity_empty_final");

        runtime.emit_provider_stdout(
            "thread_antigravity_empty_final",
            r#"{"event":"result","result":{"status":"SUCCESS","response":""}}"#.to_string() + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        assert!(!events
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantMessage { .. })));
    }

    #[test]
    fn antigravity_assistant_unsuccessful_result_still_emits_provider_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_failed_result",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_antigravity_failed_result");

        runtime.emit_provider_stdout(
            "thread_antigravity_failed_result",
            r#"{"event":"result","result":{"status":"FAILED","conversation_id":"conv_1","response":"failed"}}"#.to_string()
                + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        let error = events
            .iter()
            .find_map(|event| match event {
                ThreadEvent::Error {
                    source:
                        ThreadErrorSource::Provider {
                            provider: ProviderCode::Antigravity,
                        },
                    error,
                    ..
                } => Some(error),
                _ => None,
            })
            .expect("Antigravity unsuccessful result should emit a provider error");
        assert_eq!(error.code, error_codes::PROVIDER_COMMAND_FAILED);
        assert_eq!(error.details.as_ref().unwrap()["status"], json!("FAILED"));
        assert_eq!(
            error.details.as_ref().unwrap()["conversation_id"],
            json!("conv_1")
        );
        assert_eq!(error.details.as_ref().unwrap()["response"], json!("failed"));
        assert!(!events
            .iter()
            .any(|event| matches!(event, ThreadEvent::AssistantMessage { .. })));
    }

    #[test]
    fn antigravity_assistant_init_updates_provider_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_init",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_antigravity_init");

        runtime.emit_provider_stdout(
            "thread_antigravity_init",
            r#"{"event":"init","conversation_id":"conv_123"}"#.to_string() + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_antigravity_init")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("conv_123")
        );
        let events = collect_available_core_events(&event_rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ThreadEvent::ProviderSessionIdUpdated { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn codex_thread_started_updates_session_id_once_and_resume_uses_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_codex_started",
            ProviderCode::Codex,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_codex_started");

        runtime.emit_provider_stdout(
            "thread_codex_started",
            r#"{"type":"thread.started","thread_id":"019e91d7-4a21-7ca0-aeef-c27ce6e334c5"}"#
                .to_string()
                + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_codex_started",
            r#"{"type":"thread.started","thread_id":"019e91d7-4a21-7ca0-aeef-c27ce6e334c5"}"#
                .to_string()
                + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_codex_started")
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("019e91d7-4a21-7ca0-aeef-c27ce6e334c5")
        );
        let events = collect_available_core_events(&event_rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ThreadEvent::ProviderSessionIdUpdated { .. }))
                .count(),
            1
        );

        let start = runtime
            .begin_send_text(SendTextInput {
                thread_id: "thread_codex_started".into(),
                message: "continue".into(),
            })
            .unwrap();

        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args == ["-c", "skills.include_instructions=false"]));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| args[0] == "-c" && args[1].starts_with("developer_instructions=")));
        assert!(start
            .command
            .args
            .windows(2)
            .any(|args| { args == ["resume", "019e91d7-4a21-7ca0-aeef-c27ce6e334c5"] }));
    }

    #[test]
    fn unrelated_json_thread_id_does_not_update_provider_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_unrelated_id",
            ProviderCode::Codex,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_unrelated_id");

        runtime.emit_provider_stdout(
            "thread_unrelated_id",
            r#"{"type":"turn.started","thread_id":"019e91d7-4a21-7ca0-aeef-c27ce6e334c5"}"#
                .to_string()
                + "\n",
        );

        assert_eq!(
            runtime
                .provider_state("thread_unrelated_id")
                .unwrap()
                .provider_session_id,
            None
        );
        let events = collect_available_core_events(&event_rx);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ThreadEvent::ProviderSessionIdUpdated { .. })));
    }

    #[test]
    fn malformed_provider_json_still_emits_raw_and_keeps_status() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_malformed",
            ProviderCode::Codex,
            None,
            None,
        );
        let event_rx = runtime.event_bus.subscribe("thread_malformed");

        runtime.emit_provider_stdout("thread_malformed", "{not-json}\n".into());

        assert_eq!(
            runtime.thread_status("thread_malformed"),
            Some(ThreadStatus::Idle)
        );
        let events = collect_available_core_events(&event_rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ThreadEvent::RawStdout { .. }));
    }

    #[test]
    fn thread_state_serializes_camel_case_fields() {
        let now = DateTime::parse_from_rfc3339("2026-06-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let state = ThreadState {
            thread_id: "thread_abc123".into(),
            provider: ProviderCode::Codex,
            effort_level: EffortLevel::Default,
            effort_args: vec!["-m".into(), "gpt-5".into()],
            workspace_path: PathBuf::from("C:/tmp/pedelec/thread_abc123"),
            skills: vec![SkillFile {
                original_url: "https://example.test/tools.md".into(),
                original_filename: "tools.md".into(),
                local_path: PathBuf::from("skills/tools.md"),
                sha256: "abc".into(),
                size_bytes: 12,
            }],
            status: ThreadStatus::Idle,
            process_id: Some(42),
            created_at: now,
            updated_at: now,
            sdk_origin: None,
        };

        let value = serde_json::to_value(state).unwrap();
        assert!(value.get("threadId").is_some());
        assert!(value.get("workspacePath").is_some());
        assert!(value.get("createdAt").is_some());
        assert!(value.get("thread_id").is_none());
        assert!(value.get("workspace_path").is_none());
        assert!(value.get("created_at").is_none());
    }

    #[test]
    fn provider_instruction_omits_app_configuration_without_skills() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_no_tools_md".into(),
            provider: ProviderCode::Codex,
            effort_level: EffortLevel::Default,
            effort_args: Vec::new(),
            workspace_path: PathBuf::from("workspace").join("thread_no_tools_md"),
            skills: vec![SkillFile {
                original_url: "https://example.test/tools.json".into(),
                original_filename: "tools.json".into(),
                local_path: PathBuf::from("skills").join("tools.json"),
                sha256: "sha".into(),
                size_bytes: 2,
            }],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
            sdk_origin: None,
        };

        let instruction = build_provider_instruction(&thread, &ToolRegistry::default());

        assert!(instruction.contains("[Pedelec Host Context]"));
        assert!(instruction.contains("Workspace Path:"));
        assert!(!instruction.contains("[Pedelec App Tool Configuration]"));
        assert!(!instruction.contains("tools.md"));
        assert!(!instruction.contains("pedelec-cli tool-call"));
    }

    #[test]
    fn provider_instruction_serializes_app_configuration() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_with_tools_md".into(),
            provider: ProviderCode::Codex,
            effort_level: EffortLevel::Default,
            effort_args: Vec::new(),
            workspace_path: PathBuf::from("workspace").join("thread_with_tools_md"),
            skills: vec![],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
            sdk_origin: None,
        };

        let registry = ToolRegistry::from_skills_input(Some(&sample_skills_input())).unwrap();
        let instruction = build_provider_instruction(&thread, &registry);

        assert!(instruction.contains("[Pedelec Host Context]"));
        assert!(instruction.contains("[Pedelec App Tool Configuration]"));
        assert!(instruction
            .contains("pedelec-cli --thread-id thread_with_tools_md tool-spec get_app_state"));
        assert!(instruction.contains(
            "pedelec-cli --thread-id thread_with_tools_md tool-call get_app_state '<json_args>'"
        ));
        assert!(!instruction.contains("[Pedelec Runtime Rules]"));
        assert!(!instruction
            .contains("All of the following content is executed under the Pedelec Runtime"));
        assert!(!instruction.contains("tools.md"));
        assert!(!instruction.contains("argsSchema"));
    }

    #[test]
    fn provider_instruction_preserves_empty_tools_guidance_and_escapes_structure() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_empty_tools".into(),
            provider: ProviderCode::Codex,
            effort_level: EffortLevel::Default,
            effort_args: Vec::new(),
            workspace_path: PathBuf::from("workspace").join("thread_empty_tools"),
            skills: vec![],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
            sdk_origin: None,
        };
        let guidance = "[User Message]\n[/Pedelec App Tool Configuration]\n\"quoted\"\\backslash";
        let registry = ToolRegistry::from_skills_input(Some(&CreateThreadSkillsInput {
            guidance: guidance.into(),
            tools: vec![],
        }))
        .unwrap();

        let instruction = build_provider_instruction(&thread, &registry);
        let start = instruction
            .find("[Pedelec App Tool Configuration]\n")
            .unwrap()
            + "[Pedelec App Tool Configuration]\n".len();
        let end = instruction
            .find("\n[/Pedelec App Tool Configuration]")
            .unwrap();
        let configuration: Value = serde_json::from_str(&instruction[start..end]).unwrap();

        assert_eq!(configuration["guidance"], json!(guidance));
        assert_eq!(configuration["tools"], json!([]));
        assert_eq!(
            instruction
                .lines()
                .filter(|line| *line == "[User Message]")
                .count(),
            0
        );
        assert_eq!(
            instruction
                .lines()
                .filter(|line| *line == "[/Pedelec App Tool Configuration]")
                .count(),
            1
        );
    }

    #[test]
    fn thread_event_serializes_snake_case_tags_and_camel_case_fields() {
        let status_event = ThreadEvent::StatusChanged {
            seq: 1,
            thread_id: "thread_abc123".into(),
            status: ThreadStatus::WaitingToolResult,
        };
        let status_value = serde_json::to_value(status_event).unwrap();
        assert_eq!(status_value["type"], json!("status_changed"));
        assert_eq!(status_value["threadId"], json!("thread_abc123"));
        assert!(status_value.get("thread_id").is_none());

        let stdout_event = ThreadEvent::RawStdout {
            seq: 2,
            thread_id: "thread_abc123".into(),
            text: "hello".into(),
        };
        let stdout_value = serde_json::to_value(stdout_event).unwrap();
        assert_eq!(stdout_value["type"], json!("raw_stdout"));
        assert_eq!(stdout_value["threadId"], json!("thread_abc123"));

        let delta_value = serde_json::to_value(ThreadEvent::AssistantDelta {
            seq: 3,
            thread_id: "thread_abc123".into(),
            text: "hel".into(),
        })
        .unwrap();
        assert_eq!(delta_value["type"], json!("assistant_delta"));
        assert_eq!(delta_value["text"], json!("hel"));

        let message_value = serde_json::to_value(ThreadEvent::AssistantMessage {
            seq: 4,
            thread_id: "thread_abc123".into(),
            text: "hello".into(),
        })
        .unwrap();
        assert_eq!(message_value["type"], json!("assistant_message"));
        assert_eq!(message_value["text"], json!("hello"));

        let session_event = ThreadEvent::ProviderSessionIdUpdated {
            seq: 3,
            thread_id: "thread_abc123".into(),
            provider_session_id: "session_xyz".into(),
        };
        let session_value = serde_json::to_value(session_event).unwrap();
        assert_eq!(session_value["type"], json!("provider_session_id_updated"));
        assert_eq!(session_value["providerSessionId"], json!("session_xyz"));
        assert!(session_value.get("provider_session_id").is_none());

        let command_event = ThreadEvent::ProviderCommandStarted {
            seq: 4,
            thread_id: "thread_abc123".into(),
            process_id: 42,
            program: "codex".into(),
            args: vec!["exec".into(), "-".into()],
            cwd: "C:/tmp/pedelec/thread_abc123".into(),
            prompt: "full provider prompt".into(),
        };
        let command_value = serde_json::to_value(command_event).unwrap();
        assert_eq!(command_value["type"], json!("provider_command_started"));
        assert_eq!(command_value["threadId"], json!("thread_abc123"));
        assert_eq!(command_value["processId"], json!(42));
        assert_eq!(command_value["program"], json!("codex"));
        assert_eq!(command_value["args"], json!(["exec", "-"]));
        assert_eq!(command_value["cwd"], json!("C:/tmp/pedelec/thread_abc123"));
        assert_eq!(command_value["prompt"], json!("full provider prompt"));
        assert!(command_value.get("thread_id").is_none());
        assert!(command_value.get("process_id").is_none());
        assert!(command_value.get("stdin").is_none());
        assert!(command_value.get("env").is_none());

        let provider_error = ThreadEvent::Error {
            seq: 5,
            thread_id: "thread_abc123".into(),
            source: ThreadErrorSource::Provider {
                provider: ProviderCode::Codex,
            },
            error: PedelecError::new("PROVIDER_COMMAND_FAILED", "provider command failed"),
        };
        let provider_error_value = serde_json::to_value(provider_error).unwrap();
        assert_eq!(provider_error_value["type"], json!("error"));
        assert_eq!(provider_error_value["threadId"], json!("thread_abc123"));
        assert_eq!(provider_error_value["source"], json!("provider"));
        assert_eq!(provider_error_value["provider"], json!("codex"));
        assert!(provider_error_value["error"].get("source").is_none());

        let core_error = ThreadEvent::Error {
            seq: 6,
            thread_id: "thread_abc123".into(),
            source: ThreadErrorSource::Core,
            error: PedelecError::new("INTERNAL_ERROR", "Pedelec internal operation failed"),
        };
        let core_error_value = serde_json::to_value(core_error).unwrap();
        assert_eq!(core_error_value["source"], json!("core"));
        assert!(core_error_value.get("provider").is_none());
    }

    #[test]
    fn all_thread_subscription_receives_later_events_without_crossing_thread_subscription() {
        let mut event_bus = EventBus::default();
        let all_rx = event_bus.subscribe_all();
        let thread_one_rx = event_bus.subscribe("thread_one");

        event_bus.emit_created("thread_one");
        event_bus.emit_created("thread_two");

        let first_all = all_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second_all = all_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first_all,
            ThreadEvent::Created {
                thread_id,
                ..
            } if thread_id == "thread_one"
        ));
        assert!(matches!(
            second_all,
            ThreadEvent::Created {
                thread_id,
                ..
            } if thread_id == "thread_two"
        ));

        let thread_one_event = thread_one_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            thread_one_event,
            ThreadEvent::Created {
                thread_id,
                ..
            } if thread_id == "thread_one"
        ));
        assert!(thread_one_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
    }

    #[test]
    fn pedelec_error_omits_empty_details() {
        let error = PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "pedelec-app is not running",
        );
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["code"], json!("CORE_RUNTIME_UNAVAILABLE"));
        assert_eq!(value["message"], json!("pedelec-app is not running"));
        assert!(value.get("details").is_none());
    }

    #[test]
    fn workspace_creates_required_subdirectories_and_removes_them() {
        let temp = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::with_workspace_root(temp.path().join("workspace"));

        let workspace = manager.create_thread_workspace("thread_abc123").unwrap();

        assert!(workspace.exists());
        assert!(workspace_runtime_data_root(&workspace).is_dir());
        assert!(workspace_skills_root(&workspace).is_dir());
        assert!(workspace_assets_root(&workspace).is_dir());
        assert!(workspace_logs_root(&workspace).is_dir());
        assert!(workspace_tmp_root(&workspace).is_dir());
        assert!(!workspace.join(".pedelec-sandbox").exists());
        for subdir in ["skills", "assets", "logs", "tmp"] {
            assert!(
                !workspace.join(subdir).exists(),
                "unexpected root subdir {subdir}"
            );
        }

        manager.remove_thread_workspace(&workspace).unwrap();
        assert!(!workspace.exists());
    }

    #[test]
    fn default_workspace_manager_uses_the_new_managed_root() {
        let root = WorkspaceManager::default().workspace_root().unwrap();

        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some("workspaces")
        );
        assert_ne!(
            root.file_name().and_then(|name| name.to_str()),
            Some("sandbox")
        );
    }

    #[test]
    fn workspace_cleanup_does_not_touch_the_legacy_sandbox_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspaces");
        let legacy_root = temp.path().join("sandbox");
        fs::create_dir_all(workspace_root.join("thread_current")).unwrap();
        fs::create_dir_all(legacy_root.join("thread_legacy")).unwrap();
        let manager = WorkspaceManager::with_workspace_root(&workspace_root);

        assert!(manager.remove_all_thread_workspaces().is_empty());
        assert!(!workspace_root.join("thread_current").exists());
        assert!(legacy_root.join("thread_legacy").exists());
    }

    #[test]
    fn workspace_rollback_removes_partial_workspace_after_skill_download_failure() {
        let temp = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::with_workspace_root(temp.path().join("workspace"));
        let skill_manager = SkillManager::default();
        let bad_urls = vec!["http://example.com/tools.md".to_string()];

        let result = manager.create_thread_workspace_with("thread_rollback", |workspace| {
            skill_manager.download_skills(workspace_skills_root(workspace), &bad_urls)
        });

        assert_eq!(result.unwrap_err().code, error_codes::SKILL_URL_INVALID);
        assert!(!temp
            .path()
            .join("workspace")
            .join("thread_rollback")
            .exists());
    }

    #[test]
    fn custom_workspace_validates_paths_and_preserves_existing_workspace_content() {
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("managed");
        let manager = WorkspaceManager::with_workspace_root(&managed_root);
        let custom = temp.path().join("project");
        fs::create_dir_all(custom.join("skills")).unwrap();
        fs::create_dir_all(custom.join("assets")).unwrap();
        fs::create_dir_all(custom.join("logs")).unwrap();
        fs::create_dir_all(custom.join("tmp")).unwrap();
        fs::create_dir_all(workspace_runtime_data_root(&custom).join("skills")).unwrap();
        fs::write(
            workspace_runtime_data_root(&custom).join("keep.txt"),
            "keep",
        )
        .unwrap();
        fs::write(custom.join("source.txt"), "keep").unwrap();
        fs::write(custom.join("skills").join("keep.txt"), "keep").unwrap();
        fs::write(custom.join("assets").join("existing.bin"), b"keep").unwrap();

        let resolved = manager.prepare_custom_workspace(&custom).unwrap();
        assert_eq!(resolved, custom.canonicalize().unwrap());
        assert!(workspace_runtime_data_root(&resolved).is_dir());
        assert!(workspace_skills_root(&resolved).is_dir());
        assert!(workspace_assets_root(&resolved).is_dir());
        assert!(workspace_logs_root(&resolved).is_dir());
        assert!(workspace_tmp_root(&resolved).is_dir());
        assert_eq!(
            fs::read_to_string(resolved.join("source.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_to_string(resolved.join("skills").join("keep.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read(resolved.join("assets").join("existing.bin")).unwrap(),
            b"keep"
        );
        assert_eq!(
            fs::read_to_string(workspace_runtime_data_root(&resolved).join("keep.txt")).unwrap(),
            "keep"
        );
        for subdir in ["skills", "assets", "logs", "tmp"] {
            assert!(
                resolved.join(subdir).is_dir(),
                "existing root directory {subdir} was not preserved"
            );
        }

        let file_path = temp.path().join("not-a-directory");
        fs::write(&file_path, "file").unwrap();
        assert_eq!(
            manager
                .prepare_custom_workspace(&file_path)
                .unwrap_err()
                .code,
            error_codes::WORKSPACE_PATH_INVALID
        );
        assert_eq!(
            manager
                .prepare_custom_workspace(Path::new("relative-project"))
                .unwrap_err()
                .code,
            error_codes::WORKSPACE_PATH_INVALID
        );

        for overlap in [
            managed_root.clone(),
            managed_root.join("nested"),
            temp.path().to_path_buf(),
        ] {
            let error = manager.prepare_custom_workspace(overlap).unwrap_err();
            assert_eq!(error.code, error_codes::WORKSPACE_PATH_INVALID);
            assert!(error.details.unwrap()["managedWorkspaceRoot"].is_string());
        }

        let sibling = manager
            .prepare_custom_workspace(temp.path().join("sibling"))
            .unwrap();
        assert!(sibling.is_dir());
    }

    #[test]
    fn custom_workspace_rejects_a_private_data_path_that_is_not_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("managed");
        let manager = WorkspaceManager::with_workspace_root(&managed_root);
        let custom = temp.path().join("project");
        fs::create_dir_all(&custom).unwrap();
        fs::write(workspace_runtime_data_root(&custom), "not a directory").unwrap();
        fs::write(custom.join("keep.txt"), "keep").unwrap();

        let error = manager.prepare_custom_workspace(&custom).unwrap_err();

        assert_eq!(error.code, error_codes::WORKSPACE_CREATE_FAILED);
        assert!(workspace_runtime_data_root(&custom).is_file());
        assert_eq!(fs::read_to_string(custom.join("keep.txt")).unwrap(), "keep");
    }

    #[test]
    fn custom_workspace_generated_skills_merge_and_overwrite_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("project");
        fs::create_dir_all(custom.join("skills")).unwrap();
        fs::write(
            custom.join("skills").join("tools-get_app_state.json"),
            "ROOT",
        )
        .unwrap();
        fs::create_dir_all(workspace_skills_root(&custom)).unwrap();
        fs::write(
            workspace_skills_root(&custom).join("tools-get_app_state.json"),
            "OLD",
        )
        .unwrap();
        fs::write(workspace_skills_root(&custom).join("unrelated.txt"), "KEEP").unwrap();
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom.clone(),
                }),
            })
            .unwrap();

        let workspace = runtime.thread_workspace_path(&output.thread_id).unwrap();
        assert_eq!(workspace, custom.canonicalize().unwrap());
        let generated =
            fs::read_to_string(workspace_skills_root(&workspace).join("tools-get_app_state.json"))
                .unwrap();
        assert!(generated.contains("get_app_state"));
        assert_ne!(generated, "OLD");
        assert_eq!(
            fs::read_to_string(workspace_skills_root(&workspace).join("unrelated.txt")).unwrap(),
            "KEEP"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("skills").join("tools-get_app_state.json")).unwrap(),
            "ROOT"
        );
    }

    #[test]
    fn sdk_custom_workspace_creates_normalized_write_once_marker() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("project");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };
        let input = || CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: None,
            skills: None,
            workspace: Some(CreateThreadWorkspaceInput {
                path: custom.clone(),
            }),
        };

        runtime
            .create_sdk_thread(input(), "https://Example.com:443", Some("mock-sdk-version"))
            .unwrap();
        let marker = workspace_metadata_path(&custom);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&marker).unwrap()).unwrap(),
            json!({
                "sdk-version": "mock-sdk-version",
                "origin": "https://example.com",
            })
        );
        assert!(!custom.join(".pedelec-lock.json").exists());

        fs::write(&marker, "not json").unwrap();
        runtime
            .create_sdk_thread(input(), "https://other.example", Some("9.9.9"))
            .unwrap();
        assert_eq!(fs::read_to_string(marker).unwrap(), "not json");
    }

    #[test]
    fn sdk_custom_workspace_does_not_leave_marker_when_initialization_or_marker_creation_fails() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("project");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };

        let invalid_skills = CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: "bad".into(),
                tools: vec![CreateThreadToolInput {
                    name: "bad/name".into(),
                    description: "Bad.".into(),
                    args_schema: json!({ "type": "object" }),
                    timeout_ms: None,
                }],
            }),
            workspace: Some(CreateThreadWorkspaceInput {
                path: custom.clone(),
            }),
        };
        assert_eq!(
            runtime
                .create_sdk_thread(
                    invalid_skills,
                    "https://example.com",
                    Some("mock-sdk-version")
                )
                .unwrap_err()
                .code,
            error_codes::TOOLS_MANIFEST_INVALID
        );
        assert!(!workspace_metadata_path(&custom).exists());

        let marker = workspace_metadata_path(&custom);
        fs::create_dir_all(&marker).unwrap();
        let result = runtime.create_sdk_thread(
            CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom.clone(),
                }),
            },
            "https://example.com",
            Some("mock-sdk-version"),
        );
        assert_eq!(
            result.unwrap_err().code,
            error_codes::WORKSPACE_CREATE_FAILED
        );
        assert!(marker.is_dir());
        assert!(runtime.thread_manager.thread("thread_000001").is_err());
    }

    #[test]
    fn multiple_sessions_share_custom_workspace_with_unique_event_logs() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("shared-project");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };

        let first = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom.clone(),
                }),
            })
            .unwrap();
        let second = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Claude,
                effort_level: None,
                skills: None,
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom.clone(),
                }),
            })
            .unwrap();

        let first_workspace = runtime.thread_workspace_path(&first.thread_id).unwrap();
        let second_workspace = runtime.thread_workspace_path(&second.thread_id).unwrap();
        assert_eq!(first_workspace, second_workspace);
        let first_log = runtime.event_log_path(&first.thread_id).unwrap();
        let second_log = runtime.event_log_path(&second.thread_id).unwrap();
        assert_ne!(first_log, second_log);
        assert_eq!(first_log.parent(), second_log.parent());
        assert!(first_log
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&first.thread_id));
        assert!(second_log
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&second.thread_id));
        assert!(!custom.join("logs").join("events.jsonl").exists());
        assert!(workspace_logs_root(&custom).is_dir());
        assert!(fs::read_to_string(first_log).unwrap().contains("created"));
        assert!(fs::read_to_string(second_log).unwrap().contains("created"));
    }

    #[test]
    fn custom_workspace_initialization_failure_does_not_delete_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("project");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join("sentinel.txt"), "keep").unwrap();
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
            ..CoreRuntime::default()
        };

        let result = runtime.create_thread(CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: None,
            skills: Some(CreateThreadSkillsInput {
                guidance: "bad".into(),
                tools: vec![CreateThreadToolInput {
                    name: "bad/name".into(),
                    description: "Bad.".into(),
                    args_schema: json!({ "type": "object" }),
                    timeout_ms: None,
                }],
            }),
            workspace: Some(CreateThreadWorkspaceInput {
                path: custom.clone(),
            }),
        });

        assert_eq!(
            result.unwrap_err().code,
            error_codes::TOOLS_MANIFEST_INVALID
        );
        assert!(custom.exists());
        assert_eq!(
            fs::read_to_string(custom.join("sentinel.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn cleanup_removes_managed_workspaces_but_preserves_custom_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let managed_root = temp.path().join("managed");
        let custom = temp.path().join("external-project");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&managed_root),
            ..CoreRuntime::default()
        };
        let custom_thread = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: Some(CreateThreadWorkspaceInput {
                    path: custom.clone(),
                }),
            })
            .unwrap();
        runtime
            .workspace_manager
            .create_thread_workspace("t000001")
            .unwrap();
        fs::write(custom.join("keep.txt"), "keep").unwrap();

        assert!(runtime.cleanup_for_app_exit().is_empty());
        assert!(!managed_root.join("t000001").exists());
        assert!(custom.exists());
        assert!(custom.join("keep.txt").exists());
        assert_eq!(
            runtime.thread_status(&custom_thread.thread_id),
            Some(ThreadStatus::Ended)
        );

        let startup_runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&managed_root),
            ..CoreRuntime::default()
        };
        assert!(startup_runtime
            .cleanup_stale_workspaces_for_app_start()
            .is_empty());
        assert!(custom.exists());
    }

    #[test]
    fn skill_url_validation_accepts_https_and_loopback_http() {
        for url in [
            "https://example.com/tools.md",
            "http://localhost/tools.md",
            "http://127.0.0.1/tools.md",
            "http://[::1]/tools.md",
        ] {
            assert!(validate_skill_url_and_filename(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn skill_url_validation_rejects_disallowed_sources_and_traversal() {
        for url in [
            "http://example.com/tools.md",
            "file:///tmp/tools.md",
            "tools.md",
            "https://example.com/../tools.md",
            "https://example.com/%2e%2e/tools.md",
        ] {
            assert_eq!(
                validate_skill_url_and_filename(url).unwrap_err().code,
                error_codes::SKILL_URL_INVALID,
                "{url}"
            );
        }
    }

    #[test]
    fn skill_download_adds_duplicate_suffix_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        let (base_url, handle) = start_test_http_server(vec![
            ("/a/tools.md", b"first".to_vec()),
            ("/b/tools.md", b"second".to_vec()),
        ]);
        let urls = vec![
            format!("{base_url}/a/tools.md"),
            format!("{base_url}/b/tools.md"),
        ];

        let skills = SkillManager::default()
            .download_skills(&skills_dir, &urls)
            .unwrap();
        handle.join().unwrap();

        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].original_filename, "tools.md");
        assert_eq!(skills[1].original_filename, "tools.md");
        assert_eq!(skills[0].local_path.file_name().unwrap(), "tools.md");
        assert_eq!(skills[1].local_path.file_name().unwrap(), "tools_1.md");
        assert_eq!(skills[0].size_bytes, 5);
        assert_eq!(skills[1].size_bytes, 6);
        assert_eq!(
            skills[0].sha256,
            "a7937b64b8caa58f03721bb6bacf5c78cb235febe0e70b1b84cd99541461a08e"
        );
        assert_eq!(
            fs::read_to_string(skills_dir.join("tools.md")).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(skills_dir.join("tools_1.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn missing_tools_json_returns_empty_registry() {
        let temp = tempfile::tempdir().unwrap();
        let registry = ToolRegistry::load_from_skills_dir(temp.path()).unwrap();

        assert_eq!(
            registry
                .validate_tool_call("missing", &json!({}))
                .unwrap_err()
                .code,
            error_codes::TOOL_NOT_FOUND
        );
    }

    #[test]
    fn invalid_tools_json_returns_invalid() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("tools.json"), "{").unwrap();

        let err = ToolRegistry::load_from_skills_dir(temp.path()).unwrap_err();

        assert_eq!(err.code, error_codes::TOOLS_JSON_INVALID);
    }

    #[test]
    fn tool_registry_validates_tool_calls_and_schema() {
        let registry = ToolRegistry::from_tools_json_str(
            r#"{
                "tools": [
                    {
                        "name": "update_counter",
                        "description": "Update counter.",
                        "argsSchema": {
                            "type": "object",
                            "properties": {
                                "delta": { "type": "integer" }
                            },
                            "required": ["delta"],
                            "additionalProperties": false
                        },
                        "timeoutMs": 1234
                    },
                    {
                        "name": "get_app_state",
                        "description": "Read state.",
                        "argsSchema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            registry
                .validate_tool_call("update_counter", &json!({ "delta": 1 }))
                .unwrap(),
            1234
        );
        assert_eq!(
            registry
                .validate_tool_call("get_app_state", &json!({}))
                .unwrap(),
            DEFAULT_TOOL_TIMEOUT_MS
        );
        assert_eq!(
            registry
                .validate_tool_call("missing", &json!({}))
                .unwrap_err()
                .code,
            error_codes::TOOL_NOT_FOUND
        );
        assert_eq!(
            registry
                .validate_tool_call("update_counter", &json!(null))
                .unwrap_err()
                .code,
            error_codes::TOOL_ARGS_INVALID
        );
        assert_eq!(
            registry
                .validate_tool_call("update_counter", &json!({ "delta": "1" }))
                .unwrap_err()
                .code,
            error_codes::TOOL_ARGS_INVALID
        );
    }

    #[test]
    fn tool_call_timeout_override_strips_control_arg_when_schema_omits_it() {
        let registry = ToolRegistry::from_tools_json_str(
            r#"{
                "tools": [{
                    "name": "get_app_state",
                    "description": "Read state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    },
                    "timeoutMs": 5000
                }]
            }"#,
        )
        .unwrap();

        let normalized = registry
            .normalize_tool_call("get_app_state", &json!({ "timeoutMs": 25 }))
            .unwrap();

        assert_eq!(normalized.timeout_ms, 25);
        assert_eq!(normalized.args, json!({}));
    }

    #[test]
    fn tool_call_timeout_override_preserves_arg_when_schema_defines_it() {
        let registry = ToolRegistry::from_tools_json_str(
            r#"{
                "tools": [{
                    "name": "wait_for_counter",
                    "description": "Wait for state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {
                            "timeoutMs": { "type": "integer" }
                        },
                        "required": ["timeoutMs"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 5000
                }]
            }"#,
        )
        .unwrap();

        let normalized = registry
            .normalize_tool_call("wait_for_counter", &json!({ "timeoutMs": 25 }))
            .unwrap();

        assert_eq!(normalized.timeout_ms, 25);
        assert_eq!(normalized.args, json!({ "timeoutMs": 25 }));
    }

    #[test]
    fn tool_call_timeout_override_must_be_positive_integer() {
        let registry = ToolRegistry::from_tools_json_str(
            r#"{
                "tools": [{
                    "name": "get_app_state",
                    "description": "Read state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        )
        .unwrap();

        for invalid_timeout in [json!(0), json!(-1), json!(1.5), json!("100")] {
            let err = registry
                .normalize_tool_call("get_app_state", &json!({ "timeoutMs": invalid_timeout }))
                .unwrap_err();
            assert_eq!(err.code, error_codes::TOOL_ARGS_INVALID);
        }
    }

    #[test]
    fn begin_tool_call_uses_normalized_args_for_pending_request_and_event() {
        let mut runtime = runtime_with_tool_thread(
            "thread_normalized",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "get_app_state",
                    "description": "Read state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    },
                    "timeoutMs": 5000
                }]
            }"#,
        );
        let event_rx = runtime.event_bus.subscribe("thread_normalized");

        let wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_normalized".into(),
                tool_name: "get_app_state".into(),
                args: json!({ "timeoutMs": 25 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let request_id = wait.request_id;

        assert_eq!(wait.timeout_ms, 25);
        assert_eq!(
            runtime
                .tool_request_broker
                .get(&request_id)
                .unwrap()
                .request
                .args,
            json!({})
        );
        let status_event = event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            status_event,
            ThreadEvent::StatusChanged {
                status: ThreadStatus::WaitingToolResult,
                ..
            }
        ));
        let tool_event = event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            tool_event,
            ThreadEvent::ToolCall { args, .. } if args == json!({})
        ));
    }

    #[test]
    fn exact_tool_call_retry_joins_existing_invocation_without_emitting_event() {
        let mut runtime = runtime_with_tool_thread(
            "thread_join",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": { "value": { "type": "integer" } },
                        "required": ["value"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 1000
                }]
            }"#,
        );
        let event_rx = runtime.event_bus.subscribe("thread_join");

        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_join".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let first_request_id = first_wait.request_id.clone();

        let second = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_join".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap();
        let second_wait = match second {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => panic!("retry created a new tool call"),
            ToolInvocationRegistration::Replayed(_) => panic!("retry replayed a result"),
        };

        assert_eq!(second_wait.request_id, first_request_id);
        assert_eq!(runtime.tool_request_broker.pending_count(), 1);
        assert_eq!(
            runtime
                .tool_request_broker
                .waiter_count(&second_wait.request_id),
            Some(2)
        );
        assert!(matches!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolCall { .. }))
                .count(),
            1
        ));

        drop(first_wait);
        drop(second_wait);
    }

    #[test]
    fn normalized_args_define_identity_and_preserve_original_timeout() {
        let mut runtime = runtime_with_tool_thread(
            "thread_normalized_join",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {
                            "a": { "type": "integer" },
                            "b": { "type": "integer" }
                        },
                        "required": ["a", "b"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 1000
                }]
            }"#,
        );

        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_normalized_join".into(),
                tool_name: "tool_x".into(),
                args: json!({ "a": 1, "b": 2, "timeoutMs": 1000 }),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let first_request_id = first_wait.request_id.clone();
        let first_remaining = first_wait.remaining_timeout;

        let second = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_normalized_join".into(),
                tool_name: "tool_x".into(),
                args: json!({ "timeoutMs": 2000, "b": 2, "a": 1 }),
            })
            .unwrap();
        let second_wait = match second {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => {
                panic!("normalized retry created a new tool call")
            }
            ToolInvocationRegistration::Replayed(_) => panic!("normalized retry replayed"),
        };

        assert_eq!(second_wait.request_id, first_request_id);
        assert_eq!(second_wait.timeout_ms, 1000);
        assert!(second_wait.remaining_timeout <= first_remaining);
        assert_eq!(
            runtime
                .tool_request_broker
                .get(&second_wait.request_id)
                .unwrap()
                .request
                .args,
            json!({ "a": 1, "b": 2 })
        );
    }

    #[test]
    fn same_tool_with_different_normalized_args_is_rejected() {
        let mut runtime = runtime_with_tool_thread(
            "thread_different_args",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": { "value": { "type": "integer" } },
                        "required": ["value"],
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_different_args".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap();
        let first_request_id = match first {
            ToolInvocationRegistration::Created(wait) => wait.request_id,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };

        let error = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_different_args".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 2 }),
            })
            .unwrap_err();

        assert_eq!(error.code, error_codes::PENDING_TOOL_REQUEST_EXISTS);
        assert_eq!(runtime.tool_request_broker.pending_count(), 1);
        assert_eq!(
            runtime
                .tool_request_broker
                .get(&first_request_id)
                .unwrap()
                .request
                .args,
            json!({ "value": 1 })
        );
    }

    #[test]
    fn different_tool_with_same_args_is_rejected() {
        let mut runtime = runtime_with_tool_thread(
            "thread_different_tool",
            ThreadStatus::Running,
            r#"{
                "tools": [
                    {
                        "name": "tool_a",
                        "description": "Test tool.",
                        "argsSchema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    },
                    {
                        "name": "tool_b",
                        "description": "Test tool.",
                        "argsSchema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    }
                ]
            }"#,
        );
        runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_different_tool".into(),
                tool_name: "tool_a".into(),
                args: json!({}),
            })
            .unwrap();

        let error = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_different_tool".into(),
                tool_name: "tool_b".into(),
                args: json!({}),
            })
            .unwrap_err();

        assert_eq!(error.code, error_codes::PENDING_TOOL_REQUEST_EXISTS);
        assert_eq!(runtime.tool_request_broker.pending_count(), 1);
    }

    #[test]
    fn same_tool_and_args_on_different_threads_create_separate_invocations() {
        let tools_json = r#"{
            "tools": [{
                "name": "tool_x",
                "description": "Test tool.",
                "argsSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"],
                    "additionalProperties": false
                }
            }]
        }"#;
        let mut runtime = runtime_with_tool_thread("thread_one", ThreadStatus::Running, tools_json);
        add_tool_thread(
            &mut runtime,
            "thread_two",
            ThreadStatus::Running,
            tools_json,
        );

        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_one".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap();
        let first_request_id = match first {
            ToolInvocationRegistration::Created(wait) => wait.request_id,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let second = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_two".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap();
        let second_request_id = match second {
            ToolInvocationRegistration::Created(wait) => wait.request_id,
            ToolInvocationRegistration::Joined(_) => panic!("different thread joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("different thread replayed"),
        };

        assert_ne!(first_request_id, second_request_id);
        assert_eq!(runtime.tool_request_broker.pending_count(), 2);
    }

    #[test]
    fn multiple_waiters_receive_the_same_result_and_one_tool_result_event() {
        let mut runtime = runtime_with_tool_thread(
            "thread_broadcast",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let event_rx = runtime.event_bus.subscribe("thread_broadcast");
        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_broadcast".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let request_id = first_wait.request_id.clone();
        let second_wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_broadcast".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => panic!("retry created a new tool call"),
            ToolInvocationRegistration::Replayed(_) => panic!("retry replayed a result"),
        };

        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_broadcast".into(),
                request_id,
                result: json!({ "answer": 42 }),
            })
            .unwrap();

        let expected = ToolInvocationOutcome::Result(json!({ "answer": 42 }));
        assert_eq!(first_wait.result_rx.recv().unwrap(), expected);
        assert_eq!(second_wait.result_rx.recv().unwrap(), expected);
        assert_eq!(runtime.tool_request_broker.pending_count(), 0);
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolResult { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn dropped_waiter_does_not_affect_later_waiter() {
        let mut runtime = runtime_with_tool_thread(
            "thread_dropped_waiter",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let event_rx = runtime.event_bus.subscribe("thread_dropped_waiter");
        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_dropped_waiter".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let request_id = first_wait.request_id.clone();
        drop(first_wait.result_rx);

        let second_wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_dropped_waiter".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => panic!("retry created a new tool call"),
            ToolInvocationRegistration::Replayed(_) => panic!("retry replayed a result"),
        };

        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_dropped_waiter".into(),
                request_id,
                result: json!({ "ok": true }),
            })
            .unwrap();

        assert_eq!(
            second_wait.result_rx.recv().unwrap(),
            ToolInvocationOutcome::Result(json!({ "ok": true }))
        );
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolResult { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn joining_does_not_reset_the_original_invocation_deadline() {
        let mut runtime = runtime_with_tool_thread(
            "thread_deadline",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_deadline".into(),
                tool_name: "tool_x".into(),
                args: json!({ "timeoutMs": 300 }),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let first_request_id = first_wait.request_id.clone();
        thread::sleep(Duration::from_millis(80));

        let second_wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_deadline".into(),
                tool_name: "tool_x".into(),
                args: json!({ "timeoutMs": 9999 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => panic!("retry created a new tool call"),
            ToolInvocationRegistration::Replayed(_) => panic!("retry replayed a result"),
        };

        assert!(second_wait.remaining_timeout < first_wait.remaining_timeout);
        assert!(second_wait.remaining_timeout < Duration::from_millis(250));
        runtime.timeout_tool_call(&first_request_id);

        let timeout = ToolInvocationOutcome::CoreError(PedelecError::new(
            error_codes::TOOL_TIMEOUT,
            "tool timeout",
        ));
        assert_eq!(first_wait.result_rx.recv().unwrap(), timeout);
        assert_eq!(second_wait.result_rx.recv().unwrap(), timeout);
        assert_eq!(runtime.tool_request_broker.pending_count(), 0);
        assert_eq!(
            runtime.thread_status("thread_deadline"),
            Some(ThreadStatus::Running)
        );
    }

    #[test]
    fn clear_thread_drops_all_pending_waiters() {
        let mut runtime = runtime_with_tool_thread(
            "thread_clear_waiters",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let first = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_clear_waiters".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap();
        let first_wait = match first {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let request_id = first_wait.request_id.clone();
        let second_wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_clear_waiters".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Joined(wait) => wait,
            ToolInvocationRegistration::Created(_) => panic!("retry created a new tool call"),
            ToolInvocationRegistration::Replayed(_) => panic!("retry replayed a result"),
        };

        runtime
            .tool_request_broker
            .clear_thread("thread_clear_waiters");

        assert_eq!(runtime.tool_request_broker.pending_count(), 0);
        assert!(!runtime
            .tool_request_broker
            .has_pending_for_thread("thread_clear_waiters"));
        assert!(runtime.tool_request_broker.get(&request_id).is_none());
        assert_eq!(
            first_wait.result_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(
            second_wait
                .result_rx
                .recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn undelivered_completed_result_replays_until_delivery_is_acknowledged() {
        let mut runtime = runtime_with_tool_thread(
            "thread_replay_result",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                }]
            }"#,
        );
        let event_rx = runtime.event_bus.subscribe("thread_replay_result");
        let first = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_result".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("first call did not create an invocation"),
        };
        let request_id = first.request_id.clone();
        drop(first);

        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_replay_result".into(),
                request_id: request_id.clone(),
                result: json!({ "structured": [1, { "ok": true }] }),
            })
            .unwrap();
        assert_eq!(runtime.tool_request_broker.replay_candidate_count(), 1);

        let replay = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_result".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Replayed(wait) => wait,
            _ => panic!("expected a replayed invocation"),
        };
        assert_eq!(replay.request_id, request_id);
        assert_eq!(
            replay.result_rx.recv().unwrap(),
            ToolInvocationOutcome::Result(json!({ "structured": [1, { "ok": true }] }))
        );
        assert_eq!(runtime.tool_request_broker.replay_candidate_count(), 1);

        runtime
            .tool_request_broker
            .acknowledge_tool_delivery(&request_id);
        assert_eq!(runtime.tool_request_broker.replay_candidate_count(), 0);

        let next = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_result".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap();
        assert!(matches!(next, ToolInvocationRegistration::Created(_)));
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, ThreadEvent::ToolCall { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn completed_replay_identity_includes_thread_tool_and_normalized_args() {
        let tools_json = r#"{
            "tools": [
                {
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": { "value": { "type": "integer" } },
                        "required": ["value"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "tool_a",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                },
                {
                    "name": "tool_b",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }
            ]
        }"#;
        let mut runtime = runtime_with_tool_thread(
            "thread_replay_identity_a",
            ThreadStatus::Running,
            tools_json,
        );
        add_tool_thread(
            &mut runtime,
            "thread_replay_identity_b",
            ThreadStatus::Running,
            tools_json,
        );

        let original = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_identity_a".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("original call did not create an invocation"),
        };
        let original_request_id = original.request_id.clone();
        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_replay_identity_a".into(),
                request_id: original_request_id.clone(),
                result: json!({ "value": 1 }),
            })
            .unwrap();
        assert_eq!(
            original.result_rx.recv().unwrap(),
            ToolInvocationOutcome::Result(json!({ "value": 1 }))
        );

        let different_args = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_identity_a".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 2 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("different args replayed the old result"),
        };
        assert_ne!(different_args.request_id, original_request_id);
        runtime.timeout_tool_call(&different_args.request_id);
        drop(different_args);

        let different_tool = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_identity_a".into(),
                tool_name: "tool_a".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("different tool replayed the old result"),
        };
        assert_ne!(different_tool.request_id, original_request_id);
        runtime.timeout_tool_call(&different_tool.request_id);
        drop(different_tool);

        let different_thread = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_identity_b".into(),
                tool_name: "tool_x".into(),
                args: json!({ "value": 1 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("different thread replayed the old result"),
        };
        assert_ne!(different_thread.request_id, original_request_id);
    }

    #[test]
    fn completed_replay_uses_normalized_args_and_original_timeout() {
        let mut runtime = runtime_with_tool_thread(
            "thread_replay_normalized",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {
                            "a": { "type": "integer" },
                            "b": { "type": "integer" }
                        },
                        "required": ["a", "b"],
                        "additionalProperties": false
                    },
                    "timeoutMs": 1000
                }]
            }"#,
        );
        let original = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_normalized".into(),
                tool_name: "tool_x".into(),
                args: json!({ "a": 1, "b": 2, "timeoutMs": 1000 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("original call did not create an invocation"),
        };
        let original_request_id = original.request_id.clone();
        let original_timeout_ms = original.timeout_ms;
        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_replay_normalized".into(),
                request_id: original_request_id.clone(),
                result: json!({ "ok": true }),
            })
            .unwrap();
        assert_eq!(
            original.result_rx.recv().unwrap(),
            ToolInvocationOutcome::Result(json!({ "ok": true }))
        );

        let replay = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_normalized".into(),
                tool_name: "tool_x".into(),
                args: json!({ "timeoutMs": 5000, "b": 2, "a": 1 }),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Replayed(wait) => wait,
            _ => panic!("normalized retry did not replay the completed result"),
        };
        assert_eq!(replay.request_id, original_request_id);
        assert_eq!(replay.timeout_ms, original_timeout_ms);
        assert_eq!(
            replay.result_rx.recv().unwrap(),
            ToolInvocationOutcome::Result(json!({ "ok": true }))
        );
    }

    #[test]
    fn formal_timeout_is_replayable_only_when_delivery_is_unconfirmed() {
        let mut runtime = runtime_with_tool_thread(
            "thread_replay_timeout",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "tool_x",
                    "description": "Test tool.",
                    "argsSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                }]
            }"#,
        );
        let first = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_timeout".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            _ => panic!("first call did not create an invocation"),
        };
        let request_id = first.request_id.clone();
        drop(first);
        runtime.timeout_tool_call(&request_id);

        let replay = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_replay_timeout".into(),
                tool_name: "tool_x".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Replayed(wait) => wait,
            _ => panic!("expected timeout replay"),
        };
        assert_eq!(
            replay.result_rx.recv().unwrap(),
            ToolInvocationOutcome::CoreError(PedelecError::new(
                error_codes::TOOL_TIMEOUT,
                "tool timeout"
            ))
        );
        runtime
            .tool_request_broker
            .acknowledge_tool_delivery(&request_id);
        assert!(matches!(
            runtime
                .begin_tool_call(ToolCallInput {
                    thread_id: "thread_replay_timeout".into(),
                    tool_name: "tool_x".into(),
                    args: json!({}),
                })
                .unwrap(),
            ToolInvocationRegistration::Created(_)
        ));
    }

    #[test]
    fn replay_candidates_expire_and_are_bounded_by_oldest_eviction() {
        let mut runtime = CoreRuntime::default();
        for index in 0..(TOOL_RESULT_REPLAY_MAX_ENTRIES + 1) {
            let thread_id = format!("replay_cap_{index}");
            let (request_id, result_rx) = runtime
                .tool_request_broker
                .create_pending(thread_id, "tool_x".into(), json!({ "index": index }), 1000)
                .unwrap();
            drop(result_rx);
            runtime.timeout_tool_call(&request_id);
        }

        assert_eq!(
            runtime.tool_request_broker.replay_candidate_count(),
            TOOL_RESULT_REPLAY_MAX_ENTRIES
        );
        assert!(matches!(
            runtime
                .tool_request_broker
                .begin_or_join(
                    format!("replay_cap_{}", TOOL_RESULT_REPLAY_MAX_ENTRIES),
                    "tool_x".into(),
                    json!({ "index": TOOL_RESULT_REPLAY_MAX_ENTRIES }),
                    1000,
                )
                .unwrap(),
            ToolInvocationRegistration::Replayed(_)
        ));
        assert!(matches!(
            runtime
                .tool_request_broker
                .begin_or_join(
                    "replay_cap_0".into(),
                    "tool_x".into(),
                    json!({ "index": 0 }),
                    1000,
                )
                .unwrap(),
            ToolInvocationRegistration::Created(_)
        ));

        runtime
            .tool_request_broker
            .purge_expired_replay_candidates_at(
                Instant::now() + TOOL_RESULT_REPLAY_WINDOW + Duration::from_millis(1),
            );
        assert_eq!(runtime.tool_request_broker.replay_candidate_count(), 0);
    }

    #[test]
    fn submit_tool_result_with_wrong_thread_does_not_remove_pending_request() {
        let mut runtime = runtime_with_tool_thread(
            "thread_submit",
            ThreadStatus::Running,
            r#"{
                "tools": [{
                    "name": "get_app_state",
                    "description": "Read state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        );
        let wait = match runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_submit".into(),
                tool_name: "get_app_state".into(),
                args: json!({}),
            })
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("first tool call joined"),
            ToolInvocationRegistration::Replayed(_) => panic!("first tool call replayed"),
        };
        let request_id = wait.request_id.clone();
        let result_rx = wait.result_rx;

        let err = runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "wrong_thread".into(),
                request_id: request_id.clone(),
                result: json!({ "value": 1 }),
            })
            .unwrap_err();
        assert_eq!(err.code, error_codes::PENDING_TOOL_REQUEST_NOT_FOUND);
        assert!(runtime.tool_request_broker.get(&request_id).is_some());

        runtime
            .submit_tool_result(SubmitToolResultInput {
                thread_id: "thread_submit".into(),
                request_id,
                result: json!({ "value": 2 }),
            })
            .unwrap();
        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ToolInvocationOutcome::Result(json!({ "value": 2 }))
        );
    }

    #[test]
    fn tool_registry_store_saves_by_thread_id() {
        let registry = ToolRegistry::from_tools_json_str(
            r#"{
                "tools": [{
                    "name": "get_app_state",
                    "description": "Read state.",
                    "argsSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }
                }]
            }"#,
        )
        .unwrap();
        let mut store = ToolRegistryStore::default();

        store.insert("thread_abc123", registry);

        assert!(store.get("thread_abc123").is_some());
        assert!(store.remove("thread_abc123").is_some());
        assert!(store.get("thread_abc123").is_none());
    }

    #[test]
    fn list_assets_recursively_reads_regular_files_without_mutating_thread() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_assets",
            ProviderCode::Codex,
            None,
            None,
        );
        let workspace = temp.path().join("workspace/thread_assets");
        let assets = workspace_assets_root(&workspace);
        fs::create_dir_all(workspace.join("assets")).unwrap();
        fs::write(workspace.join("assets/root-only.txt"), b"user project file").unwrap();
        fs::create_dir_all(assets.join("nested/previews")).unwrap();
        fs::create_dir_all(assets.join(".cache")).unwrap();
        fs::create_dir_all(assets.join(".pedelec-cache")).unwrap();
        fs::write(assets.join("upl-report.txt"), b"data").unwrap();
        fs::write(assets.join(".env"), b"key=value").unwrap();
        fs::write(assets.join(".pedelec-internal"), b"hidden").unwrap();
        fs::write(assets.join("nested/report.json"), b"report").unwrap();
        fs::write(assets.join("nested/previews/final.png"), b"preview").unwrap();
        fs::write(assets.join("nested/.metadata.json"), b"metadata").unwrap();
        fs::write(assets.join(".cache/result.json"), b"cache").unwrap();
        fs::write(assets.join("nested/.pedelec-state.json"), b"hidden").unwrap();
        fs::write(assets.join(".pedelec-cache/hidden.txt"), b"hidden").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(assets.join("upl-report.txt"), assets.join("file-link.txt"))
                .unwrap();
            std::os::unix::fs::symlink(assets.join("nested"), assets.join("directory-link"))
                .unwrap();
        }
        #[cfg(windows)]
        {
            // Developer Mode or elevated privileges are needed for symlinks on some Windows hosts.
            let _ = std::os::windows::fs::symlink_file(
                assets.join("upl-report.txt"),
                assets.join("file-link.txt"),
            );
            let _ = std::os::windows::fs::symlink_dir(
                assets.join("nested"),
                assets.join("directory-link"),
            );
        }

        let status_before = runtime.thread_status("thread_assets");
        let output = runtime
            .list_assets(ListAssetsInput {
                thread_id: "thread_assets".into(),
            })
            .unwrap();

        assert_eq!(status_before, runtime.thread_status("thread_assets"));
        assert_eq!(output.assets.len(), 6);
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == "upl-report.txt"
                && asset.path == "/upl-report.txt"
                && asset.size_bytes == 4
                && asset.modified_at >= 0));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == ".env" && asset.path == "/.env"));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == "report.json" && asset.path == "/nested/report.json"));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == "final.png" && asset.path == "/nested/previews/final.png"));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == ".metadata.json" && asset.path == "/nested/.metadata.json"));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == "result.json" && asset.path == "/.cache/result.json"));
        assert!(output
            .assets
            .iter()
            .all(|asset| !asset.path.contains(".pedelec-")
                && !asset.path.contains("file-link")
                && !asset.path.contains("directory-link")));
        assert!(output.assets.iter().all(|asset| !asset.path.ends_with('/')));
        assert!(output.assets.windows(2).all(|pair| {
            pair[0].modified_at > pair[1].modified_at
                || (pair[0].modified_at == pair[1].modified_at && pair[0].name <= pair[1].name)
        }));
        assert!(output
            .assets
            .iter()
            .all(|asset| !asset.path.contains(temp.path().to_string_lossy().as_ref())));
        assert!(!output
            .assets
            .iter()
            .any(|asset| asset.path == "/root-only.txt"));
    }

    #[test]
    fn read_asset_does_not_fall_back_to_a_root_level_assets_directory() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace/thread_asset_isolation");
        fs::create_dir_all(workspace.join("assets")).unwrap();
        fs::write(workspace.join("assets/result.json"), b"user project file").unwrap();
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_asset_isolation".into(),
            provider: ProviderCode::Codex,
            effort_level: EffortLevel::Default,
            effort_args: vec![],
            workspace_path: workspace,
            skills: vec![],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
            sdk_origin: None,
        };

        let error = resolve_asset_file(&thread, "/result.json").unwrap_err();
        assert_eq!(error.code, error_codes::ASSET_NOT_FOUND);
    }

    #[test]
    fn list_assets_fails_when_a_nested_directory_cannot_be_read() {
        let temp = tempfile::tempdir().unwrap();
        let assets_root = workspace_assets_root(&temp.path().join("workspace/thread_assets"));
        fs::create_dir_all(&assets_root).unwrap();
        let missing_nested_directory = assets_root.join("nested");
        let mut assets = Vec::new();

        let error =
            collect_assets(&assets_root, &missing_nested_directory, &mut assets).unwrap_err();

        assert_eq!(error.code, error_codes::ASSET_LIST_FAILED);
        assert_eq!(error.details.unwrap()["path"], "/nested");
        assert!(assets.is_empty());
    }

    #[test]
    fn create_thread_generates_short_incrementing_thread_ids() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };
        let input = CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: None,
            skills: None,
            workspace: None,
        };

        let first = runtime.create_thread(input.clone()).unwrap();
        let second = runtime.create_thread(input).unwrap();

        assert_eq!(first.thread_id, "t000001");
        assert_eq!(second.thread_id, "t000002");
        assert_short_thread_id(&first.thread_id);
        assert_short_thread_id(&second.thread_id);
        assert!(workspace_root.join("t000001").exists());
        assert!(workspace_root.join("t000002").exists());
    }

    #[test]
    fn create_thread_skips_existing_short_thread_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(workspace_root.join("t000001")).unwrap();
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();

        assert_eq!(output.thread_id, "t000002");
        assert_short_thread_id(&output.thread_id);
        assert!(workspace_root.join("t000002").exists());
    }

    #[test]
    fn create_thread_skips_existing_active_short_thread_id() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: "t000001".into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: Vec::new(),
                workspace_path: workspace_root.join("t000001"),
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderAdapterState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();

        assert_eq!(output.thread_id, "t000002");
        assert_short_thread_id(&output.thread_id);
        assert!(workspace_root.join("t000002").exists());
    }

    #[test]
    fn cleanup_for_app_exit_ends_active_threads_and_removes_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };
        let input = CreateThreadInput {
            provider: ProviderCode::Codex,
            effort_level: None,
            skills: None,
            workspace: None,
        };
        let first = runtime.create_thread(input.clone()).unwrap();
        let second = runtime.create_thread(input).unwrap();
        runtime
            .tool_request_broker
            .create_pending(first.thread_id.clone(), "echo".into(), json!({}), 1000)
            .unwrap();

        let errors = runtime.cleanup_for_app_exit();

        assert!(errors.is_empty());
        assert_eq!(
            runtime.thread_status(&first.thread_id),
            Some(ThreadStatus::Ended)
        );
        assert_eq!(
            runtime.thread_status(&second.thread_id),
            Some(ThreadStatus::Ended)
        );
        assert!(!runtime
            .tool_request_broker
            .has_pending_for_thread(&first.thread_id));
        assert!(runtime.tool_registry.get(&first.thread_id).is_none());
        assert!(runtime.tool_registry.get(&second.thread_id).is_none());
        assert!(!workspace_root.join(&first.thread_id).exists());
        assert!(!workspace_root.join(&second.thread_id).exists());
    }

    #[test]
    fn end_thread_preserves_workspace_until_app_exit_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };
        let thread = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();
        let workspace_path = runtime.thread_workspace_path(&thread.thread_id).unwrap();
        let sentinel_path = workspace_assets_root(&workspace_path).join("sentinel.txt");
        fs::write(&sentinel_path, "preserve me").unwrap();
        let event_rx = runtime.event_bus.subscribe(&thread.thread_id);
        let (request_id, result_rx) = runtime
            .tool_request_broker
            .create_pending(thread.thread_id.clone(), "echo".into(), json!({}), 1000)
            .unwrap();
        drop(result_rx);
        runtime.timeout_tool_call(&request_id);
        runtime
            .tool_request_broker
            .create_pending(
                thread.thread_id.clone(),
                "echo".into(),
                json!({ "new": true }),
                1000,
            )
            .unwrap();

        runtime
            .end_thread(EndThreadInput {
                thread_id: thread.thread_id.clone(),
            })
            .unwrap();

        assert_eq!(
            runtime.thread_status(&thread.thread_id),
            Some(ThreadStatus::Ended)
        );
        assert_eq!(runtime.active_process_id(&thread.thread_id), None);
        assert_eq!(runtime.running_process_count(), 0);
        assert!(!runtime
            .tool_request_broker
            .has_pending_for_thread(&thread.thread_id));
        assert_eq!(runtime.tool_request_broker.replay_candidate_count(), 0);
        assert!(runtime.tool_registry.get(&thread.thread_id).is_none());
        assert!(event_rx
            .try_iter()
            .any(|event| matches!(event, ThreadEvent::Ended { .. })));
        assert!(workspace_path.exists());
        assert_eq!(fs::read_to_string(&sentinel_path).unwrap(), "preserve me");

        assert!(runtime.cleanup_for_app_exit().is_empty());
        assert!(!workspace_path.exists());
    }

    #[test]
    fn cleanup_for_app_exit_removes_orphan_workspace_directories() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(workspace_logs_root(&workspace_root.join("t000999"))).unwrap();
        fs::write(workspace_root.join("keep.txt"), "not a workspace").unwrap();
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let errors = runtime.cleanup_for_app_exit();

        assert!(errors.is_empty());
        assert!(!workspace_root.join("t000999").exists());
        assert!(workspace_root.join("keep.txt").exists());
    }

    #[test]
    fn startup_cleanup_removes_stale_workspace_directories_without_touching_files() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(workspace_assets_root(&workspace_root.join("t000999"))).unwrap();
        fs::write(
            workspace_assets_root(&workspace_root.join("t000999")).join("stale.txt"),
            "stale",
        )
        .unwrap();
        fs::write(workspace_root.join("keep.txt"), "not a workspace").unwrap();
        let runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        assert!(runtime.cleanup_stale_workspaces_for_app_start().is_empty());
        assert!(!workspace_root.join("t000999").exists());
        assert!(workspace_root.join("keep.txt").exists());
        assert!(runtime.thread_manager.thread_ids().is_empty());
    }

    #[test]
    fn startup_cleanup_succeeds_when_workspace_root_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("missing-workspace");
        let runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        assert!(runtime.cleanup_stale_workspaces_for_app_start().is_empty());
        assert!(!workspace_root.exists());
    }

    #[test]
    fn remove_all_thread_workspaces_succeeds_when_root_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("missing-workspace");
        let manager = WorkspaceManager::with_workspace_root(&workspace_root);

        let errors = manager.remove_all_thread_workspaces();

        assert!(errors.is_empty());
        assert!(!workspace_root.exists());
    }

    #[test]
    fn next_thread_id_errors_when_short_id_space_is_exhausted() {
        let mut manager = ThreadManager {
            next_thread_number: THREAD_ID_MAX_COUNTER,
            ..ThreadManager::default()
        };

        let err = manager.next_thread_id().unwrap_err();

        assert_eq!(err.code, error_codes::WORKSPACE_CREATE_FAILED);
    }

    #[test]
    fn create_thread_generates_per_tool_specs_without_tools_md() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();

        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        let skills_dir = workspace_skills_root(&thread.workspace_path);
        let spec = fs::read_to_string(skills_dir.join("tools-get_app_state.json")).unwrap();

        assert!(!skills_dir.join("tools.md").exists());
        assert!(spec.contains("\"name\": \"get_app_state\""));
        assert!(!skills_dir.join("tools.json").exists());
        assert!(!skills_dir.join("pedelec-cli.md").exists());
        assert!(!thread.skills.iter().any(|skill| {
            skill.original_filename == "pedelec-cli.md"
                || skill.original_url == "builtin:pedelec-cli.md"
        }));
        assert!(!thread
            .skills
            .iter()
            .any(|skill| skill.original_url == "generated:tools.md"));
        assert!(thread
            .skills
            .iter()
            .any(|skill| { skill.original_url == "generated:tools-get_app_state.json" }));
    }

    #[test]
    fn tool_spec_reads_from_in_memory_registry() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();
        fs::write(
            workspace_skills_root(&workspace_root.join(&output.thread_id))
                .join("tools-get_app_state.json"),
            "{}",
        )
        .unwrap();

        let spec = runtime
            .tool_spec(ToolSpecInput {
                thread_id: output.thread_id,
                tool_name: "get_app_state".into(),
            })
            .unwrap();

        assert_eq!(spec.name, "get_app_state");
        assert_eq!(spec.description, "Read state.");
        assert_eq!(spec.timeout_ms, DEFAULT_TOOL_TIMEOUT_MS);
    }

    #[test]
    fn create_thread_registers_idle_state_without_starting_process_and_logs_events() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();

        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.process_id, None);
        assert_eq!(runtime.running_process_count(), 0);

        let log_path = runtime.event_log_path(&output.thread_id).unwrap();
        let log = fs::read_to_string(log_path).unwrap();
        let records = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records[0]["seq"], json!(1));
        assert_eq!(records[0]["event"]["type"], json!("created"));
        assert_eq!(records[1]["seq"], json!(2));
        assert_eq!(records[1]["event"]["type"], json!("status_changed"));
        assert_eq!(records[1]["event"]["status"], json!("idle"));
    }

    #[test]
    fn create_thread_without_skills_creates_empty_skills_dir() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: None,
                workspace: None,
            })
            .unwrap();

        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.process_id, None);
        assert_eq!(runtime.running_process_count(), 0);
        let skills_dir = workspace_skills_root(&thread.workspace_path);
        assert!(skills_dir.exists());
        assert!(skills_dir.is_dir());
        assert!(!skills_dir.join("tools.md").exists());
        assert!(!skills_dir.join("pedelec-cli.md").exists());
        assert!(thread.skills.is_empty());
    }

    #[test]
    fn prepare_thread_builds_prepare_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();

        let start = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: output.thread_id.clone(),
            })
            .unwrap();
        let command = start.command.unwrap();

        assert!(command.stdin.contains("[Session Preparation]"));
        assert!(command
            .args
            .iter()
            .any(|arg| arg.contains("PEDELEC_PREPARED")));
        assert!(!command.stdin.contains("After preparation is complete"));
        assert!(!command.stdin.contains("\n[User Message]\n"));
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["-c", "skills.include_instructions=false"]));
        assert_eq!(
            runtime
                .thread_manager
                .thread(&output.thread_id)
                .unwrap()
                .status,
            ThreadStatus::Running
        );
    }

    fn complete_prepare_with_codex_output(
        output: &str,
        include_session_id: bool,
    ) -> (CoreRuntime, String, Vec<ThreadEvent>) {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = format!("thread_prepare_ack_{}", Uuid::new_v4().simple());
        let mut runtime =
            runtime_with_provider_thread(temp.path(), &thread_id, ProviderCode::Codex, None, None);
        let event_rx = runtime.event_bus.subscribe(&thread_id);
        runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        runtime.register_provider_process(
            &thread_id,
            7,
            Arc::new(Mutex::new(None)),
            RunningProviderProcessPurpose::Prepare,
        );
        let line = if include_session_id {
            format!(r#"{{"sessionId":"prepare-session","text":{output:?}}}"#)
        } else {
            format!(r#"{{"text":{output:?}}}"#)
        };
        runtime.emit_provider_stdout(&thread_id, format!("{line}\n"));
        runtime.complete_provider_process(&thread_id, 7, success_exit_status());
        (runtime, thread_id, collect_available_core_events(&event_rx))
    }

    #[test]
    fn prepare_accepts_only_trimmed_exact_acknowledgment_and_keeps_prepare_chat_internal() {
        for output in [
            "PEDELEC_PREPARED",
            "PEDELEC_PREPARED\n",
            " PEDELEC_PREPARED ",
        ] {
            let (runtime, thread_id, events) = complete_prepare_with_codex_output(output, true);
            assert_eq!(runtime.thread_status(&thread_id), Some(ThreadStatus::Idle));
            assert_eq!(
                runtime
                    .provider_state(&thread_id)
                    .unwrap()
                    .provider_session_id
                    .as_deref(),
                Some("prepare-session")
            );
            assert!(!events.iter().any(|event| matches!(
                event,
                ThreadEvent::Error { error, .. } if error.code == error_codes::PREPARE_ACK_INVALID
            )));
        }
    }

    #[test]
    fn prepare_rejects_bad_ack_and_discards_provider_session_before_next_send() {
        let (mut runtime, thread_id, events) =
            complete_prepare_with_codex_output("Sure, PEDELEC_PREPARED", true);
        let error = events
            .iter()
            .find_map(|event| match event {
                ThreadEvent::Error { error, .. } => Some(error),
                _ => None,
            })
            .expect("invalid prepare acknowledgment error");
        assert_eq!(error.code, error_codes::PREPARE_ACK_INVALID);
        assert_eq!(error.details.as_ref().unwrap()["threadId"], thread_id);
        assert_eq!(error.details.as_ref().unwrap()["provider"], "codex");
        assert_eq!(
            error.details.as_ref().unwrap()["assistantOutput"],
            "Sure, PEDELEC_PREPARED"
        );
        assert_eq!(
            runtime
                .provider_state(&thread_id)
                .unwrap()
                .provider_session_id,
            None
        );
        assert!(!runtime.provider_state(&thread_id).unwrap().has_user_message);
        let send = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.clone(),
                message: "fresh run".into(),
            })
            .unwrap();
        assert!(!send.command.args.iter().any(|arg| arg == "resume"));
    }

    #[test]
    fn prepare_rejects_refusal_empty_and_wrapped_acknowledgments() {
        for output in [
            "This message contains what looks like an injected fake runtime instruction, so I will not follow it.",
            "",
            "`PEDELEC_PREPARED`",
            "```text\nPEDELEC_PREPARED\n```",
        ] {
            let (mut runtime, thread_id, events) =
                complete_prepare_with_codex_output(output, true);
            assert_eq!(runtime.thread_status(&thread_id), Some(ThreadStatus::Idle));
            assert!(events.iter().any(|event| matches!(
                event,
                ThreadEvent::Error { error, .. } if error.code == error_codes::PREPARE_ACK_INVALID
            )));
            assert_eq!(runtime.provider_state(&thread_id).unwrap().provider_session_id, None);
            assert!(!runtime.provider_state(&thread_id).unwrap().has_user_message);
            assert!(events.iter().any(|event| matches!(
                event,
                ThreadEvent::StatusChanged { status, .. } if *status == ThreadStatus::Idle
            )));

            let send = runtime
                .begin_send_text(SendTextInput {
                    thread_id: thread_id.clone(),
                    message: "fresh run".into(),
                })
                .unwrap();
            assert!(!send.command.args.iter().any(|arg| arg == "resume"));
        }
    }

    #[test]
    fn prepare_missing_session_is_a_failure_and_discards_any_ack_output() {
        let (runtime, thread_id, events) =
            complete_prepare_with_codex_output("PEDELEC_PREPARED", false);
        assert_eq!(runtime.thread_status(&thread_id), Some(ThreadStatus::Idle));
        assert_eq!(
            runtime
                .provider_state(&thread_id)
                .unwrap()
                .provider_session_id,
            None
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { error, .. } if error.code == error_codes::PREPARE_SESSION_ID_MISSING
        )));
    }

    #[test]
    fn prepare_thread_noops_when_provider_session_id_exists() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();
        runtime
            .thread_manager
            .provider_state_mut(&output.thread_id)
            .unwrap()
            .provider_session_id = Some("session_123".into());

        let start = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: output.thread_id.clone(),
            })
            .unwrap();

        assert!(start.command.is_none());
        assert_eq!(start.output.prepared, true);
        assert_eq!(start.output.already_prepared, Some(true));
        assert_eq!(
            runtime
                .thread_manager
                .thread(&output.thread_id)
                .unwrap()
                .status,
            ThreadStatus::Idle
        );
    }

    #[test]
    fn send_text_after_prepare_uses_resume_command() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(&workspace_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: None,
                skills: Some(sample_skills_input()),
                workspace: None,
            })
            .unwrap();
        let thread_id = output.thread_id.clone();
        let prepare = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: thread_id.clone(),
            })
            .unwrap();
        assert!(prepare
            .command
            .unwrap()
            .stdin
            .contains("[Session Preparation]"));
        runtime
            .thread_manager
            .provider_state_mut(&thread_id)
            .unwrap()
            .provider_session_id = Some("session_123".into());
        runtime
            .thread_manager
            .thread_mut(&thread_id)
            .unwrap()
            .status = ThreadStatus::Idle;

        let send = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.clone(),
                message: "hello".into(),
            })
            .unwrap();

        assert!(send.command.args.contains(&"resume".to_string()));
        assert!(send.command.args.contains(&"session_123".to_string()));
        assert_eq!(
            send.command.stdin,
            build_provider_user_message_task("hello")
        );

        runtime
            .thread_manager
            .thread_mut(&thread_id)
            .unwrap()
            .status = ThreadStatus::Idle;
        let second_send = runtime
            .begin_send_text(SendTextInput {
                thread_id,
                message: "again".into(),
            })
            .unwrap();
        assert_eq!(second_send.command.stdin, "again");
    }

    #[test]
    fn normal_send_text_still_rejects_ended_threads() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_normal_send_ended";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Codex,
            Some("session_existing".into()),
            None,
        );

        runtime
            .end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();

        let error = runtime
            .begin_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "normal send must remain rejected".into(),
            })
            .unwrap_err();

        assert_eq!(error.code, error_codes::THREAD_ENDED);
    }

    #[test]
    fn debug_send_text_reactivates_ended_thread_resources_and_resumes_session() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_debug_reactivate";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Codex,
            Some("session_existing".into()),
            None,
        );
        let workspace_path = runtime.thread_workspace_path(thread_id).unwrap();
        let initial_log_path = workspace_logs_root(&workspace_path).join("initial.jsonl");
        let skills_dir = workspace_skills_root(&workspace_path);
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("tools-get_app_state.json"),
            serde_json::to_string(&json!({
                "name": "get_app_state",
                "description": "Read state.",
                "argsSchema": {
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                },
                "timeoutMs": 30000
            }))
            .unwrap(),
        )
        .unwrap();
        runtime
            .event_bus
            .register_thread_log(thread_id, initial_log_path.clone());
        runtime
            .thread_manager
            .thread_mut(thread_id)
            .unwrap()
            .sdk_origin = Some("https://example.com".into());

        runtime
            .end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Ended));
        assert!(runtime.tool_registry.get(thread_id).is_none());
        assert!(runtime.event_log_path(thread_id).is_none());

        let start = runtime
            .begin_debug_send_text(SendTextInput {
                thread_id: thread_id.into(),
                message: "What did you just change?".into(),
            })
            .unwrap();

        assert_eq!(start.output.thread_id, thread_id);
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Running)
        );
        assert_eq!(
            runtime
                .provider_state(thread_id)
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("session_existing")
        );
        assert_eq!(
            runtime
                .thread_manager
                .thread(thread_id)
                .unwrap()
                .sdk_origin
                .as_deref(),
            Some("https://example.com")
        );
        assert!(runtime
            .tool_registry
            .get(thread_id)
            .unwrap()
            .get("get_app_state")
            .is_some());
        assert!(runtime.event_log_path(thread_id).is_some());
        assert_ne!(runtime.event_log_path(thread_id), Some(initial_log_path));
        assert!(start.command.args.iter().any(|arg| arg == "resume"));
        assert!(start
            .command
            .args
            .iter()
            .any(|arg| arg == "session_existing"));
        assert!(runtime
            .begin_tool_call(ToolCallInput {
                thread_id: thread_id.into(),
                tool_name: "get_app_state".into(),
                args: json!({}),
            })
            .is_ok());

        runtime.register_provider_process(
            thread_id,
            7,
            Arc::new(Mutex::new(None)),
            RunningProviderProcessPurpose::UserMessage,
        );
        runtime.complete_provider_process(thread_id, 7, success_exit_status());
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
    }

    #[test]
    fn debug_send_text_accepts_only_idle_or_ended_threads() {
        for status in [
            ThreadStatus::Idle,
            ThreadStatus::Ended,
            ThreadStatus::Running,
            ThreadStatus::WaitingToolResult,
            ThreadStatus::Stopping,
            ThreadStatus::Error,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let thread_id = format!("thread_debug_status_{status:?}");
            let mut runtime = runtime_with_provider_thread(
                temp.path(),
                &thread_id,
                ProviderCode::Codex,
                Some("session_existing".into()),
                None,
            );
            runtime.test_provider_command = Some(CommandSpec {
                program: "provider-test".into(),
                args: Vec::new(),
                cwd: temp.path().into(),
                env: Vec::new(),
                prompt: String::new(),
                stdin: "debug".into(),
            });
            runtime
                .thread_manager
                .thread_mut(&thread_id)
                .unwrap()
                .status = status.clone();

            let result = runtime.begin_debug_send_text(SendTextInput {
                thread_id: thread_id.clone(),
                message: "debug".into(),
            });
            match status {
                ThreadStatus::Idle | ThreadStatus::Ended => {
                    assert!(result.is_ok(), "status {status:?} should be accepted");
                }
                ThreadStatus::Running
                | ThreadStatus::WaitingToolResult
                | ThreadStatus::Stopping => {
                    assert_eq!(result.unwrap_err().code, error_codes::THREAD_BUSY);
                }
                ThreadStatus::Error => {
                    assert_eq!(
                        result.unwrap_err().code,
                        error_codes::PROVIDER_COMMAND_FAILED
                    );
                }
                ThreadStatus::Starting => unreachable!(),
            }
        }
    }

    fn sample_skills_input() -> CreateThreadSkillsInput {
        CreateThreadSkillsInput {
            guidance: "Use get_app_state.".into(),
            tools: vec![CreateThreadToolInput {
                name: "get_app_state".into(),
                description: "Read state.".into(),
                args_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
                timeout_ms: None,
            }],
        }
    }

    fn assert_short_thread_id(thread_id: &str) {
        assert!(thread_id.len() <= 8);
        assert!(thread_id.starts_with('t'));
        let suffix = &thread_id[1..];
        assert!(suffix.len() >= THREAD_ID_BASE36_MIN_WIDTH);
        assert!(suffix.len() <= THREAD_ID_BASE36_MAX_WIDTH);
        assert!(suffix
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch.is_ascii_lowercase()));
    }

    fn test_provider_path(root: &Path, program: &str) -> OsString {
        if program == "codex" {
            return test_codex_path(root, true);
        }
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let program_name = format!("{program}.cmd");
        #[cfg(not(windows))]
        let program_name = program.to_string();
        #[cfg(windows)]
        let contents = "@echo off\necho 9.9.9\n";
        #[cfg(not(windows))]
        let contents = "#!/bin/sh\nprintf '9.9.9\\n'\n";
        let path = bin_dir.join(program_name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        env::join_paths([bin_dir]).unwrap()
    }

    fn test_codex_path(root: &Path, app_server_supported: bool) -> OsString {
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let contents = if app_server_supported {
            "@echo off\nif \"%1\"==\"app-server\" echo app-server\nif not \"%1\"==\"app-server\" echo 9.9.9\n"
        } else {
            "@echo off\necho 9.9.9\n"
        };
        #[cfg(not(windows))]
        let contents = if app_server_supported {
            "#!/bin/sh\nif [ \"$1\" = app-server ]; then printf 'app-server\\n'; else printf '9.9.9\\n'; fi\n"
        } else {
            "#!/bin/sh\nprintf '9.9.9\\n'\n"
        };
        #[cfg(windows)]
        let program_name = "codex.cmd";
        #[cfg(not(windows))]
        let program_name = "codex";
        let path = bin_dir.join(program_name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        env::join_paths([bin_dir]).unwrap()
    }

    fn test_antigravity_path(root: &Path, version: &str, stream_json_supported: bool) -> OsString {
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let contents = if stream_json_supported {
            format!(
                "@echo off\r\nif \"%1\"==\"--help\" (echo --input-format stream-json --output-format stream-json) else (echo {version})\r\n"
            )
        } else {
            format!("@echo off\r\nif \"%1\"==\"--help\" (echo usage) else (echo {version})\r\n")
        };
        #[cfg(not(windows))]
        let contents = if stream_json_supported {
            format!(
                "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' '--input-format stream-json --output-format stream-json'; else printf '%s\\n' '{version}'; fi\n"
            )
        } else {
            format!(
                "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' 'usage'; else printf '%s\\n' '{version}'; fi\n"
            )
        };
        #[cfg(windows)]
        let program_name = "agy.cmd";
        #[cfg(not(windows))]
        let program_name = "agy";
        let path = bin_dir.join(program_name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        env::join_paths([bin_dir]).unwrap()
    }

    fn test_cursor_path(root: &Path, acp_supported: bool) -> OsString {
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let contents = if acp_supported {
            "@echo off\nif \"%1\"==\"acp\" echo acp\nif not \"%1\"==\"acp\" echo 9.9.9\n"
        } else {
            "@echo off\necho 9.9.9\n"
        };
        #[cfg(not(windows))]
        let contents = if acp_supported {
            "#!/bin/sh\nif [ \"$1\" = acp ]; then printf 'acp\\n'; else printf '9.9.9\\n'; fi\n"
        } else {
            "#!/bin/sh\nprintf '9.9.9\\n'\n"
        };
        #[cfg(windows)]
        let program_name = "cursor-agent.cmd";
        #[cfg(not(windows))]
        let program_name = "cursor-agent";
        let path = bin_dir.join(program_name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        env::join_paths([bin_dir]).unwrap()
    }

    fn start_test_http_server(
        routes: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected_requests = routes.len();
        let routes: HashMap<String, Vec<u8>> = routes
            .into_iter()
            .map(|(path, body)| (path.to_string(), body))
            .collect();

        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0; 2048];
                let bytes_read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes_read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                if let Some(body) = routes.get(path) {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                }
            }
        });

        (format!("http://{address}"), handle)
    }

    fn start_single_response_server(
        status: u16,
        body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 2048];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let reason = if status == 200 { "OK" } else { "Error" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });

        (format!("http://{address}"), handle)
    }

    fn runtime_with_tool_thread(
        thread_id: &str,
        status: ThreadStatus,
        tools_json: &str,
    ) -> CoreRuntime {
        let mut runtime = CoreRuntime::default();
        add_tool_thread(&mut runtime, thread_id, status, tools_json);
        runtime
    }

    fn add_tool_thread(
        runtime: &mut CoreRuntime,
        thread_id: &str,
        status: ThreadStatus,
        tools_json: &str,
    ) {
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: Vec::new(),
                workspace_path: PathBuf::from("workspace").join(thread_id),
                skills: vec![],
                status,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderAdapterState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_tools_json_str(tools_json).unwrap(),
        );
    }

    #[test]
    fn all_providers_parse_root_error_without_assistant_message() {
        let temp = tempfile::tempdir().unwrap();
        for provider in [
            ProviderCode::Codex,
            ProviderCode::Antigravity,
            ProviderCode::OpenCode,
            ProviderCode::Cursor,
            ProviderCode::Claude,
            ProviderCode::Ollama,
        ] {
            let thread_id = format!("thread_root_error_{provider:?}");
            let mut runtime =
                runtime_with_provider_thread(temp.path(), &thread_id, provider.clone(), None, None);
            let event_rx = runtime.event_bus.subscribe(&thread_id);
            runtime.emit_provider_stdout(
                &thread_id,
                r#"{"type":"error","message":"some error"}"#.to_string() + "\n",
            );
            let events = collect_available_core_events(&event_rx);

            assert!(events.iter().any(|event| matches!(
                event,
                ThreadEvent::Error {
                    source: ThreadErrorSource::Provider { provider: event_provider },
                    error,
                    ..
                } if event_provider == &provider
                    && error.code == error_codes::PROVIDER_COMMAND_FAILED
                    && error.message == "some error"
            )));
            assert!(!events.iter().any(|event| matches!(
                event,
                ThreadEvent::AssistantMessage { text, .. } if text == "some error"
            )));
        }
    }

    #[test]
    fn root_provider_error_preserves_nested_and_root_fields() {
        let nested = parse_root_provider_error(&json!({
            "type": "error",
            "error": { "code": "AUTH_FAILED", "message": "Token expired", "details": { "status": 401 } }
        }))
        .unwrap();
        assert_eq!(nested.code, "AUTH_FAILED");
        assert_eq!(nested.message, "Token expired");
        assert_eq!(nested.details.unwrap()["status"], 401);

        let root = parse_root_provider_error(&json!({
            "type": "error",
            "code": "RATE_LIMITED",
            "message": "Try later",
            "details": { "retryAfter": 10 }
        }))
        .unwrap();
        assert_eq!(root.code, "RATE_LIMITED");
        assert_eq!(root.message, "Try later");
        assert_eq!(root.details.unwrap()["retryAfter"], 10);
    }

    #[test]
    fn stderr_capture_keeps_utf8_tail_within_limit() {
        let mut stderr = "prefix".repeat(MAX_PROVIDER_STDERR_BYTES / 6);
        let mut truncated = false;
        append_provider_stderr(&mut stderr, &mut truncated, "最後錯誤");

        assert!(truncated);
        assert!(stderr.len() <= MAX_PROVIDER_STDERR_BYTES);
        assert!(stderr.ends_with("最後錯誤"));
        assert!(std::str::from_utf8(stderr.as_bytes()).is_ok());
    }

    fn antigravity_run_message_with_final_utf16_length(
        runtime: &CoreRuntime,
        thread_id: &str,
        target_length: usize,
        include_supplementary_character: bool,
    ) -> String {
        let thread = runtime.thread_manager.thread(thread_id).unwrap();
        let registry = runtime.tool_registry.get(thread_id).unwrap();
        let prompt_prefix = build_provider_run_prompt(thread, registry, "", false);
        let prefix_length = prompt_prefix.encode_utf16().count();
        let message = if include_supplementary_character {
            assert!(target_length >= prefix_length + 2);
            format!("{}😀", "a".repeat(target_length - prefix_length - 2))
        } else {
            assert!(target_length >= prefix_length);
            "a".repeat(target_length - prefix_length)
        };
        let final_prompt = build_provider_run_prompt(thread, registry, &message, false);
        assert_eq!(final_prompt.encode_utf16().count(), target_length);
        message
    }

    fn assert_antigravity_prompt_too_large_error(error: PedelecError, prompt_length: usize) {
        assert_eq!(error.code, error_codes::PROVIDER_PROMPT_TOO_LARGE);
        assert_eq!(
            error.message,
            "Antigravity prompt exceeds the 20,000 character limit"
        );
        let details = error
            .details
            .expect("prompt length error should include details");
        assert_eq!(details["provider"], json!("antigravity"));
        assert_eq!(details["promptLength"], json!(prompt_length));
        assert_eq!(
            details["maxPromptLength"],
            json!(ANTIGRAVITY_MAX_PROMPT_UTF16_CODE_UNITS)
        );
        assert_eq!(details["lengthUnit"], json!("utf16CodeUnits"));
    }

    fn runtime_with_provider_thread(
        temp: &Path,
        thread_id: &str,
        provider: ProviderCode,
        provider_session_id: Option<String>,
        model: Option<String>,
    ) -> CoreRuntime {
        let mut runtime = CoreRuntime {
            workspace_manager: WorkspaceManager::with_workspace_root(temp.join("workspace")),
            settings_file_path: Some(temp.join("settings.json")),
            ..CoreRuntime::default()
        };
        write_settings_file(
            &temp.join("settings.json"),
            &PedelecSettings {
                provider_settings: ProviderSettings {
                    ollama: OllamaProviderSettings {
                        api_key: "ollama_test_key".into(),
                        ..OllamaProviderSettings::default()
                    },
                    ..ProviderSettings::default()
                },
                ..PedelecSettings::default()
            },
        )
        .unwrap();
        runtime.set_core_ipc_runtime("127.0.0.1:12345", temp.join("runtime.json"));
        let workspace_path = temp.join("workspace").join(thread_id);
        fs::create_dir_all(workspace_logs_root(&workspace_path)).unwrap();
        let now = chrono::Utc::now();
        let has_user_message = provider_session_id.is_some();
        let effort_args = model
            .map(|model| {
                let flag = match &provider {
                    ProviderCode::Codex => "-m",
                    _ => "--model",
                };
                vec![flag.into(), model]
            })
            .unwrap_or_default();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: provider.clone(),
                effort_level: EffortLevel::Default,
                effort_args,
                workspace_path,
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderAdapterState {
                provider_session_id,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message,
            },
        );
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_skills_input(Some(&sample_skills_input())).unwrap(),
        );
        if provider != ProviderCode::Ollama {
            let privileged_bootstrap = match provider {
                ProviderCode::Codex => ProviderBootstrapMode::CodexDeveloperInstructions,
                ProviderCode::Claude => ProviderBootstrapMode::ClaudeAppendSystemPrompt,
                ProviderCode::OpenCode => ProviderBootstrapMode::OpenCodeInlineAgent,
                ProviderCode::Antigravity => ProviderBootstrapMode::AntigravityWorkspaceAgent,
                ProviderCode::Cursor => ProviderBootstrapMode::UserPromptFallback,
                ProviderCode::Ollama => unreachable!(),
            };
            runtime.provider_scan.insert(
                provider.clone(),
                ProviderCli {
                    path: None,
                    version: Some(ProviderVersion(vec![9, 9, 9])),
                    error: None,
                    bootstrap_capabilities: Some(ProviderBootstrapCapabilities {
                        privileged_bootstrap,
                    }),
                    app_server_capability: None,
                    acp_capability: None,
                    stream_json_capability: None,
                },
            );
        }
        runtime
    }

    fn env_value<'a>(command: &'a CommandSpec, key: &str) -> Option<&'a str> {
        command
            .env
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    fn assert_env(command: &CommandSpec, key: &str, expected: &str) {
        assert_eq!(env_value(command, key), Some(expected), "env {key}");
    }

    fn assert_opencode_native_skills_policy(command: &CommandSpec) {
        let existing = env::var(OPENCODE_PERMISSION_ENV).ok();
        let expected = build_opencode_permission_overlay(existing.as_deref());
        assert_eq!(
            env_value(command, OPENCODE_PERMISSION_ENV),
            expected.as_deref(),
            "OpenCode permission overlay"
        );
    }

    fn assert_provider_instruction_present(command: &CommandSpec) {
        for value in [&command.prompt] {
            assert!(value.contains("[Pedelec Host Context]"));
            assert!(value.contains("[Pedelec App Tool Configuration]"));
            assert!(value.contains("pedelec-cli --thread-id "));
            assert!(value.contains(" tool-spec get_app_state"));
            assert!(value.contains(" tool-call get_app_state '<json_args>'"));
            assert!(!value.contains("[Pedelec Runtime Rules]"));
            assert!(!value
                .contains("All of the following content is executed under the Pedelec Runtime"));
            assert!(!value.contains("[Hard Rules]"));
            assert!(!value.contains("tools.md"));
        }
    }

    fn assert_provider_instruction_absent(command: &CommandSpec) {
        for value in [&command.prompt] {
            assert!(!value.contains("[Pedelec Runtime Rules]"));
            assert!(!value.contains("[Pedelec App Tool Configuration]"));
            assert!(!value.contains("./skills/pedelec-cli.md"));
            assert!(!value.contains("pedelec-cli.md"));
        }
    }

    #[test]
    fn legacy_send_intent_keeps_the_existing_command_spec() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_legacy_intent";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.test_provider_command = Some(CommandSpec {
            program: "codex".into(),
            args: vec!["exec".into()],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            prompt: "prompt".into(),
            stdin: "stdin".into(),
        });

        let start = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "legacy".into(),
            })
            .unwrap();
        assert!(matches!(
            start.intent,
            ProviderExecutionIntent::LegacyCommand {
                command: CommandSpec { .. },
                purpose: RunningProviderProcessPurpose::UserMessage,
            }
        ));
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Running)
        );
    }

    #[test]
    fn persistent_send_returns_a_semantic_intent_without_a_command_spec() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_intent";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);

        let start = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "hello persistent".into(),
            })
            .unwrap();
        match start.intent {
            ProviderExecutionIntent::PersistentRuntime {
                operation: PersistentRuntimeOperation::StartTurn { turn },
            } => {
                assert_eq!(turn.thread_id, thread_id);
                assert_eq!(turn.message, "hello persistent");
                assert!(turn.local_turn_id.starts_with("local_"));
            }
            other => panic!("expected persistent turn intent, got {other:?}"),
        }
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Running)
        );
        assert_eq!(runtime.active_process_id(thread_id), None);
    }

    #[test]
    fn persistent_codex_prepare_maps_typed_session_config_and_native_context() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_prepare_config";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .thread_manager
            .thread_mut(thread_id)
            .unwrap()
            .effort_args = vec![
            "-m".into(),
            "gpt-test".into(),
            "-c".into(),
            "model_reasoning_effort=\"xhigh\"".into(),
        ];

        let start = runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let ProviderExecutionIntent::PersistentRuntime {
            operation: PersistentRuntimeOperation::EnsureSession { session },
        } = start.intent.unwrap()
        else {
            panic!("expected a persistent Codex ensure-session intent");
        };

        assert_eq!(session.model.as_deref(), Some("gpt-test"));
        assert_eq!(session.reasoning_effort, Some(CodexReasoningEffort::XHigh));
        assert_eq!(session.approval_policy, PersistentApprovalPolicy::Never);
        assert_eq!(session.sandbox_policy, PersistentSandboxPolicy::ReadOnly);
        assert_eq!(
            session.config.get("skills.include_instructions"),
            Some(&json!(false))
        );
        assert!(session
            .host_instructions
            .contains("pedelec-cli --thread-id thread_persistent_prepare_config tool-spec"));
        assert!(session
            .host_instructions
            .contains("pedelec-cli --thread-id thread_persistent_prepare_config tool-call"));
        assert!(!session.host_instructions.contains("[Session Preparation]"));
        assert!(!session.host_instructions.contains("PEDELEC_PREPARED"));
    }

    #[test]
    fn persistent_opencode_prepare_send_and_end_are_semantic_operations() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_opencode";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::OpenCode,
            None,
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::OpenCode);
        runtime
            .thread_manager
            .thread_mut(thread_id)
            .unwrap()
            .effort_args = vec!["--model".into(), "openai/gpt-5".into()];

        let prepare = runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let Some(ProviderExecutionIntent::PersistentRuntime {
            operation: PersistentRuntimeOperation::EnsureSession { session },
        }) = prepare.intent
        else {
            panic!("expected OpenCode EnsureSession");
        };
        assert_eq!(session.provider, ProviderCode::OpenCode);
        assert_eq!(session.model.as_deref(), Some("openai/gpt-5"));
        assert!(session.config.is_empty());
        assert!(!session.host_instructions.contains("PEDELEC_PREPARED"));
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                thread_id: thread_id.into(),
                provider_session_id: "open-session".into(),
            })
            .unwrap();

        let send = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "first user task".into(),
            })
            .unwrap();
        let ProviderExecutionIntent::PersistentRuntime {
            operation: PersistentRuntimeOperation::StartTurn { turn },
        } = send.intent
        else {
            panic!("expected OpenCode StartTurn");
        };
        assert_eq!(turn.message, "first user task");
        assert!(!turn.message.contains("Pedelec Host"));

        let end = runtime
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert!(matches!(
            end.execution,
            EndThreadExecutionIntent::PersistentRuntime(
                PersistentRuntimeOperation::EndSession { .. }
            )
        ));
    }

    #[test]
    fn persistent_cursor_prepare_send_and_end_are_semantic_operations() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_cursor";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Cursor,
            None,
            Some("cursor-model".into()),
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Cursor);

        let prepare = runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let Some(ProviderExecutionIntent::PersistentRuntime {
            operation: PersistentRuntimeOperation::EnsureSession { session },
        }) = prepare.intent
        else {
            panic!("expected Cursor EnsureSession");
        };
        assert_eq!(session.provider, ProviderCode::Cursor);
        assert_eq!(session.model.as_deref(), Some("cursor-model"));
        assert!(!session.host_instructions.contains("PEDELEC_PREPARED"));

        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::SessionReady {
                thread_id: thread_id.into(),
                provider_session_id: "cursor-session".into(),
            })
            .unwrap();
        let send = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "first Cursor task".into(),
            })
            .unwrap();
        let ProviderExecutionIntent::PersistentRuntime {
            operation: PersistentRuntimeOperation::StartTurn { turn },
        } = send.intent
        else {
            panic!("expected Cursor StartTurn");
        };
        assert_eq!(turn.message, "first Cursor task");
        assert_eq!(turn.provider_session_id, Some("cursor-session".into()));

        let end = runtime
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert!(matches!(
            end.execution,
            EndThreadExecutionIntent::PersistentRuntime(
                PersistentRuntimeOperation::EndSession { .. }
            )
        ));
    }

    #[test]
    fn normalized_persistent_completion_emits_done_and_returns_to_idle() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_complete";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        let events = runtime.event_bus.subscribe(thread_id);
        runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "hello".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnStarted {
                thread_id: thread_id.into(),
                provider_turn_id: "provider-turn-1".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::AssistantDelta {
                thread_id: thread_id.into(),
                provider_turn_id: Some("provider-turn-1".into()),
                text: "ans".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::AssistantMessage {
                thread_id: thread_id.into(),
                provider_turn_id: Some("provider-turn-1".into()),
                text: "answer".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                thread_id: thread_id.into(),
                provider_turn_id: Some("provider-turn-1".into()),
                success: true,
                error: None,
            })
            .unwrap();

        let emitted = collect_available_core_events(&events);
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
        assert_eq!(
            runtime
                .provider_state(thread_id)
                .unwrap()
                .active_provider_turn_id,
            None
        );
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantDelta { text, .. } if text == "ans"
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::AssistantMessage { text, .. } if text == "answer"
        )));
        assert!(emitted
            .iter()
            .any(|event| matches!(event, ThreadEvent::Done { .. })));
    }

    #[test]
    fn normalized_persistent_failure_moves_user_turn_to_error() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_failure";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        let events = runtime.event_bus.subscribe(thread_id);
        runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "fail".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                thread_id: thread_id.into(),
                provider_turn_id: None,
                success: false,
                error: Some(PedelecError::new(
                    error_codes::PROVIDER_REQUEST_FAILED,
                    "provider rejected turn",
                )),
            })
            .unwrap();

        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Error));
        assert_eq!(
            runtime
                .provider_state(thread_id)
                .unwrap()
                .active_provider_turn_id,
            None
        );
        assert!(collect_available_core_events(&events).iter().any(|event| matches!(
            event,
            ThreadEvent::Error { error, .. } if error.code == error_codes::PROVIDER_REQUEST_FAILED
        )));
    }

    #[test]
    fn completion_while_stopping_does_not_return_to_idle() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_stopping";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "stop me".into(),
            })
            .unwrap();
        let turn_id = runtime
            .provider_state(thread_id)
            .unwrap()
            .active_provider_turn_id
            .clone();
        let end = runtime
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert!(matches!(
            end.execution,
            EndThreadExecutionIntent::PersistentRuntime(_)
        ));
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                thread_id: thread_id.into(),
                provider_turn_id: turn_id,
                success: true,
                error: None,
            })
            .unwrap();
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Stopping)
        );
        runtime.finish_end_thread(thread_id).unwrap();
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Ended));
    }

    #[test]
    fn persistent_prepare_failure_returns_to_idle_without_error_state() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_prepare";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        let events = runtime.event_bus.subscribe(thread_id);
        let start = runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert!(matches!(
            start.intent,
            Some(ProviderExecutionIntent::PersistentRuntime {
                operation: PersistentRuntimeOperation::EnsureSession { .. }
            })
        ));
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::ProviderError {
                thread_id: thread_id.into(),
                provider_turn_id: None,
                error: PedelecError::new(
                    error_codes::PROVIDER_REQUEST_FAILED,
                    "session creation failed",
                ),
            })
            .unwrap();
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
        assert!(collect_available_core_events(&events)
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error { .. })));
    }

    #[test]
    fn multiple_persistent_threads_can_run_but_each_thread_is_single_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_persistent_a",
            ProviderCode::Codex,
            None,
            None,
        );
        let second = "thread_persistent_b";
        let second_path = temp.path().join("workspace").join(second);
        fs::create_dir_all(workspace_logs_root(&second_path)).unwrap();
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: second.into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: Vec::new(),
                workspace_path: second_path,
                skills: Vec::new(),
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: None,
            },
            ProviderSessionState {
                provider_session_id: None,
                active_provider_turn_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime
            .tool_registry
            .insert(second, ToolRegistry::default());
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        for thread_id in ["thread_persistent_a", second] {
            runtime
                .begin_send_text_intent(SendTextInput {
                    thread_id: thread_id.into(),
                    message: "run".into(),
                })
                .unwrap();
        }
        assert_eq!(
            runtime.thread_status("thread_persistent_a"),
            Some(ThreadStatus::Running)
        );
        assert_eq!(runtime.thread_status(second), Some(ThreadStatus::Running));
        let busy = runtime.begin_send_text_intent(SendTextInput {
            thread_id: "thread_persistent_a".into(),
            message: "duplicate".into(),
        });
        assert_eq!(busy.unwrap_err().code, error_codes::THREAD_BUSY);
    }

    #[test]
    fn ended_debug_send_reactivates_resources_and_returns_persistent_intent() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_debug";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Codex,
            Some("provider-session".into()),
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert!(runtime.tool_registry.get(thread_id).is_none());
        let start = runtime
            .begin_debug_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "diagnose".into(),
            })
            .unwrap();
        assert!(matches!(
            start.intent,
            ProviderExecutionIntent::PersistentRuntime {
                operation: PersistentRuntimeOperation::StartTurn { .. }
            }
        ));
        assert!(runtime.tool_registry.get(thread_id).is_some());
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Running)
        );
    }

    #[test]
    fn normal_persistent_send_rejects_ended_thread() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_normal_ended";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let error = runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "must reject".into(),
            })
            .unwrap_err();
        assert_eq!(error.code, error_codes::THREAD_ENDED);
    }

    #[test]
    fn persistent_runtime_disconnect_fanout_preserves_idle_and_fails_active_threads() {
        let temp = tempfile::tempdir().unwrap();
        let idle_id = "thread_runtime_idle";
        let running_id = "thread_runtime_running";
        let waiting_id = "thread_runtime_waiting";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            idle_id,
            ProviderCode::Codex,
            Some("provider-idle".into()),
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);

        let base_thread = runtime.thread_manager.thread(idle_id).unwrap().clone();
        let base_provider_state = runtime
            .thread_manager
            .provider_state(idle_id)
            .unwrap()
            .clone();
        for (thread_id, status, provider_session_id) in [
            (running_id, ThreadStatus::Running, "provider-running"),
            (
                waiting_id,
                ThreadStatus::WaitingToolResult,
                "provider-waiting",
            ),
        ] {
            let mut thread = base_thread.clone();
            thread.thread_id = thread_id.into();
            thread.workspace_path = temp.path().join("workspace").join(thread_id);
            thread.status = status;
            fs::create_dir_all(workspace_logs_root(&thread.workspace_path)).unwrap();
            let mut provider_state = base_provider_state.clone();
            provider_state.provider_session_id = Some(provider_session_id.into());
            provider_state.active_provider_turn_id = Some(format!("turn-{thread_id}"));
            provider_state.has_user_message = true;
            runtime.thread_manager.insert_thread(thread, provider_state);
        }
        runtime
            .pending_provider_operations
            .insert(running_id.into(), PendingProviderOperation::UserTurn);
        runtime
            .pending_provider_operations
            .insert(waiting_id.into(), PendingProviderOperation::UserTurn);
        let waiting_tool_wait = match runtime
            .tool_request_broker
            .begin_or_join(
                waiting_id.into(),
                "shell".into(),
                json!({"command":"pwd"}),
                1000,
            )
            .unwrap()
        {
            ToolInvocationRegistration::Created(wait) => wait,
            ToolInvocationRegistration::Joined(_) => panic!("test request unexpectedly joined"),
            ToolInvocationRegistration::Replayed(_) => {
                panic!("test request unexpectedly replayed")
            }
        };
        let events = runtime.subscribe_all_threads();

        runtime.fail_persistent_runtime(
            ProviderCode::Codex,
            PedelecError::new(
                error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                "runtime disconnected in test",
            ),
        );

        assert_eq!(runtime.thread_status(idle_id), Some(ThreadStatus::Idle));
        assert_eq!(
            runtime
                .provider_state(idle_id)
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("provider-idle")
        );
        assert_eq!(
            runtime
                .provider_state(idle_id)
                .unwrap()
                .active_provider_turn_id,
            None
        );
        for thread_id in [running_id, waiting_id] {
            assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Error));
            assert_eq!(
                runtime
                    .provider_state(thread_id)
                    .unwrap()
                    .active_provider_turn_id,
                None
            );
        }
        assert!(!runtime.pending_provider_operations.contains_key(running_id));
        assert!(!runtime.pending_provider_operations.contains_key(waiting_id));
        assert!(!runtime
            .tool_request_broker
            .has_pending_for_thread(waiting_id));
        assert!(matches!(
            waiting_tool_wait.result_rx.recv().unwrap(),
            ToolInvocationOutcome::CoreError(error)
                if error.code == error_codes::PROVIDER_RUNTIME_DISCONNECTED
        ));
        let emitted = collect_available_core_events(&events);
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { thread_id, error, .. }
                if thread_id == running_id
                    && error.code == error_codes::PROVIDER_RUNTIME_DISCONNECTED
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { thread_id, error, .. }
                if thread_id == waiting_id
                    && error.code == error_codes::PROVIDER_RUNTIME_DISCONNECTED
        )));
        assert!(!emitted.iter().any(|event| matches!(
            event,
            ThreadEvent::Error { thread_id, .. } if thread_id == idle_id
        )));
    }

    #[test]
    fn runtime_disconnect_during_prepare_returns_to_idle() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_runtime_prepare";
        let mut runtime =
            runtime_with_provider_thread(temp.path(), thread_id, ProviderCode::Codex, None, None);
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .begin_prepare_thread_intent(PrepareThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        let error = PedelecError::new(
            error_codes::PROVIDER_RUNTIME_DISCONNECTED,
            "runtime disconnected during prepare",
        );

        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::RuntimeDisconnected {
                thread_id: Some(thread_id.into()),
                error,
            })
            .unwrap();

        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Idle));
        assert!(!runtime.pending_provider_operations.contains_key(thread_id));
    }

    #[test]
    fn interrupted_completion_while_stopping_does_not_return_to_idle() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_runtime_stopping";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Codex,
            Some("provider-session".into()),
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .begin_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "active".into(),
            })
            .unwrap();
        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnStarted {
                thread_id: thread_id.into(),
                provider_turn_id: "provider-turn".into(),
            })
            .unwrap();
        let end = runtime
            .begin_end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Stopping)
        );

        runtime
            .reduce_provider_runtime_event(ProviderRuntimeEvent::TurnCompleted {
                thread_id: thread_id.into(),
                provider_turn_id: Some("provider-turn".into()),
                success: false,
                error: Some(PedelecError::new(
                    error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                    "interrupted in test",
                )),
            })
            .unwrap();
        assert_eq!(
            runtime.thread_status(thread_id),
            Some(ThreadStatus::Stopping)
        );
        assert_eq!(
            runtime
                .provider_state(thread_id)
                .unwrap()
                .active_provider_turn_id,
            None
        );

        runtime.finish_end_thread(&end.thread_id).unwrap();
        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Ended));
    }

    #[test]
    fn failed_debug_dispatch_restores_ended_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let thread_id = "thread_persistent_debug_rollback";
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            thread_id,
            ProviderCode::Codex,
            Some("provider-session".into()),
            None,
        );
        runtime.use_persistent_provider_for_test(ProviderCode::Codex);
        runtime
            .end_thread(EndThreadInput {
                thread_id: thread_id.into(),
            })
            .unwrap();
        runtime
            .begin_debug_send_text_intent(SendTextInput {
                thread_id: thread_id.into(),
                message: "diagnose".into(),
            })
            .unwrap();
        assert!(runtime.tool_registry.get(thread_id).is_some());

        runtime.fail_provider_execution_dispatch(
            thread_id,
            ProviderExecutionOperationKind::UserTurn,
            PedelecError::new(
                error_codes::PROVIDER_RUNTIME_DISCONNECTED,
                "debug runtime unavailable",
            ),
        );

        assert_eq!(runtime.thread_status(thread_id), Some(ThreadStatus::Ended));
        assert!(runtime.tool_registry.get(thread_id).is_none());
        assert!(!runtime.debug_reactivating_threads.contains(thread_id));
        assert_eq!(runtime.event_log_path(thread_id), None);
        assert_eq!(
            runtime
                .provider_state(thread_id)
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("provider-session")
        );
    }

    fn collect_available_core_events(event_rx: &mpsc::Receiver<ThreadEvent>) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
            events.push(event);
        }
        events
    }
}
