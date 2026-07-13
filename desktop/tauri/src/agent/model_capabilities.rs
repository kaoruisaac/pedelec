use super::config::AgentConfig;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn supports_vision(config: &AgentConfig) -> bool {
    let key = format!("{}\u{1f}{}", config.ollama_base_url, config.model);
    let home = match crate::pedelec_paths::pedelec_home_dir() {
        Ok(p) => p,
        Err(_) => return query(config).unwrap_or(false),
    };
    let path = home.join("cache").join("ollama-model-capabilities.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(text) = fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if value.get("schemaVersion").and_then(|v| v.as_u64()) == Some(1) {
                if let Some(entry) = value.get("entries").and_then(|v| v.get(&key)) {
                    if entry.get("expiresAt").and_then(|v| v.as_u64()).unwrap_or(0) > now {
                        return entry.get("status").and_then(|v| v.as_str()) == Some("success")
                            && entry
                                .get("capabilities")
                                .and_then(|v| v.as_array())
                                .is_some_and(|a| {
                                    a.iter().any(|v| v == "tools")
                                        && a.iter().any(|v| v == "vision")
                                });
                    }
                }
            }
        }
    }
    let queried = query(config);
    let result = queried.unwrap_or(false);
    let status = if queried.is_ok() {
        "success"
    } else {
        "failure"
    };
    let expires = now + if queried.is_ok() { 86_400 } else { 300 };
    let capabilities = if result {
        serde_json::json!(["tools", "vision"])
    } else {
        serde_json::json!([])
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_ok() {
            let mut root = serde_json::json!({"schemaVersion":1,"entries":{}});
            if let Ok(old) = fs::read_to_string(&path)
                .and_then(|s| Ok(serde_json::from_str(&s).unwrap_or(root.clone())))
            {
                root = old;
            }
            root["schemaVersion"] = serde_json::json!(1);
            root["entries"][&key] = serde_json::json!({"baseUrl":config.ollama_base_url,"model":config.model,"status":status,"capabilities":capabilities,"checkedAt":now,"expiresAt":expires});
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, root.to_string()).is_ok() {
                let _ = fs::rename(tmp, path);
            }
        }
    }
    result
}

fn query(config: &AgentConfig) -> Result<bool, ()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(config.ollama_timeout_ms))
        .build()
        .map_err(|_| ())?;
    let response = client
        .post(format!(
            "{}/api/show",
            config.ollama_base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.ollama_api_key)
        .header("Content-Type", "application/json")
        .body(serde_json::json!({"model":config.model}).to_string())
        .send()
        .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let value: serde_json::Value = response
        .text()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or(())?;
    let caps = value.get("capabilities").and_then(|v| v.as_array());
    Ok(caps.is_some_and(|a| a.iter().any(|v| v == "tools") && a.iter().any(|v| v == "vision")))
}
