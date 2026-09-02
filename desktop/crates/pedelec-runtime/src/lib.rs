//! Persistent provider-runtime building blocks.
//!
//! The crate deliberately stops at process supervision and protocol
//! transport. Provider-specific method names and session semantics belong in
//! a higher-level runtime implementation.

mod acp;
mod codex;
mod jsonl;
mod owner;
mod persistent_process;
mod rpc;
mod worker;

pub use codex::{
    CodexAppServerController, CodexApprovalPolicy, CodexReasoningEffort, CodexRuntimeError,
    CodexRuntimeEvent, CodexRuntimeLaunchConfig, CodexSandboxMode, CodexSessionAttachment,
    CodexSessionConfig, CodexSessionResult, CodexTurnConfig, CodexTurnSandboxPolicy,
    CodexTurnStartResult, CodexTurnStatus, CODEX_RUNTIME_KEY, DEFAULT_CONTROL_TIMEOUT,
    DEFAULT_MAX_FRAME_BYTES,
};
pub use jsonl::{JsonLineChannel, JsonLineEof, JsonLineError, JsonLineEvent, JsonLineFramer};
pub use owner::{
    ProviderRuntimeController, ProviderRuntimeKey, ProviderRuntimeOwner, ProviderRuntimeRegistry,
    RuntimeLifecycle, RuntimeRegistryError,
};
pub use persistent_process::{
    ExitStatusSnapshot, PersistentProcess, PersistentProcessError, PersistentProcessSpec,
    PersistentWriter, ProcessExit, ProcessExitKind, ProcessGeneration, ProcessGenerationGuard,
};
pub use rpc::{
    JsonLineWriter, RpcDisconnectReason, RpcEnvelopeMode, RpcError, RpcEvent, RpcId, RpcPeer,
    RpcServerRequest,
};
pub use worker::{
    PersistentRuntimeController, RuntimeCommand, RuntimeControllerError, RuntimeEvent,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::BufRead;
    use std::io::{self, Cursor, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn framer_splits_arbitrary_utf8_chunks() {
        let mut framer = JsonLineFramer::new(1024).unwrap();
        let input = "{\"message\":\"你好\"}\n{\"n\":2}\n".as_bytes();
        let mut frames = Vec::new();
        for byte in input {
            frames.extend(framer.push(&[*byte]).unwrap());
        }

        assert_eq!(frames, vec![json!({"message": "你好"}), json!({"n": 2})]);
        assert_eq!(framer.finish(), JsonLineEof::Clean);
    }

    #[test]
    fn framer_decodes_multiple_frames_from_one_read() {
        let mut framer = JsonLineFramer::new(1024).unwrap();
        assert_eq!(
            framer
                .push(
                    br#"{"a":1}
{"b":2}
"#
                )
                .unwrap(),
            vec![json!({"a": 1}), json!({"b": 2})]
        );
    }

    #[test]
    fn malformed_json_is_reported_explicitly() {
        let mut framer = JsonLineFramer::new(1024).unwrap();
        let error = framer.push(b"not-json\n").unwrap_err();
        assert!(matches!(error, JsonLineError::MalformedJson { .. }));
    }

    #[test]
    fn oversized_frame_is_rejected_before_buffer_growth() {
        let mut framer = JsonLineFramer::new(3).unwrap();
        assert!(matches!(
            framer.push(b"1234"),
            Err(JsonLineError::FrameTooLarge { max_bytes: 3 })
        ));
        assert_eq!(framer.buffered_bytes(), 3);
    }

    #[test]
    fn unclean_eof_is_distinguished_from_clean_eof() {
        let mut framer = JsonLineFramer::new(1024).unwrap();
        framer.push(br#"{"unfinished":true}"#).unwrap();
        assert_eq!(
            framer.finish(),
            JsonLineEof::Unclean {
                partial_bytes: br#"{"unfinished":true}"#.len()
            }
        );
    }

    #[test]
    fn channel_keeps_stdout_protocol_and_stderr_diagnostics_separate() {
        let channel = JsonLineChannel::spawn(
            Cursor::new(
                br#"{"ok":true}
"#
                .to_vec(),
            ),
            Cursor::new(b"diagnostic\n".to_vec()),
            1024,
        )
        .unwrap();
        let mut saw_frame = false;
        let mut saw_stderr = false;
        let mut saw_stdout_eof = false;
        let mut saw_stderr_eof = false;
        for _ in 0..4 {
            match channel.recv_timeout(Duration::from_secs(1)).unwrap() {
                JsonLineEvent::StdoutFrame(value) => {
                    assert_eq!(value, json!({"ok": true}));
                    saw_frame = true;
                }
                JsonLineEvent::StderrChunk(text) => {
                    assert_eq!(text, "diagnostic\n");
                    saw_stderr = true;
                }
                JsonLineEvent::StdoutEof { clean } => saw_stdout_eof = clean,
                JsonLineEvent::StderrEof => saw_stderr_eof = true,
                JsonLineEvent::Error(error) => panic!("unexpected channel error: {error:?}"),
            }
        }
        assert!(saw_frame && saw_stderr && saw_stdout_eof && saw_stderr_eof);
    }

    #[test]
    fn rpc_correlates_out_of_order_responses() {
        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        let first_peer = peer.clone();
        let first =
            thread::spawn(move || first_peer.request("first", json!({}), Duration::from_secs(1)));
        let second_peer = peer.clone();
        let second =
            thread::spawn(move || second_peer.request("second", json!({}), Duration::from_secs(1)));

        let mut requests = HashMap::new();
        for _ in 0..2 {
            let request: serde_json::Value =
                serde_json::from_slice(&written_rx.recv().unwrap()).unwrap();
            requests.insert(
                request["method"].as_str().unwrap().to_string(),
                request["id"].as_i64().unwrap(),
            );
        }
        input_tx
            .send(
                format!(
                    r#"{{"id":{},"result":"second-result"}}
"#,
                    requests["second"]
                )
                .into_bytes(),
            )
            .unwrap();
        input_tx
            .send(
                format!(
                    r#"{{"id":{},"result":"first-result"}}
"#,
                    requests["first"]
                )
                .into_bytes(),
            )
            .unwrap();

        assert_eq!(first.join().unwrap().unwrap(), json!("first-result"));
        assert_eq!(second.join().unwrap().unwrap(), json!("second-result"));
    }

    #[test]
    fn notifications_and_server_requests_are_separate_from_responses() {
        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, _written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        input_tx
            .send(
                br#"{"method":"notice","params":{"value":1}}
{"id":"server-1","method":"ask","params":{"value":2}}
"#
                .to_vec(),
            )
            .unwrap();

        assert_eq!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::Notification {
                method: "notice".into(),
                params: json!({"value": 1})
            }
        );
        assert_eq!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::ServerRequest(RpcServerRequest {
                id: RpcId::String("server-1".into()),
                method: "ask".into(),
                params: json!({"value": 2})
            })
        );
    }

    #[test]
    fn rpc_envelope_modes_preserve_bare_and_emit_json_rpc_2() {
        let (_bare_input_tx, bare_input_rx) = mpsc::channel();
        let (bare_written_tx, bare_written_rx) = mpsc::channel();
        let bare = test_peer_with_mode(bare_input_rx, bare_written_tx, RpcEnvelopeMode::Bare);
        bare.notify("notice", json!({ "value": 1 })).unwrap();
        let bare_frame: serde_json::Value =
            serde_json::from_slice(&bare_written_rx.recv().unwrap()).unwrap();
        assert_eq!(
            bare_frame,
            json!({ "method": "notice", "params": { "value": 1 } })
        );

        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, written_rx) = mpsc::channel();
        let peer = test_peer_with_mode(input_rx, written_tx, RpcEnvelopeMode::JsonRpc2);
        let request_peer = peer.clone();
        let request =
            thread::spawn(move || request_peer.request("ask", json!({}), Duration::from_secs(1)));
        let request_frame: serde_json::Value =
            serde_json::from_slice(&written_rx.recv().unwrap()).unwrap();
        assert_eq!(request_frame["jsonrpc"], "2.0");
        let request_id = request_frame["id"].clone();
        input_tx
            .send(
                format!(
                    r#"{{"jsonrpc":"2.0","id":{request_id},"result":{{"ok":true}}}}
"#
                )
                .into_bytes(),
            )
            .unwrap();
        assert_eq!(request.join().unwrap().unwrap(), json!({ "ok": true }));

        peer.notify("notice", json!({})).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&written_rx.recv().unwrap()).unwrap()
                ["jsonrpc"],
            "2.0"
        );
        peer.respond_success(RpcId::String("success".into()), json!({}))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&written_rx.recv().unwrap()).unwrap()
                ["jsonrpc"],
            "2.0"
        );
        peer.respond_error(RpcId::Number(9), json!(-32601), "not implemented", None)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&written_rx.recv().unwrap()).unwrap()
                ["jsonrpc"],
            "2.0"
        );

        input_tx
            .send(
                br#"{"jsonrpc":"2.0","id":"server-2","method":"server/ask","params":{}}
"#
                .to_vec(),
            )
            .unwrap();
        assert!(matches!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::ServerRequest(RpcServerRequest {
                id: RpcId::String(id),
                method,
                ..
            }) if id == "server-2" && method == "server/ask"
        ));
    }

    #[test]
    fn request_timeout_removes_pending_and_late_response_is_unmatched() {
        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        let timed_out = peer.request("slow", json!({}), Duration::from_millis(20));
        assert!(matches!(timed_out, Err(RpcError::RequestTimeout { .. })));
        let first_request: serde_json::Value =
            serde_json::from_slice(&written_rx.recv().unwrap()).unwrap();
        input_tx
            .send(
                format!(
                    r#"{{"id":{},"result":"late"}}
"#,
                    first_request["id"]
                )
                .into_bytes(),
            )
            .unwrap();
        assert!(matches!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::UnmatchedResponse { .. }
        ));

        let second_peer = peer.clone();
        let second =
            thread::spawn(move || second_peer.request("fast", json!({}), Duration::from_secs(1)));
        let second_request: serde_json::Value =
            serde_json::from_slice(&written_rx.recv().unwrap()).unwrap();
        input_tx
            .send(
                format!(
                    r#"{{"id":{},"result":"ok"}}
"#,
                    second_request["id"]
                )
                .into_bytes(),
            )
            .unwrap();
        assert_eq!(second.join().unwrap().unwrap(), json!("ok"));
    }

    #[test]
    fn disconnect_fails_pending_requests() {
        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, _written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        let request_peer = peer.clone();
        let request = thread::spawn(move || {
            request_peer.request("pending", json!({}), Duration::from_secs(5))
        });
        thread::sleep(Duration::from_millis(20));
        drop(input_tx);
        assert!(matches!(
            request.join().unwrap(),
            Err(RpcError::Disconnected { .. })
        ));
    }

    #[test]
    fn malformed_channel_frame_disconnects_rpc_peer() {
        let (input_tx, input_rx) = mpsc::channel();
        let (written_tx, _written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        input_tx.send(b"broken-json\n".to_vec()).unwrap();
        assert!(matches!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::Disconnected {
                reason: RpcDisconnectReason::MalformedFrame(_)
            }
        ));
    }

    #[test]
    fn explicit_rpc_disconnect_is_not_reported_as_an_unexpected_exit() {
        let (_input_tx, input_rx) = mpsc::channel();
        let (written_tx, _written_rx) = mpsc::channel();
        let peer = test_peer(input_rx, written_tx);
        peer.disconnect();
        assert_eq!(
            peer.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            RpcEvent::Disconnected {
                reason: RpcDisconnectReason::Explicit
            }
        );
    }

    #[test]
    fn process_generation_is_distinct_and_exit_callbacks_are_identity_bound() {
        let first = PersistentProcess::spawn(test_process_spec()).unwrap();
        let first_generation = first.generation();
        let second = PersistentProcess::spawn(test_process_spec()).unwrap();
        assert_ne!(first_generation, second.generation());
        let first_exit = first.shutdown(Duration::from_millis(100)).unwrap();
        let second_exit = second.shutdown(Duration::from_millis(100)).unwrap();
        assert_eq!(first_exit.generation, first_generation);
        assert_eq!(second_exit.generation, second.generation());
        assert_ne!(first_exit.generation, second_exit.generation);
        assert_eq!(first_exit.kind, ProcessExitKind::ExpectedShutdown);
    }

    #[test]
    fn child_exit_without_shutdown_is_reported_as_unexpected() {
        let process = PersistentProcess::spawn(exit_process_spec()).unwrap();
        let exit = process
            .subscribe_exit()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(exit.generation, process.generation());
        assert_eq!(exit.kind, ProcessExitKind::UnexpectedExit);
    }

    #[test]
    fn generation_guard_rejects_a_late_callback_from_the_previous_process() {
        let guard = ProcessGenerationGuard::new();
        let first = ProcessExit {
            generation: 3,
            pid: 1000,
            status: None,
            kind: ProcessExitKind::UnexpectedExit,
        };
        let second = ProcessExit {
            generation: 4,
            pid: 2000,
            status: None,
            kind: ProcessExitKind::UnexpectedExit,
        };
        guard.install(first.generation);
        guard.install(second.generation);
        assert!(!guard.accepts(&first));
        assert!(guard.accepts(&second));
        assert!(!guard.install(first.generation));
    }

    #[test]
    fn persistent_writer_sends_multiple_lines_to_one_child_lifetime() {
        let process = PersistentProcess::spawn(echo_process_spec()).unwrap();
        let stdout = process.take_stdout().unwrap();
        let reader = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut lines = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                lines.push(line);
            }
            lines
        });
        let writer = process.writer();
        writer.write_all(b"first\n").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"second\n").unwrap();
        writer.flush().unwrap();
        let exit = process.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(exit.kind, ProcessExitKind::ExpectedShutdown);
        let lines = reader.join().unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| line.trim_end_matches(['\r', '\n']))
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn controller_worker_uses_the_persistent_rpc_path() {
        let controller = PersistentRuntimeController::spawn(echo_rpc_process_spec(), 1024).unwrap();
        assert_eq!(
            controller
                .request("ping", json!({"value": 1}), Duration::from_secs(1))
                .unwrap(),
            json!("ok")
        );
        let exit = controller
            .shutdown_with_grace(Duration::from_secs(1))
            .unwrap();
        assert_eq!(exit.kind, ProcessExitKind::ExpectedShutdown);
    }

    #[test]
    fn registry_single_flight_shares_one_controller() {
        let registry = ProviderRuntimeRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let registry = registry.clone();
            let calls = Arc::clone(&calls);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                registry
                    .get_or_init("shared", || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(30));
                        Ok(Arc::new(TestController) as Arc<dyn ProviderRuntimeController>)
                    })
                    .unwrap()
            }));
        }
        let controllers = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for controller in &controllers[1..] {
            assert!(Arc::ptr_eq(&controllers[0], controller));
        }
    }

    #[test]
    fn registry_exposes_lifecycle_and_closes_after_owner_shutdown() {
        let owner = ProviderRuntimeOwner::new();
        let key = "lifecycle";
        assert_eq!(owner.registry().lifecycle(key), None);
        owner
            .get_or_init(key, || {
                Ok(Arc::new(TestController) as Arc<dyn ProviderRuntimeController>)
            })
            .unwrap();
        assert_eq!(
            owner.registry().lifecycle(key),
            Some(RuntimeLifecycle::Ready)
        );
        assert!(owner.shutdown().is_empty());
        assert_eq!(
            owner.registry().lifecycle(key),
            Some(RuntimeLifecycle::Stopped)
        );
        assert!(matches!(
            owner.get_or_init(key, || Ok(
                Arc::new(TestController) as Arc<dyn ProviderRuntimeController>
            )),
            Err(RuntimeRegistryError::ShuttingDown)
        ));
    }

    #[derive(Debug)]
    struct TestController;

    impl ProviderRuntimeController for TestController {
        fn shutdown(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestWriter {
        tx: Sender<Vec<u8>>,
    }

    impl JsonLineWriter for TestWriter {
        fn write_json(&self, value: &serde_json::Value) -> Result<(), String> {
            let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
            bytes.push(b'\n');
            self.tx.send(bytes).map_err(|error| error.to_string())
        }
    }

    struct ChunkReader {
        chunks: Receiver<Vec<u8>>,
        current: Cursor<Vec<u8>>,
    }

    impl Read for ChunkReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            if self.current.position() as usize == self.current.get_ref().len() {
                let chunk = self.chunks.recv().map_err(|_| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "test input ended")
                })?;
                self.current = Cursor::new(chunk);
            }
            self.current.read(target)
        }
    }

    fn test_peer(input_rx: Receiver<Vec<u8>>, written_tx: Sender<Vec<u8>>) -> RpcPeer {
        test_peer_with_mode(input_rx, written_tx, RpcEnvelopeMode::Bare)
    }

    fn test_peer_with_mode(
        input_rx: Receiver<Vec<u8>>,
        written_tx: Sender<Vec<u8>>,
        mode: RpcEnvelopeMode,
    ) -> RpcPeer {
        let stdout = ChunkReader {
            chunks: input_rx,
            current: Cursor::new(Vec::new()),
        };
        let channel = JsonLineChannel::spawn(stdout, Cursor::new(Vec::<u8>::new()), 1024).unwrap();
        RpcPeer::new_with_writer_and_mode(channel, Arc::new(TestWriter { tx: written_tx }), mode)
    }

    fn test_process_spec() -> PersistentProcessSpec {
        #[cfg(windows)]
        {
            PersistentProcessSpec::new("cmd.exe").args([
                "/d",
                "/c",
                "ping",
                "-n",
                "60",
                "127.0.0.1",
                ">",
                "nul",
            ])
        }
        #[cfg(not(windows))]
        {
            PersistentProcessSpec::new("sh").args(["-c", "sleep 60"])
        }
    }

    fn echo_process_spec() -> PersistentProcessSpec {
        #[cfg(windows)]
        {
            PersistentProcessSpec::new("powershell.exe").args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "$input | ForEach-Object { $_ }",
            ])
        }
        #[cfg(not(windows))]
        {
            PersistentProcessSpec::new("sh").args([
                "-c",
                "while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
            ])
        }
    }

    fn echo_rpc_process_spec() -> PersistentProcessSpec {
        #[cfg(windows)]
        {
            PersistentProcessSpec::new("powershell.exe").args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "$line = [Console]::ReadLine(); while ($null -ne $line) { $request = $line | ConvertFrom-Json; @{ id = $request.id; result = 'ok' } | ConvertTo-Json -Compress; $line = [Console]::ReadLine() }",
            ])
        }
        #[cfg(not(windows))]
        {
            PersistentProcessSpec::new("sh").args([
                "-c",
                "while IFS= read -r line; do id=$(printf '%s\\n' \"$line\" | sed -n 's/.*\\\"id\\\":\\([0-9][0-9]*\\).*/\\1/p'); printf '{\"id\":%s,\"result\":\"ok\"}\\n' \"$id\"; done",
            ])
        }
    }

    fn exit_process_spec() -> PersistentProcessSpec {
        #[cfg(windows)]
        {
            PersistentProcessSpec::new("cmd.exe").args(["/d", "/c", "exit", "7"])
        }
        #[cfg(not(windows))]
        {
            PersistentProcessSpec::new("sh").args(["-c", "exit 7"])
        }
    }
}
pub use acp::{
    AcpAuthentication, AcpConfigOptionUpdate, AcpController, AcpExtensionRequestHandler,
    AcpLaunchConfig, AcpPermissionDecision, AcpPermissionRequest, AcpPermissionResolver,
    AcpRuntimeError, AcpRuntimeEvent, AcpSessionAttachment, AcpSessionConfig, AcpSessionOrigin,
    AcpTurnStatus, AcpWorkspacePermissionPolicy, ACP_PROTOCOL_VERSION,
};
