mod backend;
mod config;
mod conversation;
mod error;
mod events;
mod instructions;
mod lifecycle;
mod ollama;
mod sandbox;
mod server;
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
pub use server::{
    parse_cli, serve_stdio, CliAction, PedelecAgentServer, ServeCommand, ServeOptions,
    ServeOutcome, PROTOCOL_VERSION,
};
pub use session::AgentSession;
pub use store::SessionMetadata;
pub use tools::AgentToolDefinition;

pub fn run() -> i32 {
    match parse_cli(std::env::args()) {
        Err(err) => {
            eprintln!("{}: {}", err.code, err.message);
            1
        }
        Ok(CliAction::Serve(command)) => match resolve_server_config() {
            Ok(config) if config.provider != command.provider => {
                eprintln!(
                    "CONFIG_ERROR: launched provider {} does not match server config {}",
                    command.provider.as_str(),
                    config.provider.as_str()
                );
                1
            }
            Ok(config) => match serve_stdio(config) {
                Ok(ServeOutcome::Shutdown | ServeOutcome::StdinClosed) => 0,
                Ok(ServeOutcome::Fatal) => 1,
                Err(err) => {
                    eprintln!("{}: {}", err.code, err.message);
                    1
                }
            },
            Err(err) => {
                eprintln!("{}: {}", err.code, err.message);
                1
            }
        },
    }
}
