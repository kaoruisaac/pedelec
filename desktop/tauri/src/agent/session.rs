use super::config::AgentConfig;
use super::error::AgentError;
use super::jsonl::append_jsonl;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub sandbox_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct SessionState {
    pub metadata: SessionMetadata,
    pub resumed: bool,
    pub dir: PathBuf,
    pub transcript_path: PathBuf,
    pub events_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Value,
}

pub fn load_or_create_session(
    session_id: &str,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    validate_session_id(session_id)?;
    let session_dir = config.home.join("sessions").join(session_id);
    let session_path = session_dir.join("session.json");
    let transcript_path = session_dir.join("transcript.jsonl");
    let events_path = session_dir.join("events.jsonl");

    if session_path.exists() {
        let content = fs::read_to_string(&session_path).map_err(|err| {
            AgentError::with_details(
                "SESSION_LOAD_FAILED",
                "Failed to load session metadata",
                serde_json::json!({ "path": session_path, "error": err.to_string() }),
            )
        })?;
        let metadata = serde_json::from_str::<SessionMetadata>(&content).map_err(|err| {
            AgentError::with_details(
                "SESSION_LOAD_FAILED",
                "Failed to parse session metadata",
                serde_json::json!({ "path": session_path, "error": err.to_string() }),
            )
        })?;
        reject_resume_conflicts(&metadata, config, sandbox_path)?;
        enforce_transcript_size(&transcript_path, config.max_transcript_bytes)?;
        return Ok(SessionState {
            metadata,
            resumed: true,
            dir: session_dir,
            transcript_path,
            events_path,
        });
    }

    fs::create_dir_all(&session_dir).map_err(|err| {
        AgentError::with_details(
            "SESSION_SAVE_FAILED",
            "Failed to create session directory",
            serde_json::json!({ "path": session_dir, "error": err.to_string() }),
        )
    })?;
    let now = Utc::now();
    let metadata = SessionMetadata {
        session_id: session_id.into(),
        provider: config.provider_name.clone(),
        model: config.model.clone(),
        sandbox_path: sandbox_path.to_path_buf(),
        created_at: now,
        updated_at: now,
    };
    save_session_metadata(&session_path, &metadata)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&transcript_path)
        .map_err(|err| {
            AgentError::with_details(
                "SESSION_SAVE_FAILED",
                "Failed to create transcript",
                serde_json::json!({ "path": transcript_path, "error": err.to_string() }),
            )
        })?;

    Ok(SessionState {
        metadata,
        resumed: false,
        dir: session_dir,
        transcript_path,
        events_path,
    })
}

pub fn append_transcript(
    session: &SessionState,
    message: &TranscriptMessage,
) -> Result<(), AgentError> {
    append_jsonl(&session.transcript_path, message)
}

pub fn load_transcript(session: &SessionState) -> Result<Vec<TranscriptMessage>, AgentError> {
    if !session.transcript_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&session.transcript_path).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to load transcript",
            serde_json::json!({ "path": session.transcript_path, "error": err.to_string() }),
        )
    })?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<TranscriptMessage>(line).map_err(|err| {
                AgentError::with_details(
                    "SESSION_LOAD_FAILED",
                    "Failed to parse transcript line",
                    serde_json::json!({ "error": err.to_string() }),
                )
            })
        })
        .collect()
}

pub fn touch_session(session: &mut SessionState) -> Result<(), AgentError> {
    session.metadata.updated_at = Utc::now();
    save_session_metadata(&session.dir.join("session.json"), &session.metadata)
}

fn save_session_metadata(path: &Path, metadata: &SessionMetadata) -> Result<(), AgentError> {
    let content = serde_json::to_string_pretty(metadata).map_err(|err| {
        AgentError::with_details(
            "SESSION_SAVE_FAILED",
            "Failed to serialize session metadata",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(path, content).map_err(|err| {
        AgentError::with_details(
            "SESSION_SAVE_FAILED",
            "Failed to save session metadata",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })
}

fn reject_resume_conflicts(
    metadata: &SessionMetadata,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<(), AgentError> {
    if metadata.provider != config.provider_name {
        return Err(conflict(
            "provider",
            &metadata.provider,
            &config.provider_name,
        ));
    }
    if metadata.model != config.model {
        return Err(conflict("model", &metadata.model, &config.model));
    }
    if metadata.sandbox_path != sandbox_path {
        return Err(conflict(
            "sandboxPath",
            &metadata.sandbox_path.to_string_lossy(),
            &sandbox_path.to_string_lossy(),
        ));
    }
    Ok(())
}

fn conflict(field: &str, existing: &str, requested: &str) -> AgentError {
    AgentError::with_details(
        "INVALID_ARGUMENT",
        "Session resume argument conflicts with existing session",
        serde_json::json!({ "field": field, "existing": existing, "requested": requested }),
    )
}

fn enforce_transcript_size(path: &Path, max_bytes: u64) -> Result<(), AgentError> {
    if path.exists() {
        let size = fs::metadata(path)?.len();
        if size > max_bytes {
            return Err(AgentError::with_details(
                "TRANSCRIPT_TOO_LARGE",
                "Transcript exceeds maximum configured size",
                serde_json::json!({ "path": path, "sizeBytes": size, "maxBytes": max_bytes }),
            ));
        }
    }
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<(), AgentError> {
    if session_id.trim().is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
    {
        return Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Invalid session id",
            serde_json::json!({ "sessionId": session_id }),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{AgentConfig, ModelProvider};

    fn config(home: PathBuf, sandbox: PathBuf) -> AgentConfig {
        AgentConfig {
            provider: ModelProvider::Ollama,
            provider_name: "ollama".into(),
            model: "fake".into(),
            ollama_base_url: "http://127.0.0.1:1".into(),
            ollama_timeout_ms: 1000,
            home,
            sandbox,
            web_search_provider: None,
            web_search_timeout_ms: 1000,
            web_search_max_results: 5,
            brave_search_api_key: None,
            pedelec_cli_path: None,
            core_runtime_file: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    #[test]
    fn creates_and_resumes_session() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(temp.path().join("home"), sandbox.clone());

        let first = load_or_create_session("s1", &cfg, &sandbox).unwrap();
        let second = load_or_create_session("s1", &cfg, &sandbox).unwrap();

        assert!(!first.resumed);
        assert!(second.resumed);
    }

    #[test]
    fn rejects_conflicting_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(temp.path().join("home"), sandbox.clone());
        load_or_create_session("s1", &cfg, &sandbox).unwrap();

        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let err = load_or_create_session("s1", &cfg, &other.canonicalize().unwrap()).unwrap_err();

        assert_eq!(err.code, "INVALID_ARGUMENT");
    }
}
