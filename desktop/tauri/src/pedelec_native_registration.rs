use crate::pedelec_core::{error_codes, PedelecError};
use crate::pedelec_paths::{pedelec_home_dir, pedelec_native_host_install_path};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const CHROME_NATIVE_HOST_NAME: &str = "cc.isaaclin.pedelec";
const CHROME_EXTENSION_ID: &str = "ogccgaminlphbkeghldidiiimajfdpag";
const DEV_CHROME_EXTENSION_ID_ENV: &str = "PEDELEC_DEV_CHROME_EXTENSION_ID";
const MANIFEST_FILE_NAME: &str = "cc.isaaclin.pedelec.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMessagingRegistration {
    pub host_name: String,
    pub manifest_path: PathBuf,
    pub native_host_path: PathBuf,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct NativeHostManifest {
    name: String,
    description: String,
    path: String,
    #[serde(rename = "type")]
    interface_type: String,
    allowed_origins: Vec<String>,
}

pub fn register_chrome_native_messaging_host() -> Result<NativeMessagingRegistration, PedelecError>
{
    let native_host_path = pedelec_native_host_install_path()?;
    let manifest_path = chrome_native_host_manifest_path()?;

    register_chrome_native_messaging_host_with_paths(
        chrome_extension_id(),
        &native_host_path,
        &manifest_path,
    )
}

#[cfg(debug_assertions)]
fn chrome_extension_id() -> &'static str {
    let _ = DEV_CHROME_EXTENSION_ID_ENV;
    option_env!("PEDELEC_DEV_CHROME_EXTENSION_ID")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(CHROME_EXTENSION_ID)
}

#[cfg(not(debug_assertions))]
fn chrome_extension_id() -> &'static str {
    let _ = DEV_CHROME_EXTENSION_ID_ENV;
    CHROME_EXTENSION_ID
}

fn validate_chrome_extension_id(extension_id: &str) -> Result<(), PedelecError> {
    let extension_id = extension_id.trim();
    if extension_id.len() != 32 || !extension_id.chars().all(|c| matches!(c, 'a'..='p')) {
        return Err(PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "Chrome extension ID must be 32 characters using only letters a-p",
            serde_json::json!({ "extensionId": extension_id }),
        ));
    }
    Ok(())
}

fn register_chrome_native_messaging_host_with_paths(
    extension_id: &str,
    native_host_path: &Path,
    manifest_path: &Path,
) -> Result<NativeMessagingRegistration, PedelecError> {
    validate_chrome_extension_id(extension_id)?;
    if !native_host_path.exists() {
        return Err(PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "installed pedelec-native-host binary was not found",
            serde_json::json!({ "path": native_host_path.to_string_lossy() }),
        ));
    }

    let manifest = build_native_host_manifest(extension_id, native_host_path);
    write_native_host_manifest(manifest_path, &manifest)?;
    register_manifest_with_platform(manifest_path)?;

    Ok(NativeMessagingRegistration {
        host_name: CHROME_NATIVE_HOST_NAME.to_string(),
        manifest_path: manifest_path.to_path_buf(),
        native_host_path: native_host_path.to_path_buf(),
    })
}

fn build_native_host_manifest(extension_id: &str, native_host_path: &Path) -> NativeHostManifest {
    NativeHostManifest {
        name: CHROME_NATIVE_HOST_NAME.to_string(),
        description: "Pedelec native messaging host".to_string(),
        path: native_host_path.to_string_lossy().to_string(),
        interface_type: "stdio".to_string(),
        allowed_origins: vec![format!("chrome-extension://{}/", extension_id.trim())],
    }
}

fn write_native_host_manifest(
    manifest_path: &Path,
    manifest: &NativeHostManifest,
) -> Result<(), PedelecError> {
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot create Chrome native messaging manifest directory",
                serde_json::json!({
                    "path": parent.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })?;
    }

    let payload = serde_json::to_string_pretty(manifest).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot serialize Chrome native messaging manifest",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(manifest_path, payload).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot write Chrome native messaging manifest",
            serde_json::json!({
                "path": manifest_path.to_string_lossy(),
                "error": err.to_string()
            }),
        )
    })
}

#[cfg(target_os = "windows")]
fn chrome_native_host_manifest_path() -> Result<PathBuf, PedelecError> {
    Ok(windows_chrome_native_host_manifest_path(
        &pedelec_home_dir()?
    ))
}

#[cfg(target_os = "macos")]
fn chrome_native_host_manifest_path() -> Result<PathBuf, PedelecError> {
    let home = dirs::home_dir().ok_or_else(|| {
        PedelecError::new(
            error_codes::IPC_UNAVAILABLE,
            "cannot resolve home directory for Chrome native messaging manifest",
        )
    })?;
    Ok(macos_chrome_native_host_manifest_path(&home))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn chrome_native_host_manifest_path() -> Result<PathBuf, PedelecError> {
    Err(PedelecError::new(
        error_codes::IPC_UNAVAILABLE,
        "Chrome native messaging registration is only supported on Windows and macOS",
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_chrome_native_host_manifest_path(pedelec_home: &Path) -> PathBuf {
    pedelec_home.join(MANIFEST_FILE_NAME)
}

#[cfg(any(test, target_os = "macos"))]
fn macos_chrome_native_host_manifest_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Google")
        .join("Chrome")
        .join("NativeMessagingHosts")
        .join(MANIFEST_FILE_NAME)
}

#[cfg(target_os = "windows")]
fn register_manifest_with_platform(manifest_path: &Path) -> Result<(), PedelecError> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key_path = format!(
        r"Software\Google\Chrome\NativeMessagingHosts\{}",
        CHROME_NATIVE_HOST_NAME
    );
    let (key, _) = hkcu.create_subkey(&key_path).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot create Chrome native messaging registry key",
            serde_json::json!({
                "key": key_path,
                "error": err.to_string()
            }),
        )
    })?;
    key.set_value("", &manifest_path.to_string_lossy().to_string())
        .map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot set Chrome native messaging registry value",
                serde_json::json!({
                    "path": manifest_path.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })
}

#[cfg(target_os = "macos")]
fn register_manifest_with_platform(_manifest_path: &Path) -> Result<(), PedelecError> {
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn register_manifest_with_platform(_manifest_path: &Path) -> Result<(), PedelecError> {
    Err(PedelecError::new(
        error_codes::IPC_UNAVAILABLE,
        "Chrome native messaging registration is only supported on Windows and macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hardcoded_extension_id_is_valid() {
        validate_chrome_extension_id(CHROME_EXTENSION_ID).unwrap();
    }

    #[test]
    fn invalid_extension_id_returns_error() {
        let err = validate_chrome_extension_id("invalid").unwrap_err();

        assert_eq!(err.code, error_codes::IPC_UNAVAILABLE);
        assert!(err.message.contains("Chrome extension ID"));
    }

    #[test]
    fn manifest_contains_allowed_origin() {
        let native_host_path = PathBuf::from(r"C:\pedelec\pedelec-native-host.exe");

        let manifest = build_native_host_manifest(CHROME_EXTENSION_ID, &native_host_path);
        let value = serde_json::to_value(manifest).unwrap();

        assert_eq!(
            value,
            json!({
                "name": CHROME_NATIVE_HOST_NAME,
                "description": "Pedelec native messaging host",
                "path": native_host_path.to_string_lossy(),
                "type": "stdio",
                "allowed_origins": [
                    format!("chrome-extension://{CHROME_EXTENSION_ID}/")
                ]
            })
        );
    }

    #[test]
    fn manifest_can_use_custom_extension_id() {
        let native_host_path = PathBuf::from(r"C:\pedelec\pedelec-native-host.exe");
        let extension_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let manifest = build_native_host_manifest(extension_id, &native_host_path);
        let value = serde_json::to_value(manifest).unwrap();

        assert_eq!(
            value["allowed_origins"],
            json!([format!("chrome-extension://{extension_id}/")])
        );
    }

    #[test]
    fn platform_manifest_paths_are_stable() {
        assert_eq!(
            windows_chrome_native_host_manifest_path(Path::new(r"C:\Users\me\.pedelec")),
            PathBuf::from(r"C:\Users\me\.pedelec").join(MANIFEST_FILE_NAME)
        );
        assert_eq!(
            macos_chrome_native_host_manifest_path(Path::new("/Users/me")),
            PathBuf::from("/Users/me")
                .join("Library")
                .join("Application Support")
                .join("Google")
                .join("Chrome")
                .join("NativeMessagingHosts")
                .join(MANIFEST_FILE_NAME)
        );
    }

    #[test]
    fn missing_native_host_binary_returns_error() {
        let temp = tempfile::tempdir().unwrap();
        let missing_host = temp
            .path()
            .join(crate::pedelec_paths::pedelec_native_host_binary_name());
        let manifest_path = temp.path().join(MANIFEST_FILE_NAME);

        let err = register_chrome_native_messaging_host_with_paths(
            CHROME_EXTENSION_ID,
            &missing_host,
            &manifest_path,
        )
        .unwrap_err();

        assert_eq!(err.code, error_codes::IPC_UNAVAILABLE);
        assert!(err.message.contains("pedelec-native-host binary"));
    }

    #[test]
    fn writes_native_host_manifest_when_binary_exists() {
        let temp = tempfile::tempdir().unwrap();
        let native_host = temp
            .path()
            .join(crate::pedelec_paths::pedelec_native_host_binary_name());
        let manifest_path = temp.path().join(MANIFEST_FILE_NAME);
        fs::write(&native_host, b"fake-native-host").unwrap();

        let manifest = build_native_host_manifest(CHROME_EXTENSION_ID, &native_host);
        write_native_host_manifest(&manifest_path, &manifest).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(
            written["allowed_origins"],
            json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")])
        );
    }
}
