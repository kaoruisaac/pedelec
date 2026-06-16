use super::cli::parse_args;
use super::config::resolve_config;
use super::error::AgentError;
use super::jsonl::{AgentEvent, JsonlWriter};
use super::model::{adapter_for, ModelMessage, ModelToolCall};
use super::sandbox::Sandbox;
use super::session::{
    append_transcript, load_or_create_session, load_transcript, touch_session, TranscriptMessage,
};
use super::tools::{execute_tool, tool_definitions};
use serde_json::Value;

const SYSTEM_PROMPT: &str = "You are pedelec-agent, a lightweight read-only assistant.\n\n\
You can:\n\
- Read text files inside the provided sandbox.\n\
- Search the web using the web.search tool.\n\
- Call Pedelec host app tools using the pedelec_cli.tool_call tool.\n\n\
You cannot:\n\
- Write files.\n\
- Delete files.\n\
- Execute arbitrary shell commands.\n\
- Access files outside the sandbox.\n\n\
When you need file content, call fs.read_text_file.\n\
When you need to discover available files, call fs.list_text_files.\n\
When you need current information, call web.search.\n\
When you need host app state or actions, call pedelec_cli.tool_call.\n\n\
Do not claim you modified files.\n\
Do not invent file contents.\n\
Do not invent web search results.";

pub fn run() -> i32 {
    match run_inner(std::env::args().collect()) {
        Ok(()) => 0,
        Err(err) => {
            let event = AgentEvent::Error { error: err };
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
            1
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), AgentError> {
    let cli = parse_args(args)?;
    let config = resolve_config(&cli)?;
    let sandbox = Sandbox::new(
        &config.sandbox,
        config.max_file_bytes,
        config.max_list_files,
    )?;
    let mut session = load_or_create_session(&cli.session_id, &config, sandbox.root())?;
    let writer = JsonlWriter::new(session.events_path.clone());
    writer.emit(&AgentEvent::Session {
        session_id: session.metadata.session_id.clone(),
        resumed: session.resumed,
    })?;
    writer.emit(&AgentEvent::Status {
        status: "running".into(),
    })?;

    let mut transcript = load_transcript(&session)?;
    let user_message = TranscriptMessage {
        role: "user".into(),
        name: None,
        content: Value::String(cli.prompt.clone()),
    };
    append_transcript(&session, &user_message)?;
    transcript.push(user_message);

    let adapter = adapter_for(&config)?;
    let tools = tool_definitions();
    let mut messages = build_model_messages(&transcript);
    let mut final_text = None;

    for round in 0..=config.max_tool_rounds {
        let output = adapter.run_turn(&messages, &tools)?;
        if let Some(text) = output.text.clone() {
            final_text = Some(text.clone());
            let assistant = TranscriptMessage {
                role: "assistant".into(),
                name: None,
                content: Value::String(text),
            };
            append_transcript(&session, &assistant)?;
            messages.push(transcript_message_to_model(&assistant));
        }

        if output.tool_calls.is_empty() {
            break;
        }
        if round >= config.max_tool_rounds {
            return Err(AgentError::new(
                "MAX_TOOL_ROUNDS_EXCEEDED",
                "The agent exceeded max tool rounds.",
            ));
        }

        messages.push(assistant_tool_call_message(&output.tool_calls));
        for call in output.tool_calls {
            let tool = call.function.name;
            let args = call.function.arguments;
            writer.emit(&AgentEvent::ToolCall {
                tool: tool.clone(),
                args: args.clone(),
            })?;
            match execute_tool(
                &tool,
                &args,
                &session.metadata.session_id,
                &sandbox,
                &config,
            ) {
                Ok(result) => {
                    writer.emit(&AgentEvent::ToolResult {
                        tool: tool.clone(),
                        ok: true,
                        result: Some(result.clone()),
                        error: None,
                    })?;
                    let tool_message = TranscriptMessage {
                        role: "tool".into(),
                        name: Some(tool.clone()),
                        content: result.clone(),
                    };
                    append_transcript(&session, &tool_message)?;
                    messages.push(ModelMessage {
                        role: "tool".into(),
                        content: Some(result.to_string()),
                        tool_calls: None,
                    });
                }
                Err(error) => {
                    writer.emit(&AgentEvent::ToolResult {
                        tool: tool.clone(),
                        ok: false,
                        result: None,
                        error: Some(error.clone()),
                    })?;
                    let tool_message = TranscriptMessage {
                        role: "tool".into(),
                        name: Some(tool.clone()),
                        content: serde_json::json!({ "error": error }),
                    };
                    append_transcript(&session, &tool_message)?;
                    messages.push(ModelMessage {
                        role: "tool".into(),
                        content: Some(tool_message.content.to_string()),
                        tool_calls: None,
                    });
                }
            }
        }
    }

    if let Some(text) = final_text {
        writer.emit(&AgentEvent::AssistantMessage { text })?;
    }
    touch_session(&mut session)?;
    writer.emit(&AgentEvent::Status {
        status: "done".into(),
    })?;
    writer.emit(&AgentEvent::Done {})?;
    Ok(())
}

fn build_model_messages(transcript: &[TranscriptMessage]) -> Vec<ModelMessage> {
    let mut messages = vec![ModelMessage {
        role: "system".into(),
        content: Some(SYSTEM_PROMPT.into()),
        tool_calls: None,
    }];
    messages.extend(transcript.iter().map(transcript_message_to_model));
    messages
}

fn transcript_message_to_model(message: &TranscriptMessage) -> ModelMessage {
    ModelMessage {
        role: message.role.clone(),
        content: Some(match &message.content {
            Value::String(text) => text.clone(),
            value => value.to_string(),
        }),
        tool_calls: None,
    }
}

fn assistant_tool_call_message(calls: &[ModelToolCall]) -> ModelMessage {
    ModelMessage {
        role: "assistant".into(),
        content: Some(String::new()),
        tool_calls: Some(calls.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn exits_with_jsonl_for_missing_model() {
        let temp = tempfile::tempdir().unwrap();
        let code = run_inner(vec![
            "pedelec-agent".into(),
            "run".into(),
            "s".into(),
            "hello".into(),
            "--env-file".into(),
            temp.path()
                .join("missing.env")
                .to_string_lossy()
                .to_string(),
        ]);

        assert!(code.is_err());
    }

    #[test]
    fn fake_ollama_tool_round_creates_session_and_transcript() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "hello readme").unwrap();
        let (base_url, handle) = fake_ollama_server();
        let env_file = temp.path().join(".env.local");
        std::fs::write(
            &env_file,
            format!(
                "PEDELEC_AGENT_MODEL=fake\nOLLAMA_BASE_URL={base_url}\nPEDELEC_AGENT_HOME={}\n",
                temp.path().join(".pedelec-agent").to_string_lossy()
            ),
        )
        .unwrap();

        run_inner(vec![
            "pedelec-agent".into(),
            "run".into(),
            "s1".into(),
            "read".into(),
            "--sandbox".into(),
            temp.path().to_string_lossy().to_string(),
            "--env-file".into(),
            env_file.to_string_lossy().to_string(),
        ])
        .unwrap();
        handle.join().unwrap();

        assert!(temp
            .path()
            .join(".pedelec-agent/sessions/s1/session.json")
            .exists());
        let transcript = std::fs::read_to_string(
            temp.path()
                .join(".pedelec-agent/sessions/s1/transcript.jsonl"),
        )
        .unwrap();
        assert!(transcript.contains("hello readme"));
        assert!(transcript.contains("final answer"));
    }

    fn fake_ollama_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0; 8192];
                let _ = stream.read(&mut buffer).unwrap();
                let body = if index == 0 {
                    r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read_text_file","arguments":{"path":"README.md"}}}]}}"#
                } else {
                    r#"{"message":{"role":"assistant","content":"final answer"}}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        (format!("http://{addr}"), handle)
    }
}
