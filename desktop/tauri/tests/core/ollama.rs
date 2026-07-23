use pedelec_lib::pedelec_core::{validate_ollama_base_url, validate_ollama_timeout};

#[test]
fn ollama_public_validation_accepts_a_valid_local_endpoint() {
    assert_eq!(
        validate_ollama_base_url("http://127.0.0.1:11434").unwrap(),
        "http://127.0.0.1:11434"
    );
    assert_eq!(validate_ollama_timeout(30_000).unwrap(), 30_000);
}

#[test]
fn ollama_public_validation_rejects_an_api_path_and_zero_timeout() {
    assert!(validate_ollama_base_url("http://127.0.0.1:11434/api").is_err());
    assert!(validate_ollama_timeout(0).is_err());
}
