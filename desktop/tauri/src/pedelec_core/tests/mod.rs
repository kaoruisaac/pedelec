use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn provider_info_exposes_scanned_version_without_exposing_ollama_version() {
        let scan = HashMap::from([(
            ProviderCode::Codex,
            ProviderCli {
                path: Some(PathBuf::from("C:/providers/codex.cmd")),
                version: Some(ProviderVersion(vec![1, 2, 3])),
                error: None,
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

    #[cfg(windows)]
    #[test]
    fn provider_version_command_runs_script_wrappers_through_headless_cmd() {
        let command = provider_version_command(Path::new("C:/providers/codex.cmd"));

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

    #[test]
    fn codex_new_command_uses_sandbox_args_env_prompt_and_no_generated_session_id() {
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
        let sandbox_path = temp.path().join("sandbox").join("thread_codex_new");
        assert_eq!(
            start.command.args,
            vec![
                "exec",
                "--cd",
                sandbox_path.to_str().unwrap(),
                "--sandbox",
                "danger-full-access",
                "--skip-git-repo-check",
                "--json",
                "-m",
                "gpt-5",
                "-"
            ]
        );
        assert_eq!(start.command.cwd, sandbox_path);
        assert!(!start.command.args.iter().any(|arg| arg == "--last"));
        assert_provider_instruction_present(&start.command);
        assert!(start.command.stdin.ends_with("hello"));
        assert_env(&start.command, "PEDELEC_THREAD_ID", "thread_codex_new");
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

        assert_eq!(
            start.command.args,
            vec![
                "exec",
                "--cd",
                temp.path()
                    .join("sandbox")
                    .join("thread_codex_resume")
                    .to_str()
                    .unwrap(),
                "--sandbox",
                "danger-full-access",
                "--skip-git-repo-check",
                "--json",
                "resume",
                "123e4567-e89b-12d3-a456-426614174000",
                "-"
            ]
        );
        assert!(!start.command.args.iter().any(|arg| arg == "--last"));
        assert_eq!(start.command.prompt, "continue");
        assert_eq!(start.command.stdin, "continue");
        assert_provider_instruction_absent(&start.command);
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

        let sandbox_path = temp.path().join("sandbox").join("thread_opencode_new");
        assert_eq!(start.command.program, "opencode");
        assert_eq!(
            start.command.args,
            vec![
                "run",
                "--dangerously-skip-permissions",
                "--thinking",
                "--pure",
                "--format",
                "json",
                "--dir",
                sandbox_path.to_str().unwrap(),
                "--model",
                "ollama/qwen2.5-coder:14b",
                "-"
            ]
        );
        assert_eq!(start.command.cwd, sandbox_path);
        assert!(start.command.stdin.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(!start
            .command
            .args
            .iter()
            .any(|arg| arg == &start.command.stdin));
        assert_env(&start.command, "PEDELEC_PROVIDER", "opencode");
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
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::Error { error, .. } if error.code == error_codes::PROVIDER_COMMAND_FAILED)
        ));
    }

    #[test]
    fn cursor_new_command_uses_agent_workspace_model_json_and_stdin_prompt() {
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

        let sandbox_path = temp.path().join("sandbox").join("thread_cursor_new");
        assert_eq!(start.command.program, "agent");
        assert_eq!(
            start.command.args,
            vec![
                "--workspace",
                sandbox_path.to_str().unwrap(),
                "--output-format",
                "stream-json",
                "--force",
                "--trust",
                "--model",
                "gpt-5",
            ]
        );
        assert_eq!(start.command.cwd, sandbox_path);
        assert!(start.command.stdin.ends_with(message));
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
        assert_provider_instruction_absent(&start.command);
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
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::Error { error, .. } if error.code == error_codes::PROVIDER_COMMAND_FAILED)
        ));
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

        let sandbox_path = temp.path().join("sandbox").join("thread_claude_new");
        assert_eq!(start.command.program, "claude");
        assert_eq!(
            start.command.args,
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--model",
                "sonnet",
            ]
        );
        assert_eq!(start.command.cwd, sandbox_path);
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

        assert_eq!(
            start.command.args,
            vec![
                "-p",
                "--resume",
                "4fab02ca-67b9-489d-8b89-0b1f0b9550e6",
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--model",
                "sonnet",
            ]
        );
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
            |event| matches!(event, ThreadEvent::Error { error, .. } if error.code == error_codes::PROVIDER_COMMAND_FAILED)
        ));
    }

    #[test]
    fn ollama_new_command_uses_pedelec_agent_model_sandbox_and_stdin_prompt() {
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

        let sandbox_path = temp.path().join("sandbox").join("thread_ollama_new");
        assert_eq!(start.command.program, "pedelec-agent");
        assert_eq!(
            start.command.args,
            vec![
                "--provider",
                "ollama",
                "--model",
                "qwen3-14b-32k:latest",
                "--sandbox",
                sandbox_path.to_str().unwrap(),
            ]
        );
        assert_eq!(start.command.cwd, sandbox_path);
        assert!(start.command.stdin.ends_with(message));
        assert_provider_instruction_present(&start.command);
        assert!(!start.command.args.iter().any(|arg| arg == message));
        assert!(!start.command.args.iter().any(|arg| arg == "--session-id"));
        assert_env(&start.command, "PEDELEC_THREAD_ID", "thread_ollama_new");
        assert_env(&start.command, "PEDELEC_PROVIDER", "ollama");
        assert_env(&start.command, "PEDELEC_MODEL", "qwen3-14b-32k:latest");
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

    #[test]
    fn ollama_resume_uses_provider_session_id_and_outer_thread_env() {
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
                    .join("sandbox")
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
        assert_env(&start.command, "PEDELEC_THREAD_ID", "thread_outer");
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
                ThreadEvent::Error { error, .. }
                    if error.code == "OLLAMA_UNAVAILABLE"
                        && error.message == "Ollama request failed"
                        && error.details.as_ref().and_then(|details| details.get("status")) == Some(&json!(500))
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ThreadEvent::Error { error, .. }
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
        let settings = PedelecSettings {
            default_provider: Some(ProviderCode::Ollama),
            default_models: HashMap::from([(ProviderCode::Ollama, "qwen3:8b".into())]),
            provider_settings: ProviderSettings {
                ollama: OllamaProviderSettings {
                    base_url: "http://127.0.0.1:11434".into(),
                    timeout_ms: 120_000,
                    api_key: "ollama_xxx".into(),
                },
            },
        };

        let value = serde_json::to_value(&settings).unwrap();

        assert_eq!(
            value,
            json!({
                "defaultProvider": "ollama",
                "defaultModels": {
                    "ollama": "qwen3:8b"
                },
                "providerSettings": {
                    "ollama": {
                        "baseUrl": "http://127.0.0.1:11434",
                        "timeoutMs": 120000,
                        "apiKey": "ollama_xxx"
                    }
                }
            })
        );
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
    }

    #[test]
    fn update_settings_persists_provider_and_default_models() {
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
                default_models: HashMap::from([
                    (ProviderCode::Codex, "gpt-5".into()),
                    (ProviderCode::Ollama, "qwen3-14b-32k:latest".into()),
                ]),
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap();

        assert_eq!(
            saved,
            PedelecSettings {
                default_provider: Some(ProviderCode::Ollama),
                default_models: HashMap::from([
                    (ProviderCode::Codex, "gpt-5".into()),
                    (ProviderCode::Ollama, "qwen3-14b-32k:latest".into()),
                ]),
                provider_settings: ProviderSettings {
                    ollama: OllamaProviderSettings {
                        api_key: "ollama".into(),
                        ..OllamaProviderSettings::default()
                    },
                },
            }
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
                default_models: HashMap::from([(ProviderCode::Ollama, "qwen3:8b".into())]),
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(" https://ollama.example.test/ ".into()),
                        timeout_ms: Some(250_000),
                        api_key: Some(" ollama_cloud_key ".into()),
                    },
                },
            })
            .unwrap();

        assert_eq!(
            saved.provider_settings.ollama.base_url,
            "https://ollama.example.test"
        );
        assert_eq!(saved.provider_settings.ollama.timeout_ms, 250_000);
        assert_eq!(saved.provider_settings.ollama.api_key, "ollama_cloud_key");
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
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some("   ".into()),
                        timeout_ms: None,
                        api_key: Some("ollama".into()),
                    },
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

        let invalid_url = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some("ftp://127.0.0.1:11434".into()),
                        timeout_ms: Some(120_000),
                        api_key: Some("ollama".into()),
                    },
                },
            })
            .unwrap_err();
        assert_eq!(invalid_url.code, error_codes::OLLAMA_BASE_URL_INVALID);

        let invalid_timeout = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                        timeout_ms: Some(0),
                        api_key: Some("ollama".into()),
                    },
                },
            })
            .unwrap_err();
        assert_eq!(invalid_timeout.code, error_codes::OLLAMA_REQUEST_FAILED);

        let missing_api_key = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput {
                    ollama: OllamaProviderSettingsInput {
                        base_url: Some(DEFAULT_OLLAMA_BASE_URL.into()),
                        timeout_ms: Some(120_000),
                        api_key: Some("   ".into()),
                    },
                },
            })
            .unwrap_err();
        assert_eq!(missing_api_key.code, error_codes::OLLAMA_API_KEY_REQUIRED);
    }

    #[test]
    fn update_settings_trims_and_removes_empty_default_models() {
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
                default_models: HashMap::from([
                    (ProviderCode::Codex, "  gpt-5  ".into()),
                    (ProviderCode::Antigravity, "   ".into()),
                ]),
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap();

        assert_eq!(
            saved.default_models,
            HashMap::from([(ProviderCode::Codex, "gpt-5".into())])
        );
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
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap_err();

        assert_eq!(err.code, error_codes::DEFAULT_PROVIDER_UNAVAILABLE);
        assert!(!temp.path().join("settings.json").exists());
    }

    #[test]
    fn update_settings_allows_unavailable_ollama_provider() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = CoreRuntime {
            settings_file_path: Some(temp.path().join("settings.json")),
            provider_path_value_override: Some(OsString::from("")),
            ..CoreRuntime::default()
        };

        let saved = runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                default_models: HashMap::new(),
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Ollama));
        assert!(temp.path().join("settings.json").exists());
    }

    #[test]
    fn update_settings_allows_unavailable_non_default_provider_models() {
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
                default_models: HashMap::from([
                    (ProviderCode::Codex, "gpt-5".into()),
                    (ProviderCode::Antigravity, "antigravity-2.5-pro".into()),
                ]),
                provider_settings: ProviderSettingsInput::default(),
            })
            .unwrap();

        assert_eq!(saved.default_provider, Some(ProviderCode::Codex));
        assert_eq!(
            saved.default_models.get(&ProviderCode::Antigravity),
            Some(&"antigravity-2.5-pro".to_string())
        );
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
        let candidates = provider_binary_lookup_candidates("agent", &dirs);

        #[cfg(windows)]
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("C:/bin/agent"),
                PathBuf::from("C:/bin/agent.exe"),
                PathBuf::from("C:/bin/agent.cmd"),
                PathBuf::from("C:/bin/agent.bat"),
            ]
        );

        #[cfg(not(windows))]
        assert_eq!(candidates, vec![PathBuf::from("C:/bin").join("agent")]);
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
    fn antigravity_assistant_message_parser_accepts_only_agent_response_deltas() {
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
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "hello")
        ));
        assert!(events.iter().all(|event| {
            !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "ignore user" || text == "ignore missing role")
        }));
    }

    #[test]
    fn antigravity_assistant_message_parser_preserves_delta_whitespace() {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_antigravity_nested_role",
            ProviderCode::Antigravity,
            None,
            None,
        );
        let event_rx = runtime
            .event_bus
            .subscribe("thread_antigravity_nested_role");

        runtime.emit_provider_stdout(
            "thread_antigravity_nested_role",
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"  nested hello\n"}}"#
                .to_string() + "\n",
        );
        runtime.emit_provider_stdout(
            "thread_antigravity_nested_role",
            r#"{"event":"step_update","step_update":{"step_type":"agent_response"}}"#.to_string()
                + "\n",
        );

        let events = collect_available_core_events(&event_rx);
        assert!(events.iter().any(
            |event| matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "  nested hello\n")
        ));
        assert!(events.iter().all(|event| {
            !matches!(event, ThreadEvent::AssistantMessage { text, .. } if text == "sibling text")
        }));
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

        assert_eq!(
            start.command.args,
            vec![
                "exec",
                "--cd",
                temp.path()
                    .join("sandbox")
                    .join("thread_codex_started")
                    .to_str()
                    .unwrap(),
                "--sandbox",
                "danger-full-access",
                "--skip-git-repo-check",
                "--json",
                "resume",
                "019e91d7-4a21-7ca0-aeef-c27ce6e334c5",
                "-"
            ]
        );
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
            model: Some("gpt-5".into()),
            sandbox_path: PathBuf::from("C:/tmp/pedelec/thread_abc123"),
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
        };

        let value = serde_json::to_value(state).unwrap();
        assert!(value.get("threadId").is_some());
        assert!(value.get("sandboxPath").is_some());
        assert!(value.get("createdAt").is_some());
        assert!(value.get("thread_id").is_none());
        assert!(value.get("sandbox_path").is_none());
        assert!(value.get("created_at").is_none());
    }

    #[test]
    fn provider_instruction_omits_app_configuration_without_skills() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_no_tools_md".into(),
            provider: ProviderCode::Codex,
            model: None,
            sandbox_path: PathBuf::from("sandbox").join("thread_no_tools_md"),
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
        };

        let instruction = build_provider_instruction(&thread, &ToolRegistry::default());

        assert!(!instruction.contains("tools.md"));
        assert!(!instruction.contains("pedelec-cli tool-call"));
        assert_eq!(instruction, "");
    }

    #[test]
    fn provider_instruction_serializes_app_configuration() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_with_tools_md".into(),
            provider: ProviderCode::Codex,
            model: None,
            sandbox_path: PathBuf::from("sandbox").join("thread_with_tools_md"),
            skills: vec![],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
        };

        let registry = ToolRegistry::from_skills_input(Some(&sample_skills_input())).unwrap();
        let instruction = build_provider_instruction(&thread, &registry);

        assert!(instruction.contains("[Pedelec Runtime Rules]"));
        assert!(instruction.contains("[Pedelec App Tool Configuration]"));
        assert!(instruction.contains("pedelec-cli tool-spec get_app_state"));
        assert!(instruction.contains("pedelec-cli tool-call get_app_state '<json_args>'"));
        assert!(!instruction.contains("tools.md"));
        assert!(!instruction.contains("argsSchema"));
    }

    #[test]
    fn provider_instruction_preserves_empty_tools_guidance_and_escapes_structure() {
        let now = chrono::Utc::now();
        let thread = ThreadState {
            thread_id: "thread_empty_tools".into(),
            provider: ProviderCode::Codex,
            model: None,
            sandbox_path: PathBuf::from("sandbox").join("thread_empty_tools"),
            skills: vec![],
            status: ThreadStatus::Idle,
            process_id: None,
            created_at: now,
            updated_at: now,
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
    fn sandbox_creates_required_subdirectories_and_removes_them() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SandboxManager::with_sandbox_root(temp.path().join("sandbox"));

        let sandbox = manager.create_thread_sandbox("thread_abc123").unwrap();

        assert!(sandbox.exists());
        for subdir in SANDBOX_SUBDIRS {
            assert!(sandbox.join(subdir).is_dir(), "missing subdir {subdir}");
        }

        manager.remove_thread_sandbox(&sandbox).unwrap();
        assert!(!sandbox.exists());
    }

    #[test]
    fn sandbox_rollback_removes_partial_sandbox_after_skill_download_failure() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SandboxManager::with_sandbox_root(temp.path().join("sandbox"));
        let skill_manager = SkillManager::default();
        let bad_urls = vec!["http://example.com/tools.md".to_string()];

        let result = manager.create_thread_sandbox_with("thread_rollback", |sandbox| {
            skill_manager.download_skills(sandbox.join("skills"), &bad_urls)
        });

        assert_eq!(result.unwrap_err().code, error_codes::SKILL_URL_INVALID);
        assert!(!temp.path().join("sandbox").join("thread_rollback").exists());
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

        let (request_id, timeout_ms, _result_rx) = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_normalized".into(),
                tool_name: "get_app_state".into(),
                args: json!({ "timeoutMs": 25 }),
            })
            .unwrap();

        assert_eq!(timeout_ms, 25);
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
        let (request_id, _timeout_ms, result_rx) = runtime
            .begin_tool_call(ToolCallInput {
                thread_id: "thread_submit".into(),
                tool_name: "get_app_state".into(),
                args: json!({}),
            })
            .unwrap();

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
            json!({ "value": 2 })
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
    fn list_assets_reads_only_completed_first_level_files_without_mutating_thread() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime_with_provider_thread(
            temp.path(),
            "thread_assets",
            ProviderCode::Codex,
            None,
            None,
        );
        let input = temp.path().join("sandbox/thread_assets/input");
        fs::create_dir_all(input.join("nested")).unwrap();
        fs::write(input.join("upl-report.txt"), b"data").unwrap();
        fs::write(input.join(".env"), b"key=value").unwrap();
        fs::write(input.join(".pedelec-internal"), b"hidden").unwrap();
        fs::write(input.join("nested/ignored.txt"), b"ignored").unwrap();

        let status_before = runtime.thread_status("thread_assets");
        let output = runtime
            .list_assets(ListAssetsInput {
                thread_id: "thread_assets".into(),
            })
            .unwrap();

        assert_eq!(status_before, runtime.thread_status("thread_assets"));
        assert_eq!(output.assets.len(), 2);
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == "upl-report.txt"
                && asset.path == "input/upl-report.txt"
                && asset.size_bytes == 4
                && asset.modified_at >= 0));
        assert!(output
            .assets
            .iter()
            .any(|asset| asset.name == ".env" && asset.path == "input/.env"));
        assert!(output
            .assets
            .iter()
            .all(|asset| !asset.path.contains(temp.path().to_string_lossy().as_ref())));
    }

    #[test]
    fn create_thread_generates_short_incrementing_thread_ids() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };
        let input = CreateThreadInput {
            provider: ProviderCode::Codex,
            model: None,
            skills: None,
        };

        let first = runtime.create_thread(input.clone()).unwrap();
        let second = runtime.create_thread(input).unwrap();

        assert_eq!(first.thread_id, "t000001");
        assert_eq!(second.thread_id, "t000002");
        assert_short_thread_id(&first.thread_id);
        assert_short_thread_id(&second.thread_id);
        assert!(sandbox_root.join("t000001").exists());
        assert!(sandbox_root.join("t000002").exists());
    }

    #[test]
    fn create_thread_skips_existing_short_thread_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        fs::create_dir_all(sandbox_root.join("t000001")).unwrap();
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: None,
            })
            .unwrap();

        assert_eq!(output.thread_id, "t000002");
        assert_short_thread_id(&output.thread_id);
        assert!(sandbox_root.join("t000002").exists());
    }

    #[test]
    fn create_thread_skips_existing_active_short_thread_id() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: "t000001".into(),
                provider: ProviderCode::Codex,
                model: None,
                sandbox_path: sandbox_root.join("t000001"),
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
            },
            ProviderAdapterState {
                provider_session_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: None,
            })
            .unwrap();

        assert_eq!(output.thread_id, "t000002");
        assert_short_thread_id(&output.thread_id);
        assert!(sandbox_root.join("t000002").exists());
    }

    #[test]
    fn cleanup_for_app_exit_ends_active_threads_and_removes_sandboxes() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };
        let input = CreateThreadInput {
            provider: ProviderCode::Codex,
            model: None,
            skills: None,
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
        assert!(!sandbox_root.join(&first.thread_id).exists());
        assert!(!sandbox_root.join(&second.thread_id).exists());
    }

    #[test]
    fn cleanup_for_app_exit_removes_orphan_sandbox_directories() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        fs::create_dir_all(sandbox_root.join("t000999").join("logs")).unwrap();
        fs::write(sandbox_root.join("keep.txt"), "not a sandbox").unwrap();
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let errors = runtime.cleanup_for_app_exit();

        assert!(errors.is_empty());
        assert!(!sandbox_root.join("t000999").exists());
        assert!(sandbox_root.join("keep.txt").exists());
    }

    #[test]
    fn remove_all_thread_sandboxes_succeeds_when_root_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("missing-sandbox");
        let manager = SandboxManager::with_sandbox_root(&sandbox_root);

        let errors = manager.remove_all_thread_sandboxes();

        assert!(errors.is_empty());
        assert!(!sandbox_root.exists());
    }

    #[test]
    fn next_thread_id_errors_when_short_id_space_is_exhausted() {
        let mut manager = ThreadManager {
            next_thread_number: THREAD_ID_MAX_COUNTER,
            ..ThreadManager::default()
        };

        let err = manager.next_thread_id().unwrap_err();

        assert_eq!(err.code, error_codes::SANDBOX_CREATE_FAILED);
    }

    #[test]
    fn create_thread_generates_per_tool_specs_without_tools_md() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
            })
            .unwrap();

        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        let skills_dir = thread.sandbox_path.join("skills");
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
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
            })
            .unwrap();
        fs::write(
            sandbox_root
                .join(&output.thread_id)
                .join("skills")
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
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
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
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: None,
            })
            .unwrap();

        let thread = runtime.thread_manager.thread(&output.thread_id).unwrap();
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert_eq!(thread.process_id, None);
        assert_eq!(runtime.running_process_count(), 0);
        let skills_dir = thread.sandbox_path.join("skills");
        assert!(skills_dir.exists());
        assert!(skills_dir.is_dir());
        assert!(!skills_dir.join("tools.md").exists());
        assert!(!skills_dir.join("pedelec-cli.md").exists());
        assert!(thread.skills.is_empty());
    }

    #[test]
    fn prepare_thread_builds_prepare_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
            })
            .unwrap();

        let start = runtime
            .begin_prepare_thread(PrepareThreadInput {
                thread_id: output.thread_id.clone(),
            })
            .unwrap();
        let command = start.command.unwrap();

        assert!(command.stdin.contains("[Session Preparation]"));
        assert!(command.stdin.contains("PEDELEC_PREPARED"));
        assert!(command.stdin.contains(
            "Respond to the task in the following [Session Preparation] or [User Message] block."
        ));
        assert!(!command.stdin.contains("\n[User Message]\n"));
        assert_eq!(
            runtime
                .thread_manager
                .thread(&output.thread_id)
                .unwrap()
                .status,
            ThreadStatus::Running
        );
    }

    #[test]
    fn prepare_thread_noops_when_provider_session_id_exists() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
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
        let sandbox_root = temp.path().join("sandbox");
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(&sandbox_root),
            ..CoreRuntime::default()
        };

        let output = runtime
            .create_thread(CreateThreadInput {
                provider: ProviderCode::Codex,
                model: None,
                skills: Some(sample_skills_input()),
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
        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let program_name = format!("{program}.exe");
        #[cfg(not(windows))]
        let program_name = program.to_string();
        fs::write(bin_dir.join(program_name), b"fake provider").unwrap();
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
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider: ProviderCode::Codex,
                model: None,
                sandbox_path: PathBuf::from("sandbox").join(thread_id),
                skills: vec![],
                status,
                process_id: None,
                created_at: now,
                updated_at: now,
            },
            ProviderAdapterState {
                provider_session_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_tools_json_str(tools_json).unwrap(),
        );
        runtime
    }

    fn runtime_with_provider_thread(
        temp: &Path,
        thread_id: &str,
        provider: ProviderCode,
        provider_session_id: Option<String>,
        model: Option<String>,
    ) -> CoreRuntime {
        let mut runtime = CoreRuntime {
            sandbox_manager: SandboxManager::with_sandbox_root(temp.join("sandbox")),
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
                },
                ..PedelecSettings::default()
            },
        )
        .unwrap();
        runtime.set_core_ipc_runtime("127.0.0.1:12345", temp.join("runtime.json"));
        let sandbox_path = temp.join("sandbox").join(thread_id);
        fs::create_dir_all(sandbox_path.join("logs")).unwrap();
        let now = chrono::Utc::now();
        let has_user_message = provider_session_id.is_some();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: thread_id.into(),
                provider,
                model,
                sandbox_path,
                skills: vec![],
                status: ThreadStatus::Idle,
                process_id: None,
                created_at: now,
                updated_at: now,
            },
            ProviderAdapterState {
                provider_session_id,
                last_process_id: None,
                has_user_message,
            },
        );
        runtime.tool_registry.insert(
            thread_id,
            ToolRegistry::from_skills_input(Some(&sample_skills_input())).unwrap(),
        );
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

    fn assert_provider_instruction_present(command: &CommandSpec) {
        for value in [&command.prompt] {
            assert!(value.contains("[Pedelec Runtime Rules]"));
            assert!(value.contains("[Pedelec App Tool Configuration]"));
            assert!(value.contains("pedelec-cli tool-spec get_app_state"));
            assert!(value.contains("pedelec-cli tool-call get_app_state '<json_args>'"));
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

    fn collect_available_core_events(event_rx: &mpsc::Receiver<ThreadEvent>) -> Vec<ThreadEvent> {
        let mut events = Vec::new();
        while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
            events.push(event);
        }
        events
    }
}
