use super::error::AgentError;
use serde::Serialize;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    Session {
        session_id: String,
        resumed: bool,
    },
    Status {
        status: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCall {
        tool: String,
        args: Value,
    },
    ToolResult {
        tool: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<AgentError>,
    },
    Error {
        error: AgentError,
    },
    Done {},
}

pub struct JsonlWriter {
    event_log_path: PathBuf,
}

impl JsonlWriter {
    pub fn new(event_log_path: impl Into<PathBuf>) -> Self {
        Self {
            event_log_path: event_log_path.into(),
        }
    }

    pub fn emit(&self, event: &AgentEvent) -> Result<(), AgentError> {
        let line = serde_json::to_string(event).map_err(|err| {
            AgentError::with_details(
                "JSONL_WRITE_FAILED",
                "Failed to serialize JSONL event",
                serde_json::json!({ "error": err.to_string() }),
            )
        })?;
        println!("{line}");
        io::stdout().flush().map_err(AgentError::from)?;
        append_line(&self.event_log_path, &line)
    }
}

pub fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), AgentError> {
    let line = serde_json::to_string(value).map_err(|err| {
        AgentError::with_details(
            "JSONL_WRITE_FAILED",
            "Failed to serialize JSONL record",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    append_line(path, &line)
}

fn append_line(path: &Path, line: &str) -> Result<(), AgentError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_as_single_json_object() {
        let event = AgentEvent::Status {
            status: "running".into(),
        };
        let line = serde_json::to_string(&event).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();

        assert!(parsed.is_object());
        assert!(!line.contains('\n'));
    }

    #[test]
    fn session_event_uses_camel_case_fields() {
        let event = AgentEvent::Session {
            session_id: "s1".into(),
            resumed: true,
        };
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["sessionId"], "s1");
        assert!(value.get("session_id").is_none());
    }
}
