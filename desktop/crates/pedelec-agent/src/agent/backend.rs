use super::conversation::{InferenceAttachment, NormalizedToolCall};
use super::error::AgentError;
use super::tools::AgentToolDefinition;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    pub tools: bool,
    pub vision: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InferenceUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl InferenceUsage {
    pub fn from_token_counts(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Self {
        let total_tokens = match (input_tokens, output_tokens) {
            (Some(input), Some(output)) => Some(input + output),
            (Some(input), None) => Some(input),
            (None, Some(output)) => Some(output),
            (None, None) => None,
        };
        Self {
            input_tokens,
            output_tokens,
            total_tokens,
        }
    }

    pub fn accumulate(&mut self, other: &InferenceUsage) {
        self.input_tokens = sum_opt(self.input_tokens, other.input_tokens);
        self.output_tokens = sum_opt(self.output_tokens, other.output_tokens);
        self.total_tokens = match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Some(input + output),
            _ => sum_opt(self.total_tokens, other.total_tokens),
        };
    }
}

fn sum_opt(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InferenceMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Vec<NormalizedToolCall>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub attachments: Vec<InferenceAttachment>,
}

impl InferenceMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            attachments: Vec::new(),
        }
    }

    pub fn from_conversation(
        message: &super::conversation::ConversationMessage,
        attachments: Vec<InferenceAttachment>,
    ) -> Self {
        Self {
            role: message.role.as_str().to_string(),
            content: message.content.clone(),
            tool_calls: message.tool_calls.clone(),
            tool_call_id: message.tool_call_id.clone(),
            tool_name: message.tool_name.clone(),
            attachments,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InferenceRequest {
    pub model: String,
    pub messages: Vec<InferenceMessage>,
    pub tools: Vec<AgentToolDefinition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InferenceResult {
    pub text: Option<String>,
    pub tool_calls: Vec<NormalizedToolCall>,
    pub usage: Option<InferenceUsage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceEvent {
    TextDelta(String),
}

pub trait InferenceEventSink {
    fn on_event(&mut self, event: InferenceEvent);
}

pub trait InferenceBackend: Send + Sync {
    fn inspect_model(&self, model: &str) -> Result<ModelCapabilities, AgentError>;

    fn infer(
        &self,
        request: InferenceRequest,
        sink: &mut dyn InferenceEventSink,
    ) -> Result<InferenceResult, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates_per_round_token_counts() {
        let mut usage = InferenceUsage::from_token_counts(Some(10), Some(4));
        usage.accumulate(&InferenceUsage::from_token_counts(Some(3), Some(7)));
        assert_eq!(usage.input_tokens, Some(13));
        assert_eq!(usage.output_tokens, Some(11));
        assert_eq!(usage.total_tokens, Some(24));
    }
}
