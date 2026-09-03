use super::backend::{
    InferenceBackend, InferenceEvent, InferenceEventSink, InferenceMessage, InferenceRequest,
    InferenceResult, ModelCapabilities,
};
use super::config::{AgentSessionConfig, PedelecAgentServerConfig, ToolHostConfig};
use super::conversation::{
    ActiveTurn, CommittedTurnRecord, ConversationMessage, InferenceAttachment, NormalizedToolCall,
};
use super::error::AgentError;
use super::events::{AgentTurnSink, TurnResult, TurnToolResult};
use super::instructions::compose_system_prompt;
use super::sandbox::Sandbox;
use super::store::{
    create_session_store, default_agent_home_dir, load_session_store, SessionStore,
};
use super::tavily::{TavilyClient, TavilyRoundWrapper};
use super::tools::{agent_tool_definitions, execute_tool_with_tavily, AgentToolDefinition};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStatus {
    Ready,
    Running { turn_id: String },
    Closed,
}

pub struct AgentSession {
    backend: Arc<dyn InferenceBackend>,
    store: SessionStore,
    sandbox: Sandbox,
    host_instructions: Option<String>,
    capabilities: ModelCapabilities,
    committed: Vec<ConversationMessage>,
    active_turn: Option<ActiveTurn>,
    tools: Vec<AgentToolDefinition>,
    tool_host: ToolHostConfig,
    tavily: Option<TavilyClient>,
    web_search_enabled: bool,
    max_tool_rounds: usize,
    max_transcript_bytes: u64,
    live: Arc<AtomicBool>,
    status: SessionStatus,
}

impl AgentSession {
    pub fn open(
        backend: Arc<dyn InferenceBackend>,
        server: &PedelecAgentServerConfig,
        session: AgentSessionConfig,
    ) -> Result<Self, AgentError> {
        if session.model.trim().is_empty() {
            return Err(AgentError::new("INVALID_ARGUMENT", "Model is required."));
        }
        let sandbox = Sandbox::new(
            &session.workspace_path,
            server.max_file_bytes,
            server.max_image_bytes,
            server.max_list_files,
        )?;
        let agent_home = match &server.session_root {
            Some(path) => path.clone(),
            None => default_agent_home_dir()?,
        };
        let provider = server.provider.as_str();
        let loaded = match session.requested_session_id.as_deref() {
            Some(session_id) => Some(load_session_store(
                &agent_home,
                session_id,
                provider,
                &session.model,
                sandbox.root(),
                server.max_transcript_bytes,
            )?),
            None => None,
        };

        let capabilities = backend.inspect_model(&session.model)?;
        if !capabilities.tools {
            return Err(AgentError::with_details(
                "MODEL_TOOLS_UNSUPPORTED",
                "The selected model does not support tool calling.",
                serde_json::json!({ "model": session.model, "capabilities": capabilities }),
            ));
        }

        let (store, committed) = match loaded {
            Some(pair) => pair,
            None => (
                create_session_store(&agent_home, provider, &session.model, sandbox.root())?,
                Vec::new(),
            ),
        };

        let web_search_enabled = server.web_search_enabled();
        let tools = agent_tool_definitions(capabilities.vision, web_search_enabled);
        let tavily = match server.tavily_api_key.clone() {
            Some(key) => Some(TavilyClient::new(key)?),
            None => None,
        };

        Ok(Self {
            backend,
            store,
            sandbox,
            host_instructions: session.host_instructions,
            capabilities,
            committed,
            active_turn: None,
            tools,
            tool_host: server.tool_host_config(),
            tavily,
            web_search_enabled,
            max_tool_rounds: server.max_tool_rounds,
            max_transcript_bytes: server.max_transcript_bytes,
            live: Arc::new(AtomicBool::new(true)),
            status: SessionStatus::Ready,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.store.metadata.session_id
    }

    pub fn resumed(&self) -> bool {
        self.store.resumed
    }

    pub fn model(&self) -> &str {
        &self.store.metadata.model
    }

    pub fn workspace_path(&self) -> &std::path::Path {
        &self.store.metadata.workspace_path
    }

    pub fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    pub fn tools(&self) -> &[AgentToolDefinition] {
        &self.tools
    }

    pub fn committed_messages(&self) -> &[ConversationMessage] {
        &self.committed
    }

    pub fn host_instructions(&self) -> Option<&str> {
        self.host_instructions.as_deref()
    }

    pub fn set_host_instructions(&mut self, host_instructions: Option<String>) {
        self.host_instructions = host_instructions;
    }

    pub fn live_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.live)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.status, SessionStatus::Closed) || !self.live.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) {
        self.live.store(false, Ordering::SeqCst);
        self.active_turn = None;
        self.status = SessionStatus::Closed;
    }

    pub fn run_turn(
        &mut self,
        turn_id: impl Into<String>,
        user_message: impl Into<String>,
        sink: &mut dyn AgentTurnSink,
    ) -> Result<TurnResult, AgentError> {
        let turn_id = turn_id.into();
        let user_message = user_message.into();
        self.ensure_open()?;
        match &self.status {
            SessionStatus::Running { turn_id: existing } => {
                return Err(AgentError::with_details(
                    "TURN_IN_PROGRESS",
                    "Session already has an active turn.",
                    serde_json::json!({ "turnId": existing }),
                ));
            }
            SessionStatus::Closed => {
                return Err(AgentError::new("SESSION_CLOSED", "Session is closed."));
            }
            SessionStatus::Ready => {}
        }
        if turn_id.trim().is_empty() {
            return Err(AgentError::new("INVALID_ARGUMENT", "turnId is required."));
        }
        if user_message.trim().is_empty() {
            return Err(AgentError::new(
                "INVALID_ARGUMENT",
                "User message is required.",
            ));
        }

        self.status = SessionStatus::Running {
            turn_id: turn_id.clone(),
        };
        self.active_turn = Some(ActiveTurn::begin(
            turn_id,
            ConversationMessage::user(user_message),
        ));
        match self.run_turn_inner(sink) {
            Ok(result) => Ok(result),
            Err(err) => {
                self.discard_active_turn();
                Err(err)
            }
        }
    }

    fn run_turn_inner(&mut self, sink: &mut dyn AgentTurnSink) -> Result<TurnResult, AgentError> {
        loop {
            self.ensure_live()?;
            let request = self.build_inference_request()?;
            let mut delta_sink = DeltaForwarder { inner: sink };
            let output = self.backend.infer(request, &mut delta_sink)?;
            self.record_round_usage(&output, sink)?;
            if let Some(active) = self.active_turn.as_mut() {
                active.clear_attachments();
            }

            let tool_calls = self.assign_tool_call_ids(output.tool_calls)?;
            self.push_active_message(
                ConversationMessage::assistant(output.text.clone(), tool_calls.clone()),
                Vec::new(),
            )?;

            if tool_calls.is_empty() {
                let text = output.text.unwrap_or_default();
                return self.commit_active_turn(text);
            }

            let round = self
                .active_turn
                .as_ref()
                .ok_or_else(|| invariant("Active turn missing during tool loop."))?
                .current_tool_round;
            if round >= self.max_tool_rounds {
                return Err(AgentError::new(
                    "MAX_TOOL_ROUNDS_EXCEEDED",
                    "The agent exceeded max tool rounds.",
                ));
            }
            self.execute_tool_calls(&tool_calls, sink)?;
            if let Some(active) = self.active_turn.as_mut() {
                active.current_tool_round += 1;
            }
        }
    }

    fn record_round_usage(
        &mut self,
        output: &InferenceResult,
        sink: &mut dyn AgentTurnSink,
    ) -> Result<(), AgentError> {
        let active = self
            .active_turn
            .as_mut()
            .ok_or_else(|| invariant("Active turn missing while recording usage."))?;
        if let Some(usage) = &output.usage {
            active.cumulative_usage.accumulate(usage);
        }
        sink.usage_updated(&active.cumulative_usage);
        Ok(())
    }

    fn assign_tool_call_ids(
        &self,
        mut calls: Vec<NormalizedToolCall>,
    ) -> Result<Vec<NormalizedToolCall>, AgentError> {
        let active = self
            .active_turn
            .as_ref()
            .ok_or_else(|| invariant("Active turn missing while assigning tool call ids."))?;
        for (index, call) in calls.iter_mut().enumerate() {
            if call.id.trim().is_empty() {
                call.id = format!(
                    "{}:r{}:{}",
                    active.turn_id, active.current_tool_round, index
                );
            }
            if call.name.trim().is_empty() {
                return Err(AgentError::new(
                    "INTERNAL_INVARIANT",
                    "Backend returned a tool call without a name.",
                ));
            }
            if matches!(call.arguments, Value::String(_)) {
                return Err(AgentError::new(
                    "INTERNAL_INVARIANT",
                    "Backend returned tool arguments as a JSON string.",
                ));
            }
        }
        Ok(calls)
    }

    fn execute_tool_calls(
        &mut self,
        calls: &[NormalizedToolCall],
        sink: &mut dyn AgentTurnSink,
    ) -> Result<(), AgentError> {
        let tavily = self.tavily.take();
        let result = self.execute_tool_calls_inner(calls, sink, tavily.as_ref());
        self.tavily = tavily;
        result
    }

    fn execute_tool_calls_inner(
        &mut self,
        calls: &[NormalizedToolCall],
        sink: &mut dyn AgentTurnSink,
        tavily: Option<&TavilyClient>,
    ) -> Result<(), AgentError> {
        let mut tavily_round = tavily.map(TavilyRoundWrapper::new);
        let mut images_in_round = 0;
        for call in calls {
            sink.tool_call(call);
            if call.name == "fs.read_image" && !self.capabilities.vision {
                return Err(AgentError::new(
                    "MODEL_VISION_UNSUPPORTED",
                    "Image tools are unavailable for this model.",
                ));
            }
            if call.name == "fs.read_image" {
                images_in_round += 1;
                if images_in_round > 4 {
                    let error = AgentError::new(
                        "TOO_MANY_IMAGES_IN_ROUND",
                        "At most 4 images may be read in one tool round.",
                    );
                    self.record_tool_error(call, error, sink)?;
                    continue;
                }
            }

            let session_id = self.session_id().to_string();
            match execute_tool_with_tavily(
                &call.name,
                &call.arguments,
                &session_id,
                &self.sandbox,
                &self.tool_host,
                tavily_round.as_mut(),
            ) {
                Ok(result) => {
                    let content = result.content.clone();
                    sink.tool_result(&TurnToolResult {
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        ok: true,
                        content: Some(content.clone()),
                        error: None,
                    });
                    self.push_active_message(
                        ConversationMessage::tool_result(
                            call.id.clone(),
                            call.name.clone(),
                            content.to_string(),
                        ),
                        result.attachments,
                    )?;
                }
                Err(error) => self.record_tool_error(call, error, sink)?,
            }
        }
        Ok(())
    }

    fn record_tool_error(
        &mut self,
        call: &NormalizedToolCall,
        error: AgentError,
        sink: &mut dyn AgentTurnSink,
    ) -> Result<(), AgentError> {
        let content = serde_json::json!({ "error": error });
        sink.tool_result(&TurnToolResult {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            ok: false,
            content: Some(content.clone()),
            error: Some(error),
        });
        self.push_active_message(
            ConversationMessage::tool_result(
                call.id.clone(),
                call.name.clone(),
                content.to_string(),
            ),
            Vec::new(),
        )
    }

    fn commit_active_turn(&mut self, text: String) -> Result<TurnResult, AgentError> {
        self.ensure_live()?;
        let active = self
            .active_turn
            .as_ref()
            .ok_or_else(|| invariant("Active turn missing at commit."))?;
        let turn_id = active.turn_id.clone();
        let usage = active.cumulative_usage.clone();
        let record = CommittedTurnRecord::new(turn_id.clone(), active.messages.clone());
        self.store
            .append_committed_turn(&record, self.max_transcript_bytes)?;
        let active = self
            .active_turn
            .take()
            .ok_or_else(|| invariant("Active turn missing after durable commit."))?;
        self.committed.extend(active.messages);
        self.store.touch()?;
        self.status = SessionStatus::Ready;
        Ok(TurnResult {
            turn_id,
            text,
            usage,
        })
    }

    fn build_inference_request(&self) -> Result<InferenceRequest, AgentError> {
        let active = self
            .active_turn
            .as_ref()
            .ok_or_else(|| invariant("Active turn missing while building inference request."))?;
        if active.messages.len() != active.attachments_by_message.len() {
            return Err(invariant(
                "Active turn message and attachment lists are out of sync.",
            ));
        }
        let mut messages = Vec::new();
        messages.push(InferenceMessage::system(compose_system_prompt(
            self.host_instructions.as_deref(),
            self.capabilities.vision,
            self.web_search_enabled,
        )));
        for message in &self.committed {
            messages.push(InferenceMessage::from_conversation(message, Vec::new()));
        }
        for (message, attachments) in active
            .messages
            .iter()
            .zip(active.attachments_by_message.iter())
        {
            messages.push(InferenceMessage::from_conversation(
                message,
                attachments.clone(),
            ));
        }
        Ok(InferenceRequest {
            model: self.store.metadata.model.clone(),
            messages,
            tools: self.tools.clone(),
        })
    }

    fn push_active_message(
        &mut self,
        message: ConversationMessage,
        attachments: Vec<InferenceAttachment>,
    ) -> Result<(), AgentError> {
        self.active_turn
            .as_mut()
            .ok_or_else(|| invariant("Active turn missing while appending a message."))?
            .push(message, attachments)
    }

    fn discard_active_turn(&mut self) {
        self.active_turn = None;
        if !matches!(self.status, SessionStatus::Closed) {
            self.status = SessionStatus::Ready;
        }
    }

    fn ensure_open(&self) -> Result<(), AgentError> {
        if matches!(self.status, SessionStatus::Closed) || !self.live.load(Ordering::SeqCst) {
            return Err(AgentError::new("SESSION_CLOSED", "Session is closed."));
        }
        Ok(())
    }

    fn ensure_live(&self) -> Result<(), AgentError> {
        if !self.live.load(Ordering::SeqCst) || matches!(self.status, SessionStatus::Closed) {
            return Err(AgentError::new(
                "TURN_INVALIDATED",
                "The turn was invalidated before commit.",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn force_running(&mut self, turn_id: &str) {
        self.status = SessionStatus::Running {
            turn_id: turn_id.to_string(),
        };
    }

    #[cfg(test)]
    fn transcript_path(&self) -> &std::path::Path {
        &self.store.transcript_path
    }

    #[cfg(test)]
    fn session_dir(&self) -> &std::path::Path {
        &self.store.dir
    }
}

struct DeltaForwarder<'a> {
    inner: &'a mut dyn AgentTurnSink,
}

impl InferenceEventSink for DeltaForwarder<'_> {
    fn on_event(&mut self, event: InferenceEvent) {
        match event {
            InferenceEvent::TextDelta(text) => self.inner.assistant_delta(&text),
        }
    }
}

fn invariant(message: &str) -> AgentError {
    AgentError::new("INTERNAL_INVARIANT", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::backend::{InferenceUsage, ModelCapabilities};
    use crate::agent::config::{BackendKind, PedelecAgentServerConfig};
    use crate::agent::conversation::{ConversationRole, SESSION_SCHEMA_VERSION};
    use crate::agent::events::IgnoringTurnSink;
    use crate::agent::store::load_committed_turn_records;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct ScriptedStep {
        deltas: Vec<String>,
        result: Result<InferenceResult, AgentError>,
        on_infer: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    struct ScriptedBackend {
        capabilities: ModelCapabilities,
        steps: Mutex<Vec<ScriptedStep>>,
        requests: Mutex<Vec<InferenceRequest>>,
    }

    impl ScriptedBackend {
        fn new(capabilities: ModelCapabilities, steps: Vec<ScriptedStep>) -> Arc<Self> {
            Arc::new(Self {
                capabilities,
                steps: Mutex::new(steps),
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    impl InferenceBackend for ScriptedBackend {
        fn inspect_model(&self, _model: &str) -> Result<ModelCapabilities, AgentError> {
            Ok(self.capabilities)
        }

        fn infer(
            &self,
            request: InferenceRequest,
            sink: &mut dyn InferenceEventSink,
        ) -> Result<InferenceResult, AgentError> {
            self.requests.lock().unwrap().push(request);
            let mut steps = self.steps.lock().unwrap();
            let step = steps.remove(0);
            drop(steps);
            if let Some(on_infer) = step.on_infer {
                on_infer();
            }
            for delta in step.deltas {
                sink.on_event(InferenceEvent::TextDelta(delta));
            }
            step.result
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        deltas: Vec<String>,
        usages: Vec<InferenceUsage>,
        tool_calls: Vec<NormalizedToolCall>,
        tool_results: Vec<TurnToolResult>,
    }

    impl AgentTurnSink for RecordingSink {
        fn assistant_delta(&mut self, text: &str) {
            self.deltas.push(text.to_string());
        }
        fn usage_updated(&mut self, usage: &InferenceUsage) {
            self.usages.push(usage.clone());
        }
        fn tool_call(&mut self, call: &NormalizedToolCall) {
            self.tool_calls.push(call.clone());
        }
        fn tool_result(&mut self, result: &TurnToolResult) {
            self.tool_results.push(result.clone());
        }
    }

    fn server_config(session_root: PathBuf, _workspace: PathBuf) -> PedelecAgentServerConfig {
        PedelecAgentServerConfig {
            provider: BackendKind::Ollama,
            base_url: "http://127.0.0.1:1".into(),
            timeout_ms: 1000,
            api_key: "ollama".into(),
            tavily_api_key: None,
            pedelec_cli_path: None,
            core_runtime_file: None,
            session_root: Some(session_root),
            max_transcript_bytes: 64_000,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            max_image_bytes: 20 * 1024 * 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    fn session_config(workspace: PathBuf) -> AgentSessionConfig {
        AgentSessionConfig {
            requested_session_id: None,
            model: "fake".into(),
            workspace_path: workspace,
            host_instructions: Some("[Pedelec Host Context]\nworkspace=/tmp".into()),
        }
    }

    fn terminal(text: &str, usage: InferenceUsage) -> ScriptedStep {
        ScriptedStep {
            deltas: vec![text.to_string()],
            result: Ok(InferenceResult {
                text: Some(text.to_string()),
                tool_calls: Vec::new(),
                usage: Some(usage),
                finish_reason: Some("stop".into()),
            }),
            on_infer: None,
        }
    }

    fn tool_round(name: &str, arguments: Value, usage: InferenceUsage) -> ScriptedStep {
        ScriptedStep {
            deltas: vec!["working".into()],
            result: Ok(InferenceResult {
                text: Some("working".into()),
                tool_calls: vec![NormalizedToolCall::new("", name, arguments)],
                usage: Some(usage),
                finish_reason: None,
            }),
            on_infer: None,
        }
    }

    fn open_session(
        temp: &tempfile::TempDir,
        backend: Arc<dyn InferenceBackend>,
        requested_session_id: Option<String>,
    ) -> AgentSession {
        let workspace = temp.path().canonicalize().unwrap();
        let mut session = session_config(workspace.clone());
        session.requested_session_id = requested_session_id;
        AgentSession::open(
            backend,
            &server_config(temp.path().join("agent-home"), workspace.clone()),
            session,
        )
        .unwrap()
    }

    #[test]
    fn tools_false_rejects_session_activation() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: false,
                vision: false,
            },
            vec![],
        );
        let workspace = temp.path().canonicalize().unwrap();
        let err = match AgentSession::open(
            backend,
            &server_config(temp.path().join("agent-home"), workspace.clone()),
            session_config(workspace),
        ) {
            Ok(_) => panic!("session opened without tool support"),
            Err(err) => err,
        };
        assert_eq!(err.code, "MODEL_TOOLS_UNSUPPORTED");
    }

    #[test]
    fn vision_flag_controls_image_tools() {
        let temp = tempfile::tempdir().unwrap();
        let no_vision = open_session(
            &temp,
            ScriptedBackend::new(
                ModelCapabilities {
                    tools: true,
                    vision: false,
                },
                vec![],
            ),
            None,
        );
        let names = no_vision
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"fs.read_text_file"));
        assert!(!names.contains(&"fs.read_image"));
        assert!(!names.contains(&"fs.list_image_files"));

        let with_vision = open_session(
            &temp,
            ScriptedBackend::new(
                ModelCapabilities {
                    tools: true,
                    vision: true,
                },
                vec![],
            ),
            None,
        );
        let names = with_vision
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"fs.read_image"));
        assert!(names.contains(&"fs.list_image_files"));
    }

    #[test]
    fn image_bytes_are_ephemeral_and_not_written_to_transcript() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("image.png"), [0_u8, 1, 2]).unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: true,
            },
            vec![
                tool_round(
                    "fs.read_image",
                    json!({"path": "image.png"}),
                    InferenceUsage::from_token_counts(Some(2), Some(1)),
                ),
                terminal("a cat", InferenceUsage::from_token_counts(Some(4), Some(2))),
            ],
        );
        let cloned = Arc::clone(&backend);
        let mut session = open_session(&temp, backend, None);
        session
            .run_turn("turn-1", "inspect", &mut IgnoringTurnSink)
            .unwrap();
        let requests = cloned.requests.lock().unwrap();
        let second = &requests[1];
        assert!(second
            .messages
            .iter()
            .any(|message| !message.attachments.is_empty()));
        let transcript = std::fs::read_to_string(session.transcript_path()).unwrap();
        assert!(!transcript.contains("AAEC"));
        assert!(transcript.contains("image.png"));
    }

    #[test]
    fn no_tool_terminal_answer_commits_one_turn() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello readme").unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![terminal(
                "final answer",
                InferenceUsage::from_token_counts(Some(11), Some(5)),
            )],
        );
        let mut session = open_session(&temp, backend, None);
        let mut sink = RecordingSink::default();
        let result = session.run_turn("turn-1", "hello", &mut sink).unwrap();

        assert_eq!(result.text, "final answer");
        assert_eq!(result.usage.total_tokens, Some(16));
        assert_eq!(sink.deltas, vec!["final answer"]);
        let records = load_committed_turn_records(session.transcript_path()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(records[0].turn_id, "turn-1");
        assert_eq!(session.committed_messages().len(), 2);
        let raw = std::fs::read_to_string(session.session_dir().join("session.json")).unwrap();
        assert!(raw.contains("\"schemaVersion\": 2"));
        assert!(!raw.contains("Pedelec Host Context"));
        let transcript = std::fs::read_to_string(session.transcript_path()).unwrap();
        assert!(!transcript.contains("You are pedelec-agent"));
        assert!(!transcript.contains("[Pedelec Host Context]"));
        assert!(!transcript.contains("PEDELEC_PREPARED"));
    }

    #[test]
    fn multi_round_tool_loop_commits_only_at_terminal_success() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello readme").unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![
                tool_round(
                    "fs.read_text_file",
                    json!({"path": "README.md"}),
                    InferenceUsage::from_token_counts(Some(4), Some(2)),
                ),
                terminal(
                    "final answer",
                    InferenceUsage::from_token_counts(Some(6), Some(3)),
                ),
            ],
        );
        let cloned = Arc::clone(&backend);
        let mut session = open_session(&temp, backend, None);
        let mut sink = RecordingSink::default();
        let result = session.run_turn("turn-1", "read", &mut sink).unwrap();

        assert_eq!(result.text, "final answer");
        assert_eq!(result.usage.input_tokens, Some(10));
        assert_eq!(result.usage.output_tokens, Some(5));
        assert_eq!(
            sink.deltas,
            vec!["working".to_string(), "final answer".into()]
        );
        assert_eq!(sink.tool_calls.len(), 1);
        assert!(sink.tool_results[0].ok);
        assert!(sink.tool_results[0].content.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("hello readme"));
        let records = load_committed_turn_records(session.transcript_path()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].messages.len(), 4);
        let assistant_tools = &records[0].messages[1];
        assert_eq!(assistant_tools.role, ConversationRole::Assistant);
        assert_eq!(assistant_tools.content.as_deref(), Some("working"));
        assert_eq!(assistant_tools.tool_calls.len(), 1);
        let tool_id = assistant_tools.tool_calls[0].id.clone();
        assert!(tool_id.starts_with("turn-1:r0:"));
        assert!(assistant_tools.tool_calls[0].arguments.is_object());
        assert_eq!(
            records[0].messages[2].tool_call_id.as_deref(),
            Some(tool_id.as_str())
        );
        assert_eq!(
            records[0].messages[3].content.as_deref(),
            Some("final answer")
        );

        let requests = cloned.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        assert!(second
            .messages
            .iter()
            .any(|message| { message.role == "assistant" && !message.tool_calls.is_empty() }));
        assert!(second.messages.iter().any(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some(tool_id.as_str())
        }));
        assert_eq!(second.messages[0].role, "system");
        assert!(second.messages[0]
            .content
            .as_ref()
            .unwrap()
            .contains("[Pedelec Host Context]"));
    }

    #[test]
    fn ordinary_tool_error_is_fed_back_to_model() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![
                tool_round(
                    "fs.read_text_file",
                    json!({"path": "missing.md"}),
                    InferenceUsage::from_token_counts(Some(1), Some(1)),
                ),
                terminal("sorry", InferenceUsage::from_token_counts(Some(1), Some(1))),
            ],
        );
        let mut session = open_session(&temp, backend, None);
        let mut sink = RecordingSink::default();
        session.run_turn("turn-1", "read", &mut sink).unwrap();
        assert!(!sink.tool_results[0].ok);
        assert_eq!(
            sink.tool_results[0].error.as_ref().unwrap().code,
            "FILE_NOT_FOUND"
        );
        let records = load_committed_turn_records(session.transcript_path()).unwrap();
        assert!(records[0].messages[2]
            .content
            .as_ref()
            .unwrap()
            .contains("FILE_NOT_FOUND"));
    }

    #[test]
    fn max_tool_rounds_failure_discards_active_turn() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello").unwrap();
        let mut steps = Vec::new();
        for _ in 0..=8 {
            steps.push(tool_round(
                "fs.read_text_file",
                json!({"path": "README.md"}),
                InferenceUsage::from_token_counts(Some(1), Some(1)),
            ));
        }
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            steps,
        );
        let mut session = open_session(&temp, backend, None);
        let err = session
            .run_turn("turn-1", "read", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "MAX_TOOL_ROUNDS_EXCEEDED");
        assert!(session.committed_messages().is_empty());
        assert_eq!(
            load_committed_turn_records(session.transcript_path())
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn failed_backend_turn_writes_no_committed_record() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![ScriptedStep {
                deltas: vec![],
                result: Err(AgentError::new("OLLAMA_UNAVAILABLE", "down")),
                on_infer: None,
            }],
        );
        let mut session = open_session(&temp, backend, None);
        let err = session
            .run_turn("turn-1", "hello", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "OLLAMA_UNAVAILABLE");
        assert!(session.committed_messages().is_empty());
        assert_eq!(
            std::fs::read_to_string(session.transcript_path())
                .unwrap()
                .trim(),
            ""
        );
    }

    #[test]
    fn durable_commit_failure_does_not_report_terminal_success() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![terminal(
                "final",
                InferenceUsage::from_token_counts(Some(1), Some(1)),
            )],
        );
        let mut session = open_session(&temp, backend, None);
        let transcript = session.transcript_path().to_path_buf();
        std::fs::remove_file(&transcript).unwrap();
        std::fs::create_dir(&transcript).unwrap();
        let err = session
            .run_turn("turn-1", "hello", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_COMMIT_FAILED");
        assert!(session.committed_messages().is_empty());
    }

    #[test]
    fn live_token_invalidation_during_infer_skips_commit() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![terminal(
                "final",
                InferenceUsage::from_token_counts(Some(1), Some(1)),
            )],
        );
        let mut session = open_session(
            &temp,
            Arc::clone(&backend) as Arc<dyn InferenceBackend>,
            None,
        );
        let token = session.live_token();
        backend.steps.lock().unwrap().clear();
        backend.steps.lock().unwrap().push(ScriptedStep {
            deltas: vec!["final".into()],
            result: Ok(InferenceResult {
                text: Some("final".into()),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: Some("stop".into()),
            }),
            on_infer: Some(Arc::new(move || {
                token.store(false, Ordering::SeqCst);
            })),
        });
        let err = session
            .run_turn("turn-1", "hello", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "TURN_INVALIDATED");
        assert!(session.committed_messages().is_empty());
    }

    #[test]
    fn resume_rebuilds_committed_conversation_without_active_turn() {
        let temp = tempfile::tempdir().unwrap();
        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![terminal(
                "first",
                InferenceUsage::from_token_counts(Some(1), Some(1)),
            )],
        );
        let mut session = open_session(&temp, backend, None);
        session
            .run_turn("turn-1", "hello", &mut IgnoringTurnSink)
            .unwrap();
        let session_id = session.session_id().to_string();
        drop(session);

        let backend = ScriptedBackend::new(
            ModelCapabilities {
                tools: true,
                vision: false,
            },
            vec![],
        );
        let resumed = open_session(&temp, backend, Some(session_id));
        assert!(resumed.resumed());
        assert_eq!(resumed.committed_messages().len(), 2);
        assert!(resumed.active_turn.is_none());
    }

    #[test]
    fn same_session_rejects_parallel_turns() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = open_session(
            &temp,
            ScriptedBackend::new(
                ModelCapabilities {
                    tools: true,
                    vision: false,
                },
                vec![],
            ),
            None,
        );
        session.force_running("turn-1");
        let err = session
            .run_turn("turn-2", "hello", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "TURN_IN_PROGRESS");
    }

    #[test]
    fn closed_session_rejects_new_turns() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = open_session(
            &temp,
            ScriptedBackend::new(
                ModelCapabilities {
                    tools: true,
                    vision: false,
                },
                vec![],
            ),
            None,
        );
        session.close();
        let err = session
            .run_turn("turn-1", "hello", &mut IgnoringTurnSink)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_CLOSED");
    }
}
