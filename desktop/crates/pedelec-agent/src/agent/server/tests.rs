use super::*;
use crate::agent::backend::{
    InferenceEvent, InferenceEventSink, InferenceRequest, InferenceResult, InferenceUsage,
    ModelCapabilities,
};
use crate::agent::config::{BackendKind, PedelecAgentServerConfig};
use crate::agent::conversation::{NormalizedToolCall, SESSION_SCHEMA_VERSION};
use crate::agent::error::AgentError;
use crate::agent::store::load_committed_turn_records;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Hold {
    started: Mutex<bool>,
    started_cvar: Condvar,
    release: Mutex<bool>,
    release_cvar: Condvar,
}

impl Hold {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Mutex::new(false),
            started_cvar: Condvar::new(),
            release: Mutex::new(false),
            release_cvar: Condvar::new(),
        })
    }

    fn mark_started(&self) {
        *self.started.lock().unwrap() = true;
        self.started_cvar.notify_all();
    }

    fn wait_started(&self) {
        let mut started = self.started.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !*started {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (guard, result) = self.started_cvar.wait_timeout(started, remaining).unwrap();
            started = guard;
            if result.timed_out() && !*started {
                panic!("timed out waiting for backend hold start");
            }
        }
    }

    fn wait_release(&self) {
        let mut release = self.release.lock().unwrap();
        while !*release {
            release = self.release_cvar.wait(release).unwrap();
        }
    }

    fn release(&self) {
        *self.release.lock().unwrap() = true;
        self.release_cvar.notify_all();
    }
}

struct InferStep {
    deltas: Vec<String>,
    result: Result<InferenceResult, AgentError>,
    hold: Option<Arc<Hold>>,
}

struct ScriptedBackend {
    capabilities: ModelCapabilities,
    inspect_holds: Mutex<HashMap<String, Arc<Hold>>>,
    inspect_calls: Mutex<Vec<String>>,
    infer_steps: Mutex<Vec<InferStep>>,
    infer_calls: Mutex<u32>,
    requests: Mutex<Vec<InferenceRequest>>,
}

impl ScriptedBackend {
    fn new(capabilities: ModelCapabilities, steps: Vec<InferStep>) -> Arc<Self> {
        Arc::new(Self {
            capabilities,
            inspect_holds: Mutex::new(HashMap::new()),
            inspect_calls: Mutex::new(Vec::new()),
            infer_steps: Mutex::new(steps),
            infer_calls: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn set_inspect_hold(&self, model: &str, hold: Arc<Hold>) {
        self.inspect_holds
            .lock()
            .unwrap()
            .insert(model.to_string(), hold);
    }

    fn infer_calls(&self) -> u32 {
        *self.infer_calls.lock().unwrap()
    }
}

impl InferenceBackend for ScriptedBackend {
    fn inspect_model(&self, model: &str) -> Result<ModelCapabilities, AgentError> {
        self.inspect_calls.lock().unwrap().push(model.to_string());
        let hold = self.inspect_holds.lock().unwrap().get(model).cloned();
        if let Some(hold) = hold {
            hold.mark_started();
            hold.wait_release();
        }
        Ok(self.capabilities)
    }

    fn infer(
        &self,
        request: InferenceRequest,
        sink: &mut dyn InferenceEventSink,
    ) -> Result<InferenceResult, AgentError> {
        self.requests.lock().unwrap().push(request);
        *self.infer_calls.lock().unwrap() += 1;
        let step = self.infer_steps.lock().unwrap().remove(0);
        if let Some(hold) = &step.hold {
            hold.mark_started();
            hold.wait_release();
        }
        for delta in step.deltas {
            sink.on_event(InferenceEvent::TextDelta(delta));
        }
        step.result
    }
}

fn terminal(text: &str) -> InferStep {
    InferStep {
        deltas: vec![text.to_string()],
        result: Ok(InferenceResult {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            usage: Some(InferenceUsage::from_token_counts(Some(2), Some(1))),
            finish_reason: Some("stop".into()),
        }),
        hold: None,
    }
}

fn failing(code: &str) -> InferStep {
    InferStep {
        deltas: vec![],
        result: Err(AgentError::new(code, "backend failed")),
        hold: None,
    }
}

fn held_terminal(text: &str, hold: Arc<Hold>) -> InferStep {
    let mut step = terminal(text);
    step.hold = Some(hold);
    step
}

fn tool_result(calls: Vec<NormalizedToolCall>) -> InferStep {
    InferStep {
        deltas: Vec::new(),
        result: Ok(InferenceResult {
            text: Some("working".into()),
            tool_calls: calls,
            usage: Some(InferenceUsage::from_token_counts(Some(2), Some(1))),
            finish_reason: None,
        }),
        hold: None,
    }
}

fn held_tool_result(calls: Vec<NormalizedToolCall>, hold: Arc<Hold>) -> InferStep {
    let mut step = tool_result(calls);
    step.hold = Some(hold);
    step
}

struct FrameSink {
    frames: Arc<Mutex<Vec<Value>>>,
    cvar: Arc<Condvar>,
    buf: Vec<u8>,
}

impl Write for FrameSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(index) = self.buf.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buf.drain(..=index).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(&line).expect("utf8 jsonl");
            let value: Value = serde_json::from_str(text).expect("json-rpc frame");
            self.frames.lock().unwrap().push(value);
            self.cvar.notify_all();
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Harness {
    server: Arc<PedelecAgentServer>,
    stdin: std::io::PipeWriter,
    frames: Arc<Mutex<Vec<Value>>>,
    cvar: Arc<Condvar>,
    join: JoinHandle<ServeOutcome>,
    next_id: u64,
    workspace: PathBuf,
}

fn server_config(session_root: PathBuf) -> PedelecAgentServerConfig {
    PedelecAgentServerConfig {
        provider: BackendKind::Ollama,
        base_url: "http://127.0.0.1:1".into(),
        timeout_ms: 1000,
        api_key: "ollama".into(),
        tavily_api_key: None,
        pedelec_cli_path: None,
        pedelec_deno_path: None,
        core_runtime_file: None,
        session_root: Some(session_root),
        max_transcript_bytes: 64_000,
        max_tool_rounds: 8,
        max_list_files: 200,
        max_file_bytes: 1024,
        max_image_bytes: 20 * 1024 * 1024,
        pedelec_cli_timeout_ms: 1000,
        pedelec_deno_timeout_ms: 1000,
    }
}

fn start_server(temp: &tempfile::TempDir, backend: Arc<dyn InferenceBackend>) -> Harness {
    let (stdin_read, stdin_write) = std::io::pipe().unwrap();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let cvar = Arc::new(Condvar::new());
    let stdout = FrameSink {
        frames: Arc::clone(&frames),
        cvar: Arc::clone(&cvar),
        buf: Vec::new(),
    };
    let server = PedelecAgentServer::new(
        server_config(temp.path().join("agent-home")),
        backend,
        Box::new(stdout),
        ServeOptions {
            exit_process_on_shutdown: false,
            exit_process_on_writer_fatal: false,
        },
    );
    let join = {
        let server = Arc::clone(&server);
        thread::spawn(move || server.serve(BufReader::new(stdin_read)))
    };
    Harness {
        server,
        stdin: stdin_write,
        frames,
        cvar,
        join,
        next_id: 1,
        workspace: temp.path().canonicalize().unwrap(),
    }
}

impl Harness {
    fn send(&mut self, value: Value) {
        writeln!(self.stdin, "{value}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        id
    }

    fn initialize(&mut self) -> Value {
        let id = self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "test"}}),
        );
        self.wait_result(id)
    }

    fn open_session(&mut self, thread_id: &str, session_id: Option<&str>, model: &str) -> Value {
        let workspace = self.workspace.clone();
        self.open_session_with(thread_id, session_id, model, &workspace, Some("host"))
    }

    fn open_session_with(
        &mut self,
        thread_id: &str,
        session_id: Option<&str>,
        model: &str,
        workspace: &std::path::Path,
        host_instructions: Option<&str>,
    ) -> Value {
        let id = self.request(
            "session/open",
            json!({
                "threadId": thread_id,
                "sessionId": session_id,
                "model": model,
                "workspacePath": workspace,
                "hostInstructions": host_instructions
            }),
        );
        self.wait_result(id)
    }

    fn wait_result(&self, id: u64) -> Value {
        let frames = self.wait_until(|frames| {
            frames
                .iter()
                .any(|frame| frame.get("id") == Some(&json!(id)))
        });
        frames
            .into_iter()
            .find(|frame| frame.get("id") == Some(&json!(id)))
            .unwrap()
    }

    fn wait_until(&self, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frames = self.frames.lock().unwrap();
        loop {
            if predicate(&frames) {
                return frames.clone();
            }
            let now = Instant::now();
            if now >= deadline {
                panic!("timed out waiting for frames: {frames:?}");
            }
            let (guard, result) = self
                .cvar
                .wait_timeout(frames, deadline.saturating_duration_since(now))
                .unwrap();
            frames = guard;
            if result.timed_out() && !predicate(&frames) {
                panic!("timed out waiting for frames: {frames:?}");
            }
        }
    }

    fn snapshot(&self) -> Vec<Value> {
        self.frames.lock().unwrap().clone()
    }

    fn shutdown(mut self) -> Vec<Value> {
        let id = self.request("shutdown", json!({}));
        let _ = self.wait_result(id);
        let frames = Arc::clone(&self.frames);
        drop(self.stdin);
        let _ = self.join.join();
        let snapshot = frames.lock().unwrap().clone();
        snapshot
    }
}

fn tools_caps() -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        vision: false,
    }
}

fn assert_jsonrpc(frames: &[Value]) {
    for frame in frames {
        assert_eq!(frame["jsonrpc"], "2.0");
        assert!(
            frame.get("id").is_some() || frame.get("method").is_some(),
            "frame was not JSON-RPC: {frame}"
        );
        assert_ne!(frame.get("type").and_then(Value::as_str), Some("session"));
        assert_ne!(frame.get("type").and_then(Value::as_str), Some("status"));
        assert_ne!(frame.get("type").and_then(Value::as_str), Some("done"));
    }
}

fn error_code(frame: &Value) -> &str {
    frame["error"]["data"]["code"].as_str().unwrap()
}

#[test]
fn initialize_success_bad_version_and_pre_init_reject() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    let pre = h.request(
        "session/open",
        json!({
            "threadId": "t1",
            "model": "m",
            "workspacePath": h.workspace
        }),
    );
    let pre_frame = h.wait_result(pre);
    assert_eq!(error_code(&pre_frame), "NOT_INITIALIZED");

    let bad = h.request("initialize", json!({"protocolVersion": 99}));
    let bad_frame = h.wait_result(bad);
    assert_eq!(error_code(&bad_frame), "PROTOCOL_VERSION_UNSUPPORTED");

    let ok = h.initialize();
    assert_eq!(ok["result"]["protocolVersion"], 1);
    assert_eq!(ok["result"]["serverInfo"]["name"], "pedelec-agent");
    assert_eq!(ok["result"]["provider"], "ollama");
    assert_eq!(ok["result"]["capabilities"]["multipleSessions"], true);
    let again = h.initialize();
    assert_eq!(again["result"]["protocolVersion"], 1);
    assert_jsonrpc(&h.shutdown());
}

#[test]
fn stdout_frames_are_jsonrpc_only() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("hi")]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let turn = h.request(
        "turn/start",
        json!({
            "threadId": "t1",
            "sessionId": session_id,
            "turnId": "local_1",
            "message": "hello"
        }),
    );
    h.wait_until(|frames| {
        frames
            .iter()
            .any(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
            && frames
                .iter()
                .any(|frame| frame.get("id") == Some(&json!(turn)))
    });
    assert_jsonrpc(&h.shutdown());
}

#[test]
fn concurrent_request_ids_complete_independently() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let slow = Hold::new();
    backend.set_inspect_hold("slow", Arc::clone(&slow));
    let mut h = start_server(&temp, backend);
    h.initialize();
    let slow_id = h.request(
        "session/open",
        json!({
            "threadId": "slow",
            "model": "slow",
            "workspacePath": h.workspace
        }),
    );
    slow.wait_started();
    let ping = h.request("initialize", json!({"protocolVersion": 1}));
    let ping_frame = h.wait_result(ping);
    assert!(ping_frame.get("result").is_some());
    assert!(h
        .snapshot()
        .iter()
        .all(|frame| frame.get("id") != Some(&json!(slow_id))));
    slow.release();
    let slow_frame = h.wait_result(slow_id);
    assert!(slow_frame.get("result").is_some());
    h.shutdown();
}

#[test]
fn multiple_sessions_open_in_one_process() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let a = h.open_session("tA", None, "m");
    let b = h.open_session("tB", None, "m");
    assert_ne!(a["result"]["sessionId"], b["result"]["sessionId"]);
    assert_eq!(a["result"]["alreadyAttached"], false);
    h.shutdown();
}

#[test]
fn slow_open_does_not_block_other_session_control() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let slow = Hold::new();
    backend.set_inspect_hold("slow", Arc::clone(&slow));
    let mut h = start_server(&temp, backend);
    h.initialize();
    let a = h.request(
        "session/open",
        json!({
            "threadId": "tA",
            "model": "slow",
            "workspacePath": h.workspace
        }),
    );
    slow.wait_started();
    let b = h.open_session("tB", None, "fast");
    assert!(b.get("result").is_some());
    let close = h.request(
        "session/close",
        json!({
            "threadId": "tB",
            "sessionId": b["result"]["sessionId"]
        }),
    );
    let close_frame = h.wait_result(close);
    assert_eq!(close_frame["result"]["closed"], true);
    slow.release();
    assert!(h.wait_result(a).get("result").is_some());
    h.shutdown();
}

#[test]
fn reopen_same_thread_session_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let first = h.open_session("t1", None, "m");
    let session_id = first["result"]["sessionId"].as_str().unwrap().to_string();
    let second = h.open_session("t1", Some(&session_id), "m");
    assert_eq!(second["result"]["sessionId"], session_id);
    assert_eq!(second["result"]["alreadyAttached"], true);
    h.shutdown();
}

#[test]
fn same_session_cannot_attach_second_thread() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let first = h.open_session("t1", None, "m");
    let session_id = first["result"]["sessionId"].as_str().unwrap().to_string();
    let second = h.open_session("t2", Some(&session_id), "m");
    assert_eq!(error_code(&second), "ATTACHMENT_CONFLICT");
    h.shutdown();
}

#[test]
fn old_schema_resume_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let close = h.request(
        "session/close",
        json!({"threadId": "t1", "sessionId": session_id}),
    );
    h.wait_result(close);
    let path = find_session_json(temp.path().join("agent-home"), &session_id);
    let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    meta["schemaVersion"] = json!(1);
    std::fs::write(&path, meta.to_string()).unwrap();
    let resumed = h.open_session("t1", Some(&session_id), "m");
    assert_eq!(error_code(&resumed), "SESSION_SCHEMA_INCOMPATIBLE");
    h.shutdown();
}

#[test]
fn tools_false_open_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(
        ModelCapabilities {
            tools: false,
            vision: false,
        },
        vec![],
    );
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    assert_eq!(error_code(&opened), "MODEL_TOOLS_UNSUPPORTED");
    h.shutdown();
}

#[test]
fn two_sessions_can_run_turns_concurrently() {
    let temp = tempfile::tempdir().unwrap();
    let hold_a = Hold::new();
    let hold_b = Hold::new();
    let backend = ScriptedBackend::new(
        tools_caps(),
        vec![
            held_terminal("a", Arc::clone(&hold_a)),
            held_terminal("b", Arc::clone(&hold_b)),
        ],
    );
    let mut h = start_server(&temp, backend);
    h.initialize();
    let a = h.open_session("tA", None, "m");
    let b = h.open_session("tB", None, "m");
    let sid_a = a["result"]["sessionId"].as_str().unwrap().to_string();
    let sid_b = b["result"]["sessionId"].as_str().unwrap().to_string();
    let turn_a = h.request(
        "turn/start",
        json!({"threadId":"tA","sessionId":sid_a,"turnId":"ta","message":"a"}),
    );
    let turn_b = h.request(
        "turn/start",
        json!({"threadId":"tB","sessionId":sid_b,"turnId":"tb","message":"b"}),
    );
    h.wait_result(turn_a);
    h.wait_result(turn_b);
    hold_a.wait_started();
    hold_b.wait_started();
    hold_a.release();
    hold_b.release();
    h.wait_until(|frames| {
        frames
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
            .count()
            == 2
    });
    h.shutdown();
}

#[test]
fn same_session_second_turn_is_busy() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Hold::new();
    let backend = ScriptedBackend::new(tools_caps(), vec![held_terminal("a", Arc::clone(&hold))]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let first = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"t1","message":"a"}),
    );
    h.wait_result(first);
    hold.wait_started();
    let second = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"t2","message":"b"}),
    );
    let second_frame = h.wait_result(second);
    assert_eq!(error_code(&second_frame), "SESSION_BUSY");
    hold.release();
    h.shutdown();
}

#[test]
fn duplicate_active_turn_does_not_rerun() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Hold::new();
    let backend = ScriptedBackend::new(tools_caps(), vec![held_terminal("a", Arc::clone(&hold))]);
    let cloned = Arc::clone(&backend);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let params = json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"a"});
    let first = h.request("turn/start", params.clone());
    h.wait_result(first);
    hold.wait_started();
    let second = h.request("turn/start", params);
    let second_frame = h.wait_result(second);
    assert_eq!(second_frame["result"]["alreadyStarted"], true);
    assert_eq!(cloned.infer_calls(), 1);
    hold.release();
    h.shutdown();
}

#[test]
fn completed_turn_id_is_not_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("done")]);
    let cloned = Arc::clone(&backend);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let params = json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"a"});
    let first = h.request("turn/start", params.clone());
    h.wait_until(|frames| {
        frames
            .iter()
            .any(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
    });
    let _ = first;
    let second = h.request("turn/start", params);
    let second_frame = h.wait_result(second);
    assert_eq!(error_code(&second_frame), "TURN_ALREADY_COMPLETED");
    assert_eq!(cloned.infer_calls(), 1);
    h.shutdown();
}

#[test]
fn turn_start_response_precedes_started_and_later_events() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("final")]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    let frames = h.wait_until(|frames| {
        frames
            .iter()
            .any(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
    });
    let response_idx = frames
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(turn)))
        .unwrap();
    let started_idx = frames
        .iter()
        .position(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/started"))
        .unwrap();
    let first_event_idx = frames
        .iter()
        .position(|frame| {
            matches!(
                frame.get("method").and_then(Value::as_str),
                Some("turn/assistant_delta")
                    | Some("turn/usage")
                    | Some("turn/tool_call")
                    | Some("turn/tool_result")
                    | Some("turn/assistant_message")
                    | Some("turn/completed")
            )
        })
        .unwrap();
    assert!(response_idx < started_idx);
    assert!(started_idx < first_event_idx);
    assert_eq!(frames[response_idx]["result"]["accepted"], true);
    h.shutdown();
}

#[test]
fn backend_failure_after_admission_completes_failed() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![failing("OLLAMA_UNAVAILABLE")]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    let response = h.wait_result(turn);
    assert_eq!(response["result"]["accepted"], true);
    let frames = h.wait_until(|frames| {
        frames.iter().any(|frame| {
            frame.get("method").and_then(Value::as_str) == Some("turn/completed")
                && frame["params"]["status"] == "failed"
        })
    });
    let completed = frames
        .iter()
        .find(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
        .unwrap();
    assert_eq!(completed["params"]["error"]["code"], "OLLAMA_UNAVAILABLE");
    assert!(!frames
        .iter()
        .any(|frame| { frame.get("id") == Some(&json!(turn)) && frame.get("error").is_some() }));
    h.shutdown();
}

#[test]
fn close_running_session_does_not_wait_for_backend() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Hold::new();
    let backend =
        ScriptedBackend::new(tools_caps(), vec![held_terminal("late", Arc::clone(&hold))]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    hold.wait_started();
    let started = Instant::now();
    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id,"activeTurnId":"local_1"}),
    );
    let close_frame = h.wait_result(close);
    assert_eq!(close_frame["result"]["closed"], true);
    assert!(started.elapsed() < Duration::from_millis(500));
    let close_idx = h
        .snapshot()
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(close)))
        .unwrap();
    hold.release();
    thread::sleep(Duration::from_millis(150));
    let frames = h.snapshot();
    let stale = frames.iter().skip(close_idx + 1).any(|frame| {
        frame
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| {
                method.starts_with("turn/") && frame["params"]["threadId"] == "t1"
            })
    });
    assert!(!stale, "stale events after close: {frames:?}");
    let transcript = find_session_json(temp.path().join("agent-home"), &session_id)
        .parent()
        .unwrap()
        .join("transcript.jsonl");
    assert_eq!(std::fs::read_to_string(transcript).unwrap().trim(), "");
    h.shutdown();
}

#[test]
fn late_inference_tool_result_is_discarded_after_close() {
    let temp = tempfile::tempdir().unwrap();
    let infer_hold = Hold::new();
    let backend = ScriptedBackend::new(
        tools_caps(),
        vec![held_tool_result(
            vec![NormalizedToolCall::new(
                "",
                "fs.list_text_files",
                json!({"dir": ".", "maxDepth": 1}),
            )],
            Arc::clone(&infer_hold),
        )],
    );
    let backend_probe = Arc::clone(&backend);
    let tool_starts = Arc::new(AtomicUsize::new(0));
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let attachment = h.server.registry.get_by_thread("t1").unwrap();
    h.server.set_tool_start_hook(
        "t1",
        Arc::new({
            let tool_starts = Arc::clone(&tool_starts);
            move || {
                tool_starts.fetch_add(1, Ordering::SeqCst);
            }
        }),
    );

    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    infer_hold.wait_started();

    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id,"activeTurnId":"local_1"}),
    );
    let close_frame = h.wait_result(close);
    assert_eq!(close_frame["result"]["closed"], true);
    let close_idx = h
        .snapshot()
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(close)))
        .unwrap();

    infer_hold.release();
    drop(attachment.session.lock().unwrap());

    assert_eq!(tool_starts.load(Ordering::SeqCst), 0);
    assert_eq!(backend_probe.infer_calls(), 1);
    let transcript = find_session_json(temp.path().join("agent-home"), &session_id)
        .parent()
        .unwrap()
        .join("transcript.jsonl");
    assert_eq!(std::fs::read_to_string(transcript).unwrap().trim(), "");
    let frames = h.snapshot();
    let stale = frames.iter().skip(close_idx + 1).any(|frame| {
        frame
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.starts_with("turn/"))
    });
    assert!(!stale, "stale events after close: {frames:?}");
    h.shutdown();
}

#[test]
fn close_between_tool_admissions_prevents_later_tool_start() {
    let temp = tempfile::tempdir().unwrap();
    let first_tool_hold = Hold::new();
    let backend = ScriptedBackend::new(
        tools_caps(),
        vec![tool_result(vec![
            NormalizedToolCall::new("", "fs.list_text_files", json!({"dir": ".", "maxDepth": 1})),
            NormalizedToolCall::new("", "fs.list_text_files", json!({"dir": ".", "maxDepth": 1})),
        ])],
    );
    let backend_probe = Arc::clone(&backend);
    let tool_starts = Arc::new(AtomicUsize::new(0));
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let attachment = h.server.registry.get_by_thread("t1").unwrap();
    h.server.set_tool_start_hook(
        "t1",
        Arc::new({
            let tool_starts = Arc::clone(&tool_starts);
            let first_tool_hold = Arc::clone(&first_tool_hold);
            move || {
                let index = tool_starts.fetch_add(1, Ordering::SeqCst);
                if index == 0 {
                    first_tool_hold.mark_started();
                    first_tool_hold.wait_release();
                }
            }
        }),
    );

    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    first_tool_hold.wait_started();

    let started = Instant::now();
    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id,"activeTurnId":"local_1"}),
    );
    let close_frame = h.wait_result(close);
    assert_eq!(close_frame["result"]["closed"], true);
    assert!(started.elapsed() < Duration::from_millis(500));
    let close_idx = h
        .snapshot()
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(close)))
        .unwrap();

    first_tool_hold.release();
    drop(attachment.session.lock().unwrap());

    assert_eq!(tool_starts.load(Ordering::SeqCst), 1);
    assert_eq!(backend_probe.infer_calls(), 1);
    let transcript = find_session_json(temp.path().join("agent-home"), &session_id)
        .parent()
        .unwrap()
        .join("transcript.jsonl");
    assert_eq!(std::fs::read_to_string(transcript).unwrap().trim(), "");
    let frames = h.snapshot();
    let stale = frames.iter().skip(close_idx + 1).any(|frame| {
        frame
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.starts_with("turn/"))
    });
    assert!(!stale, "stale events after close: {frames:?}");
    h.shutdown();
}

#[test]
fn repeated_close_succeeds_and_conflict_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let a = h.open_session("t1", None, "m");
    let sid_a = a["result"]["sessionId"].as_str().unwrap().to_string();
    let first = h.request("session/close", json!({"threadId":"t1","sessionId":sid_a}));
    assert_eq!(h.wait_result(first)["result"]["closed"], true);
    let second = h.request("session/close", json!({"threadId":"t1","sessionId":sid_a}));
    assert_eq!(h.wait_result(second)["result"]["closed"], true);
    let missing = h.request(
        "session/close",
        json!({"threadId":"missing","sessionId":sid_a}),
    );
    assert_eq!(h.wait_result(missing)["result"]["closed"], true);
    let b = h.open_session("t2", None, "m");
    let sid_b = b["result"]["sessionId"].as_str().unwrap().to_string();
    let conflict = h.request("session/close", json!({"threadId":"t2","sessionId":sid_a}));
    assert_eq!(error_code(&h.wait_result(conflict)), "ATTACHMENT_CONFLICT");
    let _ = sid_b;
    h.shutdown();
}

#[test]
fn close_response_is_a_hard_commit_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Hold::new();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("late")]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    h.server.set_commit_admission_hook(
        "t1",
        Arc::new({
            let hold = Arc::clone(&hold);
            move || {
                hold.mark_started();
                hold.wait_release();
            }
        }),
    );
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    hold.wait_started();
    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id}),
    );
    let close_frame = h.wait_result(close);
    assert_eq!(close_frame["result"]["closed"], true);
    let close_idx = h
        .snapshot()
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(close)))
        .unwrap();
    let transcript = find_session_json(temp.path().join("agent-home"), &session_id)
        .parent()
        .unwrap()
        .join("transcript.jsonl");
    let transcript_at_close = std::fs::read_to_string(&transcript).unwrap();
    assert_eq!(transcript_at_close.trim(), "");
    hold.release();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        std::fs::read_to_string(&transcript).unwrap().trim(),
        transcript_at_close.trim()
    );
    assert_eq!(load_committed_turn_records(&transcript).unwrap().len(), 0);
    let frames = h.snapshot();
    let stale = frames.iter().skip(close_idx + 1).any(|frame| {
        frame
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.starts_with("turn/"))
    });
    assert!(!stale, "stale events after close: {frames:?}");
    h.shutdown();
}

#[test]
fn close_detaches_before_success_so_reopen_is_a_new_attachment() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("after-resume")]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id}),
    );
    assert_eq!(h.wait_result(close)["result"]["closed"], true);
    let reopened = h.open_session("t1", Some(&session_id), "m");
    assert_eq!(reopened["result"]["sessionId"], session_id);
    assert_eq!(reopened["result"]["alreadyAttached"], false);
    assert_eq!(reopened["result"]["resumed"], true);
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    h.wait_until(|frames| {
        frames
            .iter()
            .any(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
    });
    h.shutdown();
}

#[test]
fn already_attached_rejects_model_and_workspace_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let first = h.open_session("t1", None, "m");
    let session_id = first["result"]["sessionId"].as_str().unwrap().to_string();
    let same = h.open_session("t1", Some(&session_id), "m");
    assert_eq!(same["result"]["alreadyAttached"], true);
    let model_mismatch = h.open_session("t1", Some(&session_id), "other-model");
    assert_eq!(error_code(&model_mismatch), "INVALID_ARGUMENT");
    assert_eq!(model_mismatch["error"]["data"]["details"]["field"], "model");
    let workspace_mismatch = h.open_session_with(
        "t1",
        Some(&session_id),
        "m",
        &other.path().canonicalize().unwrap(),
        Some("host"),
    );
    assert_eq!(error_code(&workspace_mismatch), "INVALID_ARGUMENT");
    assert_eq!(
        workspace_mismatch["error"]["data"]["details"]["field"],
        "workspacePath"
    );
    h.shutdown();
}

#[test]
fn already_attached_refresh_applies_host_instructions() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![terminal("ok")]);
    let cloned = Arc::clone(&backend);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let first = h.open_session_with("t1", None, "m", &h.workspace.clone(), Some("first-host"));
    let session_id = first["result"]["sessionId"].as_str().unwrap().to_string();
    let refreshed = h.open_session_with(
        "t1",
        Some(&session_id),
        "m",
        &h.workspace.clone(),
        Some("second-host"),
    );
    assert_eq!(refreshed["result"]["alreadyAttached"], true);
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_until(|frames| {
        frames
            .iter()
            .any(|frame| frame.get("method").and_then(Value::as_str) == Some("turn/completed"))
    });
    let _ = turn;
    let prompt = cloned.requests.lock().unwrap()[0].messages[0]
        .content
        .clone()
        .unwrap();
    assert!(prompt.contains("second-host"));
    assert!(!prompt.contains("first-host"));
    h.shutdown();
}

#[test]
fn already_attached_refresh_is_busy_during_active_turn() {
    let temp = tempfile::tempdir().unwrap();
    let hold = Hold::new();
    let backend = ScriptedBackend::new(tools_caps(), vec![held_terminal("a", Arc::clone(&hold))]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let turn = h.request(
        "turn/start",
        json!({"threadId":"t1","sessionId":session_id,"turnId":"local_1","message":"hello"}),
    );
    h.wait_result(turn);
    hold.wait_started();
    let refresh = h.open_session_with(
        "t1",
        Some(&session_id),
        "m",
        &h.workspace.clone(),
        Some("changed-host"),
    );
    assert_eq!(error_code(&refresh), "SESSION_BUSY");
    hold.release();
    h.shutdown();
}

#[test]
fn detached_sessions_do_not_leave_inflight_lock_entries() {
    let temp = tempfile::tempdir().unwrap();
    let backend = ScriptedBackend::new(tools_caps(), vec![]);
    let mut h = start_server(&temp, backend);
    h.initialize();
    let opened = h.open_session("t1", None, "m");
    let session_id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let close = h.request(
        "session/close",
        json!({"threadId":"t1","sessionId":session_id}),
    );
    assert_eq!(h.wait_result(close)["result"]["closed"], true);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if h.server.inflight_lock_counts() == (0, 0) {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "inflight locks were not reclaimed: {:?}",
                h.server.inflight_lock_counts()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    h.shutdown();
}

fn find_session_json(agent_home: PathBuf, session_id: &str) -> PathBuf {
    let sessions = agent_home.join("sessions");
    for year in std::fs::read_dir(&sessions).unwrap() {
        for month in std::fs::read_dir(year.unwrap().path()).unwrap() {
            let path = month.unwrap().path().join(session_id).join("session.json");
            if path.exists() {
                return path;
            }
        }
    }
    panic!("session {session_id} not found under {sessions:?}");
}

#[test]
fn schema_version_constant_is_current() {
    assert_eq!(SESSION_SCHEMA_VERSION, 2);
}
