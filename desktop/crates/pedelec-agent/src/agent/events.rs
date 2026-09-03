use super::backend::InferenceUsage;
use super::conversation::NormalizedToolCall;
use super::error::AgentError;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct TurnToolResult {
    pub tool_call_id: String,
    pub name: String,
    pub ok: bool,
    pub content: Option<Value>,
    pub error: Option<AgentError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub turn_id: String,
    pub text: String,
    pub usage: InferenceUsage,
}

pub trait AgentTurnSink {
    fn assistant_delta(&mut self, text: &str);
    fn usage_updated(&mut self, usage: &InferenceUsage);
    fn tool_call(&mut self, call: &NormalizedToolCall);
    fn tool_result(&mut self, result: &TurnToolResult);
}

#[derive(Debug, Default)]
pub struct IgnoringTurnSink;

impl AgentTurnSink for IgnoringTurnSink {
    fn assistant_delta(&mut self, _text: &str) {}
    fn usage_updated(&mut self, _usage: &InferenceUsage) {}
    fn tool_call(&mut self, _call: &NormalizedToolCall) {}
    fn tool_result(&mut self, _result: &TurnToolResult) {}
}
