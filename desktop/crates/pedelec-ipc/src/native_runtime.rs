//! Native-only Runtime admission. The browser cannot read the per-Desktop key.
//! Chrome's nativeMessaging pipe is the trusted ingress; Extension background
//! derives grants from storage and Instance identity from its external Port.
//! Local programs running as the Desktop user are inside this trust boundary.
use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const PROOF: &str = "_nativeRuntimeProof";
const TTL_MS: u64 = 30_000;

fn denied() -> PedelecError {
    PedelecError::new(
        error_codes::IPC_UNAUTHORIZED,
        "Native Runtime authorization is missing, expired or invalid",
    )
}

fn now_ms() -> Result<u64, PedelecError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| denied())?
        .as_millis() as u64)
}

fn key_path(runtime_path: &Path) -> PathBuf {
    runtime_path.with_extension("native-runtime-key")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Proof {
    grant: Value,
    issued_at: u64,
    nonce: String,
    mac: Vec<u8>,
}

fn signed_bytes(request: &CoreIpcRequest, proof: &Proof) -> Result<Vec<u8>, PedelecError> {
    let mut unsigned = request.clone();
    if let Some(Value::Object(payload)) = unsigned.payload.as_mut() {
        payload.remove(PROOF);
    }
    serde_json::to_vec(&(unsigned, &proof.grant, proof.issued_at, &proof.nonce))
        .map_err(|_| denied())
}

fn validate_grant(request: &CoreIpcRequest, grant: &Value) -> Result<(), PedelecError> {
    if grant["version"] != 1
        || request.caller_origin.as_deref() != grant["origin"].as_str()
        || request.caller_origin.is_none()
    {
        return Err(denied());
    }
    match request.r#type.as_str() {
        "revoke_runtime_full_access" if grant["revoke"] == true => Ok(()),
        "create_thread"
            if grant["instanceRequested"] == true
                && grant["agentSessionsApproved"] == true
                && grant["runtimeFullAccessApproved"] == true
                && grant["instanceId"].as_str().is_some_and(|s| !s.is_empty())
                && request
                    .payload
                    .as_ref()
                    .is_some_and(|p| p["runtimeFullAccess"] == true) =>
        {
            Ok(())
        }
        _ => Err(denied()),
    }
}

/// Called only on the Chrome nativeMessaging ingress, after stripping arbitrary
/// page fields. Never expose this signing operation through Core IPC or HTTP.
pub fn attest_native_runtime_request(
    request: &mut CoreIpcRequest,
    grant: Value,
    runtime_path: &Path,
) -> Result<(), PedelecError> {
    validate_grant(request, &grant)?;
    let key = fs::read(key_path(runtime_path)).map_err(|_| denied())?;
    let mut proof = Proof {
        grant,
        issued_at: now_ms()?,
        nonce: uuid::Uuid::new_v4().to_string(),
        mac: vec![],
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| denied())?;
    mac.update(&signed_bytes(request, &proof)?);
    proof.mac = mac.finalize().into_bytes().to_vec();
    request
        .payload
        .as_mut()
        .and_then(Value::as_object_mut)
        .ok_or_else(denied)?
        .insert(
            PROOF.into(),
            serde_json::to_value(proof).map_err(|_| denied())?,
        );
    Ok(())
}

/// Production composition used by every Desktop IPC server, including Tauri.
/// Directory selection stays in the toolkit adapter; admission lives here.
pub struct NativeRuntimePlatformServices {
    inner: Arc<dyn CoreIpcPlatformServices>,
    runtime: SharedCoreRuntime,
    key: Vec<u8>,
    used: Mutex<HashMap<String, u64>>,
}

impl NativeRuntimePlatformServices {
    pub fn install(
        inner: Arc<dyn CoreIpcPlatformServices>,
        runtime: SharedCoreRuntime,
        runtime_path: &Path,
    ) -> Result<Self, PedelecError> {
        let key = [
            uuid::Uuid::new_v4().as_bytes().as_slice(),
            uuid::Uuid::new_v4().as_bytes().as_slice(),
        ]
        .concat();
        let path = key_path(runtime_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| denied())?;
        }
        // Replace on each Desktop start, invalidating all previous attestations.
        // Remove first so an existing permissive Unix file cannot retain its mode.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(denied()),
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .and_then(|mut f| f.write_all(&key))
            .map_err(|_| denied())?;
        Ok(Self {
            inner,
            runtime,
            key,
            used: Mutex::new(HashMap::new()),
        })
    }
}

impl CoreIpcPlatformServices for NativeRuntimePlatformServices {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError> {
        self.inner.pick_directory()
    }

    fn authorize_extension_runtime_request(
        &self,
        request: &CoreIpcRequest,
    ) -> Result<pedelec_core::ExtensionRuntimeAuthorization, PedelecError> {
        let proof: Proof = serde_json::from_value(
            request
                .payload
                .as_ref()
                .and_then(|p| p.get(PROOF))
                .cloned()
                .ok_or_else(denied)?,
        )
        .map_err(|_| denied())?;
        let now = now_ms()?;
        if proof.issued_at > now || now - proof.issued_at > TTL_MS {
            return Err(denied());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).map_err(|_| denied())?;
        mac.update(&signed_bytes(request, &proof)?);
        mac.verify_slice(&proof.mac).map_err(|_| denied())?;
        validate_grant(request, &proof.grant)?;
        let mut used = self.used.lock().unwrap();
        used.retain(|_, timestamp| now.saturating_sub(*timestamp) <= TTL_MS);
        if used.contains_key(&proof.nonce) || used.len() >= 10_000 {
            return Err(denied());
        }
        used.insert(proof.nonce, proof.issued_at);
        self.runtime
            .lock()
            .unwrap()
            .grant_extension_runtime_full_access(
                request.caller_origin.as_deref().ok_or_else(denied)?,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_adapter_rejects_expiration_replay_restart_and_changed_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.json");
        let core = Arc::new(Mutex::new(pedelec_core::CoreRuntime::default()));
        let platform = NativeRuntimePlatformServices::install(
            Arc::new(NoopCoreIpcPlatformServices),
            core.clone(),
            &path,
        )
        .unwrap();
        let mut request = CoreIpcRequest {
            request_id: "proof".into(),
            r#type: "create_thread".into(),
            caller_origin: Some("https://app.example.test".into()),
            caller_sdk_version: None,
            payload: Some(serde_json::json!({ "provider": "codex", "runtimeFullAccess": true })),
        };
        let grant = serde_json::json!({ "version": 1, "origin": "https://app.example.test", "instanceId": "port-one",
            "instanceRequested": true, "agentSessionsApproved": true, "runtimeFullAccessApproved": true });
        attest_native_runtime_request(&mut request, grant, &path).unwrap();
        let mut expired = request.clone();
        let mut proof: Proof =
            serde_json::from_value(expired.payload.as_ref().unwrap()[PROOF].clone()).unwrap();
        proof.issued_at = now_ms().unwrap() - TTL_MS - 1;
        let mut mac = Hmac::<Sha256>::new_from_slice(&platform.key).unwrap();
        mac.update(&signed_bytes(&expired, &proof).unwrap());
        proof.mac = mac.finalize().into_bytes().to_vec();
        expired.payload.as_mut().unwrap()[PROOF] = serde_json::to_value(proof).unwrap();
        assert!(platform
            .authorize_extension_runtime_request(&expired)
            .is_err());
        let mut changed = request.clone();
        changed.payload.as_mut().unwrap()["workspaceId"] = serde_json::json!("different-workspace");
        assert!(platform
            .authorize_extension_runtime_request(&changed)
            .is_err());
        assert!(platform
            .authorize_extension_runtime_request(&request)
            .is_ok());
        assert!(platform
            .authorize_extension_runtime_request(&request)
            .is_err());
        let restarted = NativeRuntimePlatformServices::install(
            Arc::new(NoopCoreIpcPlatformServices),
            core,
            &path,
        )
        .unwrap();
        assert!(restarted
            .authorize_extension_runtime_request(&request)
            .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(key_path(&path)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
