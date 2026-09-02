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
        let record = ProtocolTrafficRecord {
            ts: ts.clone(),
            direction: direction.to_string(),
            kind: kind.to_string(),
            thread_id: thread_id.clone(),
            message: message.clone(),
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
            "message": message,
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
