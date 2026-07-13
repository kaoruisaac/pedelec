use super::config::AgentConfig;
use super::error::AgentError;
use super::jsonl::append_jsonl;
use crate::pedelec_paths::pedelec_home_dir;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, DirBuilder};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use uuid::{Uuid, Version};

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

pub fn create_session(
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    let agent_home = agent_home_dir()?;
    create_session_at(&agent_home, config, sandbox_path)
}

pub fn load_session(
    session_id: &str,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    let agent_home = agent_home_dir()?;
    load_session_at(&agent_home, session_id, config, sandbox_path)
}

fn agent_home_dir() -> Result<PathBuf, AgentError> {
    pedelec_home_dir()
        .map(|home| home.join("pedelec-agent"))
        .map_err(|err| AgentError {
            code: err.code,
            message: err.message,
            details: err.details,
        })
}

pub(crate) fn create_session_at(
    agent_home: &Path,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    for _ in 0..16 {
        let uuid = Uuid::now_v7();
        let session_id = uuid.hyphenated().to_string();
        let (year, month) = uuid_year_month(&uuid, &session_id)?;
        let session_dir = session_dir_for_parts(agent_home, year, month, &session_id);
        match DirBuilder::new().recursive(false).create(&session_dir) {
            Ok(()) => {
                return initialize_new_session(session_dir, session_id, config, sandbox_path);
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if let Some(parent) = session_dir.parent() {
                    fs::create_dir_all(parent).map_err(|err| {
                        AgentError::with_details(
                            "SESSION_SAVE_FAILED",
                            "Failed to create session parent directory",
                            serde_json::json!({ "path": parent, "error": err.to_string() }),
                        )
                    })?;
                }
                match DirBuilder::new().recursive(false).create(&session_dir) {
                    Ok(()) => {
                        return initialize_new_session(
                            session_dir,
                            session_id,
                            config,
                            sandbox_path,
                        );
                    }
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                    Err(err) => {
                        return Err(AgentError::with_details(
                            "SESSION_SAVE_FAILED",
                            "Failed to create session directory",
                            serde_json::json!({ "path": session_dir, "error": err.to_string() }),
                        ));
                    }
                }
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(AgentError::with_details(
                    "SESSION_SAVE_FAILED",
                    "Failed to create session directory",
                    serde_json::json!({ "path": session_dir, "error": err.to_string() }),
                ));
            }
        }
    }

    Err(AgentError::new(
        "SESSION_SAVE_FAILED",
        "Failed to allocate a unique session id",
    ))
}

pub(crate) fn load_session_at(
    agent_home: &Path,
    session_id: &str,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    let uuid = parse_uuid_v7(session_id)?;
    let (year, month) = uuid_year_month(&uuid, session_id)?;
    let session_dir = session_dir_for_parts(agent_home, year, month, session_id);
    let session_path = session_dir.join("session.json");
    let transcript_path = session_dir.join("transcript.jsonl");
    let events_path = session_dir.join("events.jsonl");

    if !session_dir.exists() || !session_path.exists() {
        return Err(AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Session was not found",
            serde_json::json!({ "sessionId": session_id, "path": session_dir }),
        ));
    }

    let content = fs::read_to_string(&session_path).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to load session metadata",
            serde_json::json!({
                "sessionId": session_id,
                "path": session_path,
                "error": err.to_string()
            }),
        )
    })?;
    let metadata = serde_json::from_str::<SessionMetadata>(&content).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to parse session metadata",
            serde_json::json!({
                "sessionId": session_id,
                "path": session_path,
                "error": err.to_string()
            }),
        )
    })?;
    if metadata.session_id != session_id {
        return Err(AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Session metadata id does not match requested session id",
            serde_json::json!({
                "sessionId": session_id,
                "path": session_path,
                "metadataSessionId": metadata.session_id
            }),
        ));
    }
    reject_resume_conflicts(&metadata, config, sandbox_path)?;
    enforce_transcript_size(&transcript_path, config.max_transcript_bytes)?;

    Ok(SessionState {
        metadata,
        resumed: true,
        dir: session_dir,
        transcript_path,
        events_path,
    })
}

fn initialize_new_session(
    session_dir: PathBuf,
    session_id: String,
    config: &AgentConfig,
    sandbox_path: &Path,
) -> Result<SessionState, AgentError> {
    let session_path = session_dir.join("session.json");
    let transcript_path = session_dir.join("transcript.jsonl");
    let events_path = session_dir.join("events.jsonl");
    let now = Utc::now();
    let metadata = SessionMetadata {
        session_id,
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

fn parse_uuid_v7(session_id: &str) -> Result<Uuid, AgentError> {
    let uuid = Uuid::parse_str(session_id).map_err(|err| {
        AgentError::with_details(
            "INVALID_ARGUMENT",
            "Invalid session id",
            serde_json::json!({ "sessionId": session_id, "error": err.to_string() }),
        )
    })?;
    if uuid.get_version() != Some(Version::SortRand) {
        return Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Session id must be a UUID v7",
            serde_json::json!({ "sessionId": session_id }),
        ));
    }
    Ok(uuid)
}

fn uuid_year_month(uuid: &Uuid, session_id: &str) -> Result<(i32, u32), AgentError> {
    let timestamp = uuid.get_timestamp().ok_or_else(|| {
        AgentError::with_details(
            "INVALID_ARGUMENT",
            "Session id does not contain a UUID v7 timestamp",
            serde_json::json!({ "sessionId": session_id }),
        )
    })?;
    let (seconds, nanos) = timestamp.to_unix();
    let datetime = Utc
        .timestamp_opt(seconds as i64, nanos)
        .single()
        .ok_or_else(|| {
            AgentError::with_details(
                "INVALID_ARGUMENT",
                "Session id timestamp is out of range",
                serde_json::json!({ "sessionId": session_id }),
            )
        })?;
    Ok((datetime.year(), datetime.month()))
}

fn session_dir_for_parts(agent_home: &Path, year: i32, month: u32, session_id: &str) -> PathBuf {
    agent_home
        .join("sessions")
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{AgentConfig, ModelProvider};

    fn config(sandbox: PathBuf) -> AgentConfig {
        AgentConfig {
            provider: ModelProvider::Ollama,
            provider_name: "ollama".into(),
            model: "fake".into(),
            ollama_base_url: "http://127.0.0.1:1".into(),
            ollama_timeout_ms: 1000,
            ollama_api_key: "ollama".into(),
            sandbox,
            pedelec_cli_path: None,
            core_runtime_file: None,
            max_transcript_bytes: 1024,
            max_tool_rounds: 8,
            max_list_files: 200,
            max_file_bytes: 1024,
            max_image_bytes: 20 * 1024 * 1024,
            pedelec_cli_timeout_ms: 1000,
        }
    }

    #[test]
    fn creates_and_resumes_session() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(sandbox.clone());
        let home = temp.path().join("home");

        let first = create_session_at(&home, &cfg, &sandbox).unwrap();
        let second = load_session_at(&home, &first.metadata.session_id, &cfg, &sandbox).unwrap();

        assert!(!first.resumed);
        assert!(second.resumed);
        assert_eq!(first.metadata.session_id, second.metadata.session_id);
    }

    #[test]
    fn rejects_conflicting_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(sandbox.clone());
        let home = temp.path().join("home");
        let session = create_session_at(&home, &cfg, &sandbox).unwrap();

        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let err = load_session_at(
            &home,
            &session.metadata.session_id,
            &cfg,
            &other.canonicalize().unwrap(),
        )
        .unwrap_err();

        assert_eq!(err.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn create_session_generates_uuid_v7_and_layered_path() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(sandbox.clone());
        let home = temp.path().join("home");

        let first = create_session_at(&home, &cfg, &sandbox).unwrap();
        let second = create_session_at(&home, &cfg, &sandbox).unwrap();
        let uuid = Uuid::parse_str(&first.metadata.session_id).unwrap();
        let (year, month) = uuid_year_month(&uuid, &first.metadata.session_id).unwrap();

        assert_eq!(uuid.get_version(), Some(Version::SortRand));
        assert_eq!(first.metadata.session_id, uuid.hyphenated().to_string());
        assert_ne!(first.metadata.session_id, second.metadata.session_id);
        assert_eq!(
            first.dir,
            home.join("sessions")
                .join(format!("{year:04}"))
                .join(format!("{month:02}"))
                .join(&first.metadata.session_id)
        );
        assert_eq!(
            fs::read_to_string(first.dir.join("session.json"))
                .unwrap()
                .contains(&first.metadata.session_id),
            true
        );
    }

    #[test]
    fn load_session_rejects_uuid_v4_and_missing_v7_session() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(sandbox.clone());
        let home = temp.path().join("home");

        let err = load_session_at(
            &home,
            "123e4567-e89b-42d3-a456-426614174000",
            &cfg,
            &sandbox,
        )
        .unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");

        let missing = Uuid::now_v7().hyphenated().to_string();
        let err = load_session_at(&home, &missing, &cfg, &sandbox).unwrap_err();
        assert_eq!(err.code, "SESSION_LOAD_FAILED");
    }

    #[test]
    fn load_session_rejects_metadata_session_id_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().canonicalize().unwrap();
        let cfg = config(sandbox.clone());
        let home = temp.path().join("home");
        let session = create_session_at(&home, &cfg, &sandbox).unwrap();
        let mut metadata = session.metadata.clone();
        metadata.session_id = Uuid::now_v7().hyphenated().to_string();
        save_session_metadata(&session.dir.join("session.json"), &metadata).unwrap();

        let err = load_session_at(&home, &session.metadata.session_id, &cfg, &sandbox).unwrap_err();

        assert_eq!(err.code, "SESSION_LOAD_FAILED");
    }
}
