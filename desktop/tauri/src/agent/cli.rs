use super::error::AgentError;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub session_id: String,
    pub prompt: String,
    pub sandbox: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub env_file: Option<PathBuf>,
    pub pedelec_cli: Option<PathBuf>,
    pub core_runtime_file: Option<PathBuf>,
}

pub fn parse_args(args: Vec<String>) -> Result<CliArgs, AgentError> {
    let mut rest = args.into_iter().skip(1).collect::<Vec<_>>();
    if rest.first().map(String::as_str) == Some("run") {
        rest.remove(0);
    }

    let session_id = take_positional(&mut rest, "session_id")?;
    let prompt = take_positional(&mut rest, "prompt")?;
    let mut parsed = CliArgs {
        session_id,
        prompt,
        ..CliArgs::default()
    };

    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--jsonl" => {
                index += 1;
            }
            "--sandbox" => {
                parsed.sandbox = Some(PathBuf::from(take_option_value(&rest, index)?));
                index += 2;
            }
            "--provider" => {
                parsed.provider = Some(take_option_value(&rest, index)?);
                index += 2;
            }
            "--model" => {
                parsed.model = Some(take_option_value(&rest, index)?);
                index += 2;
            }
            "--env-file" => {
                parsed.env_file = Some(PathBuf::from(take_option_value(&rest, index)?));
                index += 2;
            }
            "--pedelec-cli" => {
                parsed.pedelec_cli = Some(PathBuf::from(take_option_value(&rest, index)?));
                index += 2;
            }
            "--core-runtime-file" => {
                parsed.core_runtime_file = Some(PathBuf::from(take_option_value(&rest, index)?));
                index += 2;
            }
            other => {
                return Err(AgentError::with_details(
                    "INVALID_ARGUMENT",
                    format!("Unknown argument: {other}"),
                    serde_json::json!({ "argument": other }),
                ));
            }
        }
    }

    Ok(parsed)
}

fn take_positional(rest: &mut Vec<String>, name: &str) -> Result<String, AgentError> {
    if rest.is_empty() || rest[0].starts_with("--") {
        return Err(AgentError::new(
            "INVALID_ARGUMENT",
            format!("Missing required argument: {name}"),
        ));
    }
    Ok(rest.remove(0))
}

fn take_option_value(rest: &[String], index: usize) -> Result<String, AgentError> {
    rest.get(index + 1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or_else(|| {
            AgentError::new(
                "INVALID_ARGUMENT",
                format!("Missing value for {}", rest[index]),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_command() {
        let args = parse_args(vec![
            "pedelec-agent".into(),
            "run".into(),
            "s1".into(),
            "hello".into(),
            "--provider".into(),
            "ollama".into(),
        ])
        .unwrap();

        assert_eq!(args.session_id, "s1");
        assert_eq!(args.prompt, "hello");
        assert_eq!(args.provider.as_deref(), Some("ollama"));
    }

    #[test]
    fn parses_shorthand_command() {
        let args = parse_args(vec![
            "pedelec-agent".into(),
            "s1".into(),
            "hello".into(),
            "--jsonl".into(),
        ])
        .unwrap();

        assert_eq!(args.session_id, "s1");
        assert_eq!(args.prompt, "hello");
    }
}
