use super::conversation::{CommittedTurnRecord, ConversationMessage, SESSION_SCHEMA_VERSION};
use super::error::AgentError;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use pedelec_shared::paths::pedelec_home_dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use uuid::{Uuid, Version};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub workspace_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub metadata: SessionMetadata,
    pub resumed: bool,
    pub dir: PathBuf,
    pub transcript_path: PathBuf,
}

pub fn default_agent_home_dir() -> Result<PathBuf, AgentError> {
    pedelec_home_dir()
        .map(|home| agent_home_dir_from_pedelec_home(&home))
        .map_err(|err| AgentError {
            code: err.code,
            message: err.message,
            details: err.details,
        })
}

pub fn agent_home_dir_from_pedelec_home(pedelec_home: &Path) -> PathBuf {
    pedelec_home.join("agent")
}

pub fn create_session_store(
    agent_home: &Path,
    provider: &str,
    model: &str,
    workspace_path: &Path,
) -> Result<SessionStore, AgentError> {
    for _ in 0..16 {
        let uuid = Uuid::now_v7();
        let session_id = uuid.hyphenated().to_string();
        let (year, month) = uuid_year_month(&uuid, &session_id)?;
        let session_dir = session_dir_for_parts(agent_home, year, month, &session_id);
        match DirBuilder::new().recursive(false).create(&session_dir) {
            Ok(()) => {
                return initialize_new_session(
                    session_dir,
                    session_id,
                    provider,
                    model,
                    workspace_path,
                );
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
                            provider,
                            model,
                            workspace_path,
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

pub fn load_session_store(
    agent_home: &Path,
    session_id: &str,
    provider: &str,
    model: &str,
    workspace_path: &Path,
    max_transcript_bytes: u64,
) -> Result<(SessionStore, Vec<ConversationMessage>), AgentError> {
    let uuid = parse_uuid_v7(session_id)?;
    let (year, month) = uuid_year_month(&uuid, session_id)?;
    let session_dir = session_dir_for_parts(agent_home, year, month, session_id);
    let session_path = session_dir.join("session.json");
    let transcript_path = session_dir.join("transcript.jsonl");

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
    let metadata = parse_session_metadata(&content, &session_path)?;
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
    reject_resume_conflicts(&metadata, provider, model, workspace_path)?;
    enforce_transcript_size(&transcript_path, max_transcript_bytes)?;
    let committed = load_committed_conversation(&transcript_path)?;

    Ok((
        SessionStore {
            metadata,
            resumed: true,
            dir: session_dir,
            transcript_path,
        },
        committed,
    ))
}

fn initialize_new_session(
    session_dir: PathBuf,
    session_id: String,
    provider: &str,
    model: &str,
    workspace_path: &Path,
) -> Result<SessionStore, AgentError> {
    let session_path = session_dir.join("session.json");
    let transcript_path = session_dir.join("transcript.jsonl");
    let now = Utc::now();
    let metadata = SessionMetadata {
        schema_version: SESSION_SCHEMA_VERSION,
        session_id,
        provider: provider.to_string(),
        model: model.to_string(),
        workspace_path: workspace_path.to_path_buf(),
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

    Ok(SessionStore {
        metadata,
        resumed: false,
        dir: session_dir,
        transcript_path,
    })
}

impl SessionStore {
    pub fn append_committed_turn(
        &self,
        record: &CommittedTurnRecord,
        max_transcript_bytes: u64,
    ) -> Result<(), AgentError> {
        record.validate_schema()?;
        enforce_transcript_size(&self.transcript_path, max_transcript_bytes)?;
        append_jsonl(&self.transcript_path, record)
    }

    pub fn touch(&mut self) -> Result<(), AgentError> {
        self.metadata.updated_at = Utc::now();
        save_session_metadata(&self.dir.join("session.json"), &self.metadata)
    }
}

pub fn parse_session_metadata(content: &str, path: &Path) -> Result<SessionMetadata, AgentError> {
    let value = serde_json::from_str::<Value>(content).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to parse session metadata",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })?;
    let version = value.get("schemaVersion").and_then(Value::as_u64);
    if version != Some(SESSION_SCHEMA_VERSION as u64) {
        return Err(AgentError::with_details(
            "SESSION_SCHEMA_INCOMPATIBLE",
            "Persisted session schema is not supported.",
            serde_json::json!({
                "schemaVersion": version,
                "supported": SESSION_SCHEMA_VERSION,
                "path": path
            }),
        ));
    }
    serde_json::from_value(value).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to parse session metadata",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })
}

pub fn load_committed_conversation(
    transcript_path: &Path,
) -> Result<Vec<ConversationMessage>, AgentError> {
    let records = load_committed_turn_records(transcript_path)?;
    Ok(records
        .into_iter()
        .flat_map(|record| record.messages)
        .collect())
}

pub fn load_committed_turn_records(
    transcript_path: &Path,
) -> Result<Vec<CommittedTurnRecord>, AgentError> {
    if !transcript_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(transcript_path).map_err(|err| {
        AgentError::with_details(
            "SESSION_LOAD_FAILED",
            "Failed to load transcript",
            serde_json::json!({ "path": transcript_path, "error": err.to_string() }),
        )
    })?;
    let lines = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let mut records = Vec::new();
    for (index, (line_no, line)) in lines.iter().enumerate() {
        let is_last = index + 1 == lines.len();
        match parse_committed_turn_line(line) {
            Ok(record) => {
                record.validate_schema()?;
                records.push(record);
            }
            Err(err) if is_last && is_trailing_crash_debris(line, &err) => break,
            Err(err) => {
                return Err(AgentError::with_details(
                    "SESSION_CORRUPT",
                    "Persisted transcript contains a malformed committed turn.",
                    serde_json::json!({
                        "path": transcript_path,
                        "line": line_no + 1,
                        "error": err.message
                    }),
                ));
            }
        }
    }
    Ok(records)
}

struct TurnLineError {
    code: &'static str,
    message: String,
}

fn parse_committed_turn_line(line: &str) -> Result<CommittedTurnRecord, TurnLineError> {
    let value = serde_json::from_str::<Value>(line).map_err(|err| TurnLineError {
        code: "malformed",
        message: err.to_string(),
    })?;
    let version = value.get("schemaVersion").and_then(Value::as_u64);
    if version != Some(SESSION_SCHEMA_VERSION as u64) {
        return Err(TurnLineError {
            code: "incompatible",
            message: format!("unsupported turn schema version: {version:?}"),
        });
    }
    serde_json::from_value(value).map_err(|err| TurnLineError {
        code: "malformed",
        message: err.to_string(),
    })
}

fn is_trailing_crash_debris(line: &str, err: &TurnLineError) -> bool {
    err.code == "malformed" && (!line.trim().ends_with('}') || !line.trim().starts_with('{'))
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), AgentError> {
    let line = serde_json::to_string(value).map_err(|err| {
        AgentError::with_details(
            "SESSION_COMMIT_FAILED",
            "Failed to serialize committed turn",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| {
            AgentError::with_details(
                "SESSION_COMMIT_FAILED",
                "Failed to open transcript for commit",
                serde_json::json!({ "path": path, "error": err.to_string() }),
            )
        })?;
    writeln!(file, "{line}").map_err(|err| {
        AgentError::with_details(
            "SESSION_COMMIT_FAILED",
            "Failed to append committed turn",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })?;
    file.flush().map_err(|err| {
        AgentError::with_details(
            "SESSION_COMMIT_FAILED",
            "Failed to flush committed turn",
            serde_json::json!({ "path": path, "error": err.to_string() }),
        )
    })
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
    provider: &str,
    model: &str,
    workspace_path: &Path,
) -> Result<(), AgentError> {
    if metadata.provider != provider {
        return Err(conflict("provider", &metadata.provider, provider));
    }
    if metadata.model != model {
        return Err(conflict("model", &metadata.model, model));
    }
    if metadata.workspace_path != workspace_path {
        return Err(conflict(
            "workspacePath",
            &metadata.workspace_path.to_string_lossy(),
            &workspace_path.to_string_lossy(),
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
    use crate::agent::conversation::ConversationMessage;

    fn create_store(home: &Path, workspace: &Path) -> SessionStore {
        create_session_store(home, "ollama", "fake", workspace).unwrap()
    }

    #[test]
    fn new_session_writes_current_schema_version() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let store = create_store(&temp.path().join("home"), &workspace);
        let raw = fs::read_to_string(store.dir.join("session.json")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schemaVersion"], SESSION_SCHEMA_VERSION);
        assert_eq!(store.metadata.schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(store.metadata.provider, "ollama");
        assert_eq!(store.metadata.model, "fake");
        assert_eq!(store.metadata.workspace_path, workspace);
        assert!(value.get("hostInstructions").is_none());
        assert!(value.get("threadId").is_none());
    }

    #[test]
    fn missing_or_old_schema_version_resume_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let home = temp.path().join("home");
        let store = create_store(&home, &workspace);

        fs::write(
            store.dir.join("session.json"),
            serde_json::json!({
                "sessionId": store.metadata.session_id,
                "provider": "ollama",
                "model": "fake",
                "sandboxPath": workspace,
                "createdAt": store.metadata.created_at,
                "updatedAt": store.metadata.updated_at
            })
            .to_string(),
        )
        .unwrap();
        let missing = load_session_store(
            &home,
            &store.metadata.session_id,
            "ollama",
            "fake",
            &workspace,
            1024,
        )
        .unwrap_err();
        assert_eq!(missing.code, "SESSION_SCHEMA_INCOMPATIBLE");

        fs::write(
            store.dir.join("session.json"),
            serde_json::json!({
                "schemaVersion": 1,
                "sessionId": store.metadata.session_id,
                "provider": "ollama",
                "model": "fake",
                "workspacePath": workspace,
                "createdAt": store.metadata.created_at,
                "updatedAt": store.metadata.updated_at
            })
            .to_string(),
        )
        .unwrap();
        let old = load_session_store(
            &home,
            &store.metadata.session_id,
            "ollama",
            "fake",
            &workspace,
            1024,
        )
        .unwrap_err();
        assert_eq!(old.code, "SESSION_SCHEMA_INCOMPATIBLE");
    }

    #[test]
    fn successful_turn_writes_exactly_one_committed_record() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let store = create_store(&temp.path().join("home"), &workspace);
        let record = CommittedTurnRecord::new(
            "turn-1",
            vec![
                ConversationMessage::user("hi"),
                ConversationMessage::assistant(Some("hello".into()), vec![]),
            ],
        );
        store.append_committed_turn(&record, 1024).unwrap();
        let loaded = load_committed_turn_records(&store.transcript_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].turn_id, "turn-1");
        assert_eq!(loaded[0].messages.len(), 2);
    }

    #[test]
    fn trailing_malformed_record_is_ignored_as_crash_debris() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let store = create_store(&temp.path().join("home"), &workspace);
        let record = CommittedTurnRecord::new("turn-1", vec![ConversationMessage::user("hi")]);
        store.append_committed_turn(&record, 1024).unwrap();
        let mut content = fs::read_to_string(&store.transcript_path).unwrap();
        content.push_str("{\"schemaVersion\":2,\"turnId\":\"turn-2\"");
        fs::write(&store.transcript_path, content).unwrap();

        let loaded = load_committed_turn_records(&store.transcript_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].turn_id, "turn-1");
    }

    #[test]
    fn malformed_earlier_record_is_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let store = create_store(&temp.path().join("home"), &workspace);
        let good = serde_json::to_string(&CommittedTurnRecord::new(
            "turn-2",
            vec![ConversationMessage::user("later")],
        ))
        .unwrap();
        fs::write(
            &store.transcript_path,
            format!("{{\"not\":\"a turn\"}}\n{good}\n"),
        )
        .unwrap();
        let err = load_committed_turn_records(&store.transcript_path).unwrap_err();
        assert_eq!(err.code, "SESSION_CORRUPT");
    }

    #[test]
    fn resume_rebuilds_committed_conversation_and_rejects_identity_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let home = temp.path().join("home");
        let store = create_store(&home, &workspace);
        store
            .append_committed_turn(
                &CommittedTurnRecord::new(
                    "t1",
                    vec![
                        ConversationMessage::user("one"),
                        ConversationMessage::assistant(Some("two".into()), vec![]),
                    ],
                ),
                4096,
            )
            .unwrap();
        let (loaded, messages) = load_session_store(
            &home,
            &store.metadata.session_id,
            "ollama",
            "fake",
            &workspace,
            4096,
        )
        .unwrap();
        assert!(loaded.resumed);
        assert_eq!(messages.len(), 2);

        let other = temp.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let err = load_session_store(
            &home,
            &store.metadata.session_id,
            "ollama",
            "fake",
            &other.canonicalize().unwrap(),
            4096,
        )
        .unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");

        let err = load_session_store(
            &home,
            &store.metadata.session_id,
            "ollama",
            "other-model",
            &workspace,
            4096,
        )
        .unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn create_session_generates_uuid_v7_and_layered_path() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let home = temp.path().join("home");
        let first = create_store(&home, &workspace);
        let second = create_store(&home, &workspace);
        let uuid = Uuid::parse_str(&first.metadata.session_id).unwrap();
        let (year, month) = uuid_year_month(&uuid, &first.metadata.session_id).unwrap();

        assert_eq!(uuid.get_version(), Some(Version::SortRand));
        assert_ne!(first.metadata.session_id, second.metadata.session_id);
        assert_eq!(
            first.dir,
            home.join("sessions")
                .join(format!("{year:04}"))
                .join(format!("{month:02}"))
                .join(&first.metadata.session_id)
        );
    }

    #[test]
    fn production_session_root_uses_agent_directory_not_binary_path() {
        let pedelec_home = PathBuf::from("/home/user/.pedelec");
        let agent_home = agent_home_dir_from_pedelec_home(&pedelec_home);
        let session_id = Uuid::now_v7().hyphenated().to_string();
        let uuid = Uuid::parse_str(&session_id).unwrap();
        let (year, month) = uuid_year_month(&uuid, &session_id).unwrap();

        assert_eq!(agent_home, PathBuf::from("/home/user/.pedelec/agent"));
        assert_ne!(agent_home, pedelec_home.join("pedelec-agent"));
        assert_eq!(
            session_dir_for_parts(&agent_home, year, month, &session_id),
            pedelec_home
                .join("agent")
                .join("sessions")
                .join(format!("{year:04}"))
                .join(format!("{month:02}"))
                .join(session_id)
        );
    }

    #[test]
    fn load_session_rejects_uuid_v4_and_missing_v7_session() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let home = temp.path().join("home");
        let err = load_session_store(
            &home,
            "123e4567-e89b-42d3-a456-426614174000",
            "ollama",
            "fake",
            &workspace,
            1024,
        )
        .unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");

        let missing = Uuid::now_v7().hyphenated().to_string();
        let err =
            load_session_store(&home, &missing, "ollama", "fake", &workspace, 1024).unwrap_err();
        assert_eq!(err.code, "SESSION_LOAD_FAILED");
    }
}
