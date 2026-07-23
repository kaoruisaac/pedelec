use pedelec_lib::pedelec_core::{PedelecSettings, ProviderCode};
use serde_json::json;

#[test]
fn provider_code_serializes_as_the_public_wire_contract() {
    assert_eq!(
        serde_json::to_value(ProviderCode::Codex).unwrap(),
        json!("codex")
    );
    assert_eq!(
        serde_json::to_value(ProviderCode::Antigravity).unwrap(),
        json!("antigravity")
    );
    assert_eq!(
        serde_json::to_value(ProviderCode::OpenCode).unwrap(),
        json!("opencode")
    );
    assert_eq!(
        serde_json::to_value(ProviderCode::Cursor).unwrap(),
        json!("cursor")
    );
    assert_eq!(
        serde_json::to_value(ProviderCode::Claude).unwrap(),
        json!("claude")
    );
    assert_eq!(
        serde_json::to_value(ProviderCode::Ollama).unwrap(),
        json!("ollama")
    );
    assert!(serde_json::from_value::<ProviderCode>(json!("gemini")).is_err());
    assert_eq!(
        serde_json::from_value::<ProviderCode>(json!("cursor")).unwrap(),
        ProviderCode::Cursor
    );
    assert_eq!(
        serde_json::from_value::<ProviderCode>(json!("claude")).unwrap(),
        ProviderCode::Claude
    );
    assert_eq!(
        serde_json::from_value::<ProviderCode>(json!("ollama")).unwrap(),
        ProviderCode::Ollama
    );
}

#[test]
fn default_settings_are_serializable() {
    let settings = PedelecSettings::default();
    assert!(serde_json::to_value(settings).unwrap().is_object());
}
