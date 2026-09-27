use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolTrafficRecord {
    pub ts: String,
    pub direction: String,
    pub kind: String,
    pub thread_id: Option<String>,
    pub message: Value,
    pub unmatched: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProtocolTrafficLogger {
    writers: Arc<Mutex<HashMap<String, std::fs::File>>>,
}

fn redact_provider_artifact_ingress(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str) == Some("imageGeneration") {
                fields.remove("result");
                fields.remove("savedPath");
            }
            if fields.get("type").and_then(Value::as_str) == Some("image") {
                fields.remove("data");
                fields.remove("uri");
                if let Some(source) = fields.get_mut("source").and_then(Value::as_object_mut) {
                    source.remove("data");
                    source.remove("uri");
                }
            }
            if fields.get("type").and_then(Value::as_str) == Some("resource") {
                if let Some(resource) = fields.get_mut("resource").and_then(Value::as_object_mut) {
                    resource.remove("blob");
                    resource.remove("uri");
                }
            }
            if fields.get("method").and_then(Value::as_str) == Some("cursor/generate_image") {
                if let Some(params) = fields.get_mut("params").and_then(Value::as_object_mut) {
                    params.remove("filePath");
                    params.remove("referenceImagePaths");
                }
            }
            for child in fields.values_mut() {
                redact_provider_artifact_ingress(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_provider_artifact_ingress(item);
            }
        }
        _ => {}
    }
}

impl ProtocolTrafficLogger {
    pub fn register_protocol_log(&self, owner: &str, provider: &str, workspace: &Path) {
        if self
            .writers
            .lock()
            .expect("protocol log mutex poisoned")
            .contains_key(owner)
        {
            return;
        }
        let logs_root = workspace.join(".pedelec-runtime").join("logs");
        if fs::create_dir_all(&logs_root).is_err() {
            return;
        }
        let safe_provider = provider
            .to_ascii_lowercase()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let path = logs_root.join(format!(
            "protocol-{safe_provider}-{owner}-{}.jsonl",
            Uuid::new_v4()
        ));
        let Ok(file) = OpenOptions::new().create_new(true).append(true).open(path) else {
            return;
        };
        self.writers
            .lock()
            .expect("protocol log mutex poisoned")
            .entry(owner.to_string())
            .or_insert(file);
    }

    pub fn capture(
        &self,
        direction: &str,
        kind: &str,
        message: &Value,
        owner: Option<&str>,
        unmatched: bool,
    ) -> ProtocolTrafficRecord {
        let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let thread_id = owner.map(str::to_string);
        let mut safe_message = message.clone();
        redact_provider_artifact_ingress(&mut safe_message);
        let record = ProtocolTrafficRecord {
            ts: ts.clone(),
            direction: direction.to_string(),
            kind: kind.to_string(),
            thread_id: thread_id.clone(),
            message: safe_message.clone(),
            unmatched,
        };

        let Some(owner) = owner else {
            return record;
        };
        let jsonl_record = json!({
            "ts": ts,
            "direction": direction,
            "kind": kind,
            "threadId": owner,
            "message": safe_message,
        });
        let Ok(mut bytes) = serde_json::to_vec(&jsonl_record) else {
            return record;
        };
        bytes.push(b'\n');
        let mut writers = self.writers.lock().expect("protocol log mutex poisoned");
        let Some(writer) = writers.get_mut(owner) else {
            return record;
        };
        let _ = writer.write_all(&bytes);
        let _ = writer.flush();
        record
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;

    #[test]
    fn generated_image_payload_and_private_path_do_not_enter_protocol_records() {
        let logger = ProtocolTrafficLogger::default();
        let record = logger.capture(
            "in",
            "notification",
            &json!({
                "method":"turn/completed", "params":{"turn":{"items":[
                    {"type":"imageGeneration", "id":"image-1", "result":"secret-base64",
                     "savedPath":"C:/private/image.png"},
                    {"type":"agentMessage", "text":"visible"}
                ]}}
            }),
            None,
            false,
        );
        let encoded = serde_json::to_string(&record.message).unwrap();
        assert!(!encoded.contains("secret-base64"));
        assert!(!encoded.contains("C:/private"));
        assert!(encoded.contains("visible"));
        assert!(encoded.contains("image-1"));
    }

    #[test]
    fn acp_artifact_payloads_and_cursor_paths_do_not_enter_protocol_records() {
        let logger = ProtocolTrafficLogger::default();
        let record = logger.capture(
            "in",
            "notification",
            &json!({
                "method":"session/update",
                "params":{"update":{"content":[
                    {"type":"image","data":"acp-inline-secret","mimeType":"image/png","uri":"file:///private/image.png"},
                    {"type":"resource","resource":{"blob":"embedded-secret","mimeType":"application/pdf","uri":"file:///private/report.pdf"}}
                ]}}
            }),
            None,
            false,
        );
        let encoded = serde_json::to_string(&record.message).unwrap();
        assert!(!encoded.contains("acp-inline-secret"));
        assert!(!encoded.contains("embedded-secret"));
        assert!(!encoded.contains("file:///private"));
        assert!(encoded.contains("image/png"));

        let cursor = logger.capture(
            "in",
            "notification",
            &json!({
                "method":"cursor/generate_image",
                "params":{
                    "toolCallId":"cursor-tool-1",
                    "description":"generated image",
                    "filePath":"C:/private/generated.png",
                    "referenceImagePaths":["C:/private/reference.png"]
                }
            }),
            None,
            false,
        );
        let encoded = serde_json::to_string(&cursor.message).unwrap();
        assert!(!encoded.contains("C:/private"));
        assert!(encoded.contains("cursor-tool-1"));
    }
}
