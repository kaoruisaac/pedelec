use super::super::config::BackendKind;
use super::super::error::AgentError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeCommand {
    pub provider: BackendKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Serve(ServeCommand),
}

pub fn parse_cli<I, S>(args: I) -> Result<CliAction, AgentError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter().map(|arg| arg.as_ref().to_string());
    let _bin = args.next();
    let mut positional = Vec::new();
    let mut provider: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                return Err(AgentError::new(
                    "USAGE",
                    "usage: pedelec-agent serve --provider ollama",
                ));
            }
            "--provider" => {
                let value = args.next().ok_or_else(|| {
                    AgentError::new("INVALID_ARGUMENT", "--provider requires a value.")
                })?;
                provider = Some(value);
            }
            flag if flag.starts_with("--provider=") => {
                provider = Some(flag.trim_start_matches("--provider=").to_string());
            }
            "run" => {
                return Err(AgentError::new(
                    "LEGACY_CLI_REMOVED",
                    "pedelec-agent run has been removed. Use: pedelec-agent serve --provider ollama",
                ));
            }
            "--session-id" | "--sandbox" | "--model" | "--jsonl" => {
                return Err(AgentError::new(
                    "LEGACY_CLI_REMOVED",
                    "One-shot pedelec-agent flags have been removed. Use: pedelec-agent serve --provider ollama",
                ));
            }
            other if other.starts_with('-') => {
                return Err(AgentError::with_details(
                    "INVALID_ARGUMENT",
                    "Unknown argument",
                    serde_json::json!({ "argument": other }),
                ));
            }
            other => positional.push(other.to_string()),
        }
    }

    match positional.as_slice() {
        [] => {}
        [command] if command == "serve" => {}
        [command, ..] if command == "serve" => {
            return Err(AgentError::new(
                "INVALID_ARGUMENT",
                "usage: pedelec-agent serve --provider ollama",
            ));
        }
        [command, ..] => {
            return Err(AgentError::with_details(
                "LEGACY_CLI_REMOVED",
                "Unknown or removed pedelec-agent command. Use: pedelec-agent serve --provider ollama",
                serde_json::json!({ "command": command }),
            ));
        }
    }

    let Some(provider) = provider else {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            "usage: pedelec-agent serve --provider ollama",
        ));
    };
    match provider.trim().to_ascii_lowercase().as_str() {
        "ollama" => Ok(CliAction::Serve(ServeCommand {
            provider: BackendKind::Ollama,
        })),
        other => Err(AgentError::with_details(
            "INVALID_ARGUMENT",
            "Unsupported provider. Only ollama is available.",
            serde_json::json!({ "provider": other }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_provider_ollama_is_accepted() {
        let parsed = parse_cli(["pedelec-agent", "serve", "--provider", "ollama"]).unwrap();
        assert_eq!(
            parsed,
            CliAction::Serve(ServeCommand {
                provider: BackendKind::Ollama
            })
        );
        assert_eq!(
            parse_cli(["pedelec-agent", "serve", "--provider=ollama"]).unwrap(),
            parsed
        );
    }

    #[test]
    fn run_subcommand_is_rejected() {
        let err = parse_cli(["pedelec-agent", "run", "--sandbox", "."]).unwrap_err();
        assert_eq!(err.code, "LEGACY_CLI_REMOVED");
        assert!(err.message.contains("serve --provider ollama"));
    }

    #[test]
    fn one_shot_flags_are_rejected() {
        for args in [
            vec!["pedelec-agent", "--sandbox", "."],
            vec![
                "pedelec-agent",
                "--session-id",
                "0197d8f0-8e3c-7b1a-a331-3fcf7b1f9176",
            ],
            vec!["pedelec-agent", "--model", "qwen"],
            vec![
                "pedelec-agent",
                "serve",
                "--provider",
                "ollama",
                "--sandbox",
                ".",
            ],
        ] {
            let err = parse_cli(args).unwrap_err();
            assert_eq!(err.code, "LEGACY_CLI_REMOVED");
        }
    }

    #[test]
    fn missing_or_unknown_provider_is_rejected() {
        let missing = parse_cli(["pedelec-agent"]).unwrap_err();
        assert_eq!(missing.code, "INVALID_ARGUMENT");
        assert!(missing.message.contains("serve --provider ollama"));
        let missing = parse_cli(["pedelec-agent", "serve"]).unwrap_err();
        assert_eq!(missing.code, "INVALID_ARGUMENT");
        let unknown = parse_cli(["pedelec-agent", "serve", "--provider", "openai"]).unwrap_err();
        assert_eq!(unknown.code, "INVALID_ARGUMENT");
    }
}
