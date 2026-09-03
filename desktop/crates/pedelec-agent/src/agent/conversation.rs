use super::error::AgentError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConversationRole {
    User,
    Assistant,
    Tool,
}

impl ConversationRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl NormalizedToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationMessage {
    pub role: ConversationRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<NormalizedToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ConversationMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(text: Option<String>, tool_calls: Vec<NormalizedToolCall>) -> Self {
        Self {
            role: ConversationRole::Assistant,
            content: text,
            tool_calls,
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: ConversationRole::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceAttachment {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommittedTurnRecord {
    pub schema_version: u32,
    pub turn_id: String,
    pub committed_at: DateTime<Utc>,
    pub messages: Vec<ConversationMessage>,
}

impl CommittedTurnRecord {
    pub fn new(turn_id: impl Into<String>, messages: Vec<ConversationMessage>) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            turn_id: turn_id.into(),
            committed_at: Utc::now(),
            messages,
        }
    }

    pub fn validate_schema(&self) -> Result<(), AgentError> {
        if self.schema_version != SESSION_SCHEMA_VERSION {
            return Err(AgentError::with_details(
                "SESSION_SCHEMA_INCOMPATIBLE",
                "Persisted turn record schema is not supported.",
                serde_json::json!({
                    "schemaVersion": self.schema_version,
                    "supported": SESSION_SCHEMA_VERSION
                }),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub messages: Vec<ConversationMessage>,
    pub attachments_by_message: Vec<Vec<InferenceAttachment>>,
    pub cumulative_usage: super::backend::InferenceUsage,
    pub current_tool_round: usize,
}

impl ActiveTurn {
    pub fn begin(turn_id: impl Into<String>, user_message: ConversationMessage) -> Self {
        Self {
            turn_id: turn_id.into(),
            messages: vec![user_message],
            attachments_by_message: vec![Vec::new()],
            cumulative_usage: super::backend::InferenceUsage::default(),
            current_tool_round: 0,
        }
    }

    pub fn push(
        &mut self,
        message: ConversationMessage,
        attachments: Vec<InferenceAttachment>,
    ) -> Result<(), AgentError> {
        if self.messages.len() != self.attachments_by_message.len() {
            return Err(AgentError::new(
                "INTERNAL_INVARIANT",
                "Active turn message and attachment lists are out of sync.",
            ));
        }
        self.messages.push(message);
        self.attachments_by_message.push(attachments);
        Ok(())
    }

    pub fn clear_attachments(&mut self) {
        for attachments in &mut self.attachments_by_message {
            attachments.clear();
        }
    }
}
