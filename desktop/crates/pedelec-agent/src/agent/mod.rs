mod backend;
mod config;
mod conversation;
mod error;
mod events;
mod instructions;
mod ollama;
mod sandbox;
mod session;
mod store;
mod tavily;
mod tools;

pub use backend::{
    InferenceBackend, InferenceEvent, InferenceEventSink, InferenceMessage, InferenceRequest,
    InferenceResult, InferenceUsage, ModelCapabilities,
};
pub use config::{
    resolve_server_config, resolve_server_config_from, AgentSessionConfig, BackendKind,
    PedelecAgentServerConfig, ServerConfigResolveInputs, ToolHostConfig,
};
pub use conversation::{
    ActiveTurn, CommittedTurnRecord, ConversationMessage, ConversationRole, InferenceAttachment,
    NormalizedToolCall, SESSION_SCHEMA_VERSION,
};
pub use error::AgentError;
pub use events::{AgentTurnSink, IgnoringTurnSink, TurnResult, TurnToolResult};
pub use ollama::OllamaBackend;
pub use session::AgentSession;
pub use store::SessionMetadata;
pub use tools::AgentToolDefinition;

pub fn run() -> i32 {
    eprintln!(
        "pedelec-agent one-shot CLI has been removed. Persistent server support lands in a later phase."
    );
    1
}
