use crate::pedelec_core::{error_codes, PedelecError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

pub const APP_LAUNCH_CONFIG_VERSION: u32 = 1;
pub const BACKGROUND_LAUNCH_ARG: &str = "--background";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppLaunchConfig {
    pub version: u32,
    pub executable_path: PathBuf,
    pub background_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryInstallStatus {
    Installed,
    Unchanged,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryInstallOutcome {
    pub path: PathBuf,
    pub status: BinaryInstallStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockedTargetPolicy {
    Fail,
    DeferOnWindowsExecutableLock,
}

pub fn pedelec_home_dir() -> Result<PathBuf, PedelecError> {
    dirs::home_dir()
        .map(|home| home.join(".pedelec"))
        .ok_or_else(|| {
            PedelecError::new(
                error_codes::IPC_UNAVAILABLE,
                "cannot resolve user home directory",
            )
        })
}

pub fn app_launch_config_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join("app-launch.json"))
}

pub fn write_app_launch_config_for_current_exe() -> Result<PathBuf, PedelecError> {
    let executable_path = env::current_exe().map_err(|err| {
        launch_config_error(
            "write_app_launch_config",
            "cannot resolve desktop executable",
            err.to_string(),
        )
    })?;
    write_app_launch_config(
        &app_launch_config_path()?,
        &AppLaunchConfig {
            version: APP_LAUNCH_CONFIG_VERSION,
            executable_path,
            background_args: vec![BACKGROUND_LAUNCH_ARG.to_string()],
        },
    )
}

pub fn write_app_launch_config(
    path: &Path,
    config: &AppLaunchConfig,
) -> Result<PathBuf, PedelecError> {
    let parent = path.parent().ok_or_else(|| {
        launch_config_error(
            "write_app_launch_config",
            "launch config path has no parent",
            path.display().to_string(),
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        launch_config_error(
            "write_app_launch_config",
            "cannot create launch config directory",
            err.to_string(),
        )
    })?;
    let payload = serde_json::to_vec_pretty(config).map_err(|err| {
        launch_config_error(
            "write_app_launch_config",
            "cannot serialize launch config",
            err.to_string(),
        )
    })?;
    let temporary = parent.join(format!(
        ".app-launch-{}-{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::write(&temporary, payload).map_err(|err| {
        launch_config_error(
            "write_app_launch_config",
            "cannot write temporary launch config",
            err.to_string(),
        )
    })?;
    fs::rename(&temporary, path).map_err(|err| {
        let _ = fs::remove_file(&temporary);
        launch_config_error(
            "write_app_launch_config",
            "cannot replace launch config",
            err.to_string(),
        )
    })?;
    Ok(path.to_path_buf())
}

pub fn read_app_launch_config(path: &Path) -> Result<AppLaunchConfig, PedelecError> {
    let content = fs::read(path).map_err(|err| {
        launch_config_error(
            "load_app_launch_config",
            "cannot read launch config",
            err.to_string(),
        )
    })?;
    let config: AppLaunchConfig = serde_json::from_slice(&content).map_err(|err| {
        launch_config_error(
            "load_app_launch_config",
            "launch config is not valid JSON",
            err.to_string(),
        )
    })?;
    validate_app_launch_config(&config)?;
    Ok(config)
}

pub fn validate_app_launch_config(config: &AppLaunchConfig) -> Result<(), PedelecError> {
    if config.version != APP_LAUNCH_CONFIG_VERSION {
        return Err(launch_config_error(
            "validate_app_launch_config",
            "unsupported launch config version",
            config.version.to_string(),
        ));
    }
    if !config.executable_path.is_absolute() || !config.executable_path.is_file() {
        return Err(launch_config_error(
            "validate_app_launch_config",
            "desktop executable path is not an absolute existing file",
            config.executable_path.display().to_string(),
        ));
    }
    if config.background_args != vec![BACKGROUND_LAUNCH_ARG.to_string()] {
        return Err(launch_config_error(
            "validate_app_launch_config",
            "background args are not supported",
            format!("{:?}", config.background_args),
        ));
    }
    Ok(())
}

fn launch_config_error(
    stage: &str,
    reason: impl Into<String>,
    detail: impl Into<String>,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        "pedelec-app is not running",
        serde_json::json!({
            "stage": stage,
            "reason": reason.into(),
            "detail": detail.into(),
        }),
    )
}

pub fn pedelec_tool_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-cli.exe"
    } else {
        "pedelec-cli"
    }
}

pub fn pedelec_tool_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_tool_binary_name()))
}

pub fn pedelec_agent_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-agent.exe"
    } else {
        "pedelec-agent"
    }
}

pub fn pedelec_agent_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_agent_binary_name()))
}

pub fn pedelec_native_host_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-native-host.exe"
    } else {
        "pedelec-native-host"
    }
}

pub fn pedelec_native_host_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_native_host_binary_name()))
}

pub fn install_pedelec_tool_from_path(
    source: impl AsRef<Path>,
) -> Result<BinaryInstallOutcome, PedelecError> {
    install_binary_from_paths(
        source,
        pedelec_tool_install_path()?,
        "pedelec-cli",
        "cannot install pedelec-cli binary",
        LockedTargetPolicy::Fail,
    )
}

pub fn install_pedelec_tool_from_paths(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<BinaryInstallOutcome, PedelecError> {
    install_binary_from_paths(
        source,
        target,
        "pedelec-cli",
        "cannot install pedelec-cli binary",
        LockedTargetPolicy::Fail,
    )
}

pub fn install_pedelec_agent_from_path(
    source: impl AsRef<Path>,
) -> Result<BinaryInstallOutcome, PedelecError> {
    install_binary_from_paths(
        source,
        pedelec_agent_install_path()?,
        "pedelec-agent",
        "cannot install pedelec-agent binary",
        LockedTargetPolicy::Fail,
    )
}

pub fn install_pedelec_native_host_from_path(
    source: impl AsRef<Path>,
) -> Result<BinaryInstallOutcome, PedelecError> {
    install_binary_from_paths(
        source,
        pedelec_native_host_install_path()?,
        "pedelec-native-host",
        "cannot install pedelec-native-host binary",
        LockedTargetPolicy::DeferOnWindowsExecutableLock,
    )
}

fn install_binary_from_paths(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    binary_label: &str,
    install_error_message: &str,
    locked_target_policy: LockedTargetPolicy,
) -> Result<BinaryInstallOutcome, PedelecError> {
    install_binary_from_paths_with_copy(
        source,
        target,
        binary_label,
        install_error_message,
        locked_target_policy,
        |source, target| fs::copy(source, target),
    )
}

fn install_binary_from_paths_with_copy<F>(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    binary_label: &str,
    install_error_message: &str,
    locked_target_policy: LockedTargetPolicy,
    copy: F,
) -> Result<BinaryInstallOutcome, PedelecError>
where
    F: for<'a> FnOnce(&'a Path, &'a Path) -> io::Result<u64>,
{
    let source = source.as_ref();
    let target = target.as_ref();

    if !source.exists() {
        return Err(PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            format!("{binary_label} binary was not found in Tauri resources"),
            serde_json::json!({ "path": source.to_string_lossy() }),
        ));
    }

    if source == target {
        return Ok(BinaryInstallOutcome {
            path: target.to_path_buf(),
            status: BinaryInstallStatus::Unchanged,
        });
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot create .pedelec directory",
                serde_json::json!({
                    "path": parent.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })?;
    }

    let target_existed = target.exists();
    if target_existed
        && files_have_same_content(source, target).map_err(|err| {
            binary_install_error(install_error_message, source, target, "hash_binary", &err)
        })?
    {
        return Ok(BinaryInstallOutcome {
            path: target.to_path_buf(),
            status: BinaryInstallStatus::Unchanged,
        });
    }

    if let Err(err) = copy(source, target) {
        if should_defer_locked_target(
            locked_target_policy,
            target_existed,
            err.raw_os_error(),
            cfg!(windows),
        ) {
            #[cfg(debug_assertions)]
            eprintln!("pedelec-native-host update deferred because installed executable is in use");
            return Ok(BinaryInstallOutcome {
                path: target.to_path_buf(),
                status: BinaryInstallStatus::Deferred,
            });
        }
        return Err(binary_install_error(
            install_error_message,
            source,
            target,
            "copy_binary",
            &err,
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(target)
            .map_err(|err| {
                PedelecError::with_details(
                    error_codes::IPC_UNAVAILABLE,
                    format!("cannot read installed {binary_label} metadata"),
                    serde_json::json!({
                        "path": target.to_string_lossy(),
                        "error": err.to_string()
                    }),
                )
            })?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(target, permissions).map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                format!("cannot mark installed {binary_label} executable"),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "error": err.to_string()
                }),
            )
        })?;
    }

    Ok(BinaryInstallOutcome {
        path: target.to_path_buf(),
        status: BinaryInstallStatus::Installed,
    })
}

fn files_have_same_content(source: &Path, target: &Path) -> io::Result<bool> {
    if fs::metadata(source)?.len() != fs::metadata(target)?.len() {
        return Ok(false);
    }
    Ok(sha256_file(source)? == sha256_file(target)?)
}

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

fn binary_install_error(
    message: &str,
    source: &Path,
    target: &Path,
    stage: &str,
    err: &io::Error,
) -> PedelecError {
    PedelecError::with_details(
        error_codes::IPC_UNAVAILABLE,
        message,
        serde_json::json!({
            "stage": stage,
            "source": source.to_string_lossy(),
            "target": target.to_string_lossy(),
            "error": err.to_string(),
            "rawOsError": err.raw_os_error(),
        }),
    )
}

fn should_defer_locked_target(
    policy: LockedTargetPolicy,
    target_existed: bool,
    raw_os_error: Option<i32>,
    is_windows: bool,
) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    policy == LockedTargetPolicy::DeferOnWindowsExecutableLock
        && target_existed
        && is_windows
        && raw_os_error == Some(ERROR_SHARING_VIOLATION)
}

pub fn path_value_with_pedelec_dir(
    existing_path: Option<&OsStr>,
    pedelec_dir: &Path,
) -> Result<OsString, PedelecError> {
    if let Some(existing_path) = existing_path {
        if path_contains_dir(existing_path, pedelec_dir) {
            return Ok(existing_path.to_os_string());
        }
    }

    let mut paths = vec![pedelec_dir.to_path_buf()];
    if let Some(existing_path) = existing_path {
        paths.extend(env::split_paths(existing_path));
    }

    env::join_paths(paths).map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot update PATH for pedelec-cli",
            serde_json::json!({ "error": err.to_string() }),
        )
    })
}

pub fn path_value_with_default_pedelec_dir() -> Result<OsString, PedelecError> {
    let pedelec_dir = pedelec_home_dir()?;
    path_value_with_pedelec_dir(env::var_os("PATH").as_deref(), &pedelec_dir)
}

pub fn prepend_pedelec_dir_to_process_path() -> Result<(), PedelecError> {
    let path = path_value_with_default_pedelec_dir()?;
    env::set_var("PATH", path);
    Ok(())
}

#[cfg(windows)]
pub fn ensure_user_path_contains_pedelec_dir() -> Result<(), PedelecError> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let pedelec_dir = pedelec_home_dir()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (environment, _) = hkcu.create_subkey("Environment").map_err(|err| {
        PedelecError::with_details(
            error_codes::IPC_UNAVAILABLE,
            "cannot open user environment registry key",
            serde_json::json!({ "error": err.to_string() }),
        )
    })?;
    let existing = environment.get_value::<String, _>("Path").ok();
    let existing_os = existing.as_deref().map(OsStr::new);
    if existing_os
        .as_ref()
        .is_some_and(|path| path_contains_dir(path, &pedelec_dir))
    {
        return Ok(());
    }

    let updated = path_value_with_pedelec_dir(existing_os, &pedelec_dir)?;
    environment
        .set_value("Path", &updated.to_string_lossy().to_string())
        .map_err(|err| {
            PedelecError::with_details(
                error_codes::IPC_UNAVAILABLE,
                "cannot update user PATH registry value",
                serde_json::json!({ "error": err.to_string() }),
            )
        })
}

#[cfg(not(windows))]
pub fn ensure_user_path_contains_pedelec_dir() -> Result<(), PedelecError> {
    Ok(())
}

fn path_contains_dir(path_value: &OsStr, dir: &Path) -> bool {
    env::split_paths(path_value).any(|entry| paths_match(&entry, dir))
}

#[cfg(windows)]
fn paths_match(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['\\', '/']))
}

#[cfg(not(windows))]
fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::ffi::OsString;

    #[test]
    fn install_pedelec_tool_copies_sibling_binary_to_target() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("bin");
        let target_dir = temp.path().join(".pedelec");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join(pedelec_tool_binary_name());
        let target = target_dir.join(pedelec_tool_binary_name());
        fs::write(&source, b"fake-tool").unwrap();

        let installed = install_pedelec_tool_from_paths(&source, &target).unwrap();

        assert_eq!(installed.path, target);
        assert_eq!(installed.status, BinaryInstallStatus::Installed);
        assert_eq!(fs::read(&installed.path).unwrap(), b"fake-tool");
    }

    #[test]
    fn unchanged_binary_does_not_invoke_copy() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"same-content").unwrap();
        fs::write(&target, b"same-content").unwrap();
        let calls = Cell::new(0);

        let outcome = install_binary_from_paths_with_copy(
            &source,
            &target,
            "test",
            "cannot install test binary",
            LockedTargetPolicy::Fail,
            |_, _| {
                calls.set(calls.get() + 1);
                Ok(0)
            },
        )
        .unwrap();

        assert_eq!(outcome.status, BinaryInstallStatus::Unchanged);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn different_binary_content_is_updated_for_size_or_hash_difference() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"new-content").unwrap();
        fs::write(&target, b"old").unwrap();
        let size_outcome = install_binary_from_paths(
            &source,
            &target,
            "test",
            "cannot install test binary",
            LockedTargetPolicy::Fail,
        )
        .unwrap();
        assert_eq!(size_outcome.status, BinaryInstallStatus::Installed);
        assert_eq!(fs::read(&target).unwrap(), b"new-content");

        fs::write(&source, b"abcdef").unwrap();
        fs::write(&target, b"ghijkl").unwrap();
        let hash_outcome = install_binary_from_paths(
            &source,
            &target,
            "test",
            "cannot install test binary",
            LockedTargetPolicy::Fail,
        )
        .unwrap();
        assert_eq!(hash_outcome.status, BinaryInstallStatus::Installed);
        assert_eq!(fs::read(&target).unwrap(), b"abcdef");
    }

    #[test]
    fn hash_failure_does_not_try_to_copy_over_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target-directory");
        fs::create_dir(&target).unwrap();
        fs::write(
            &source,
            vec![0_u8; fs::metadata(&target).unwrap().len() as usize],
        )
        .unwrap();
        let calls = Cell::new(0);

        let err = install_binary_from_paths_with_copy(
            &source,
            &target,
            "test",
            "cannot install test binary",
            LockedTargetPolicy::Fail,
            |_, _| {
                calls.set(calls.get() + 1);
                Ok(0)
            },
        )
        .unwrap_err();

        assert_eq!(calls.get(), 0);
        assert!(target.is_dir());
        assert_eq!(err.details.unwrap()["stage"], "hash_binary");
    }

    #[test]
    fn only_windows_sharing_violation_for_native_host_may_be_deferred() {
        assert!(should_defer_locked_target(
            LockedTargetPolicy::DeferOnWindowsExecutableLock,
            true,
            Some(32),
            true,
        ));
        assert!(!should_defer_locked_target(
            LockedTargetPolicy::DeferOnWindowsExecutableLock,
            true,
            Some(5),
            true,
        ));
        assert!(!should_defer_locked_target(
            LockedTargetPolicy::DeferOnWindowsExecutableLock,
            false,
            Some(32),
            true,
        ));
        assert!(!should_defer_locked_target(
            LockedTargetPolicy::Fail,
            true,
            Some(32),
            true,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn native_host_sharing_violation_defers_without_rewriting_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"new-native-host").unwrap();
        fs::write(&target, b"old-native-host").unwrap();

        let outcome = install_binary_from_paths_with_copy(
            &source,
            &target,
            "pedelec-native-host",
            "cannot install pedelec-native-host binary",
            LockedTargetPolicy::DeferOnWindowsExecutableLock,
            |_, _| Err(io::Error::from_raw_os_error(32)),
        )
        .unwrap();

        assert_eq!(outcome.status, BinaryInstallStatus::Deferred);
        assert_eq!(fs::read(&target).unwrap(), b"old-native-host");
    }

    #[cfg(windows)]
    #[test]
    fn non_native_host_sharing_violation_remains_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"new-tool").unwrap();
        fs::write(&target, b"old-tool").unwrap();

        let err = install_binary_from_paths_with_copy(
            &source,
            &target,
            "pedelec-cli",
            "cannot install pedelec-cli binary",
            LockedTargetPolicy::Fail,
            |_, _| Err(io::Error::from_raw_os_error(32)),
        )
        .unwrap_err();

        assert_eq!(err.details.unwrap()["stage"], "copy_binary");
        assert_eq!(fs::read(&target).unwrap(), b"old-tool");
    }

    #[test]
    fn path_value_prepends_pedelec_dir_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let pedelec_dir = temp.path().join(".pedelec");
        let other_dir = temp.path().join("other");
        let existing = env::join_paths([other_dir.clone()]).unwrap();

        let updated =
            path_value_with_pedelec_dir(Some(existing.as_os_str()), &pedelec_dir).unwrap();
        let paths = env::split_paths(&updated).collect::<Vec<_>>();

        assert_eq!(paths[0], pedelec_dir);
        assert_eq!(paths[1], other_dir);
    }

    #[test]
    fn path_value_does_not_duplicate_existing_pedelec_dir() {
        let temp = tempfile::tempdir().unwrap();
        let pedelec_dir = temp.path().join(".pedelec");
        let existing = env::join_paths([pedelec_dir.clone()]).unwrap();

        let updated =
            path_value_with_pedelec_dir(Some(existing.as_os_str()), &pedelec_dir).unwrap();
        let paths = env::split_paths(&updated).collect::<Vec<_>>();

        assert_eq!(paths, vec![pedelec_dir]);
    }

    #[test]
    fn path_value_handles_empty_path() {
        let temp = tempfile::tempdir().unwrap();
        let pedelec_dir = temp.path().join(".pedelec");

        let updated = path_value_with_pedelec_dir(None, &pedelec_dir).unwrap();

        assert_eq!(updated, OsString::from(pedelec_dir));
    }

    #[test]
    fn app_launch_config_round_trips_through_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("pedelec-app");
        fs::write(&executable, b"app").unwrap();
        let path = temp.path().join(".pedelec").join("app-launch.json");
        let config = AppLaunchConfig {
            version: APP_LAUNCH_CONFIG_VERSION,
            executable_path: executable,
            background_args: vec![BACKGROUND_LAUNCH_ARG.into()],
        };

        write_app_launch_config(&path, &config).unwrap();

        assert_eq!(read_app_launch_config(&path).unwrap(), config);
        assert!(!path.parent().unwrap().join(".app-launch.tmp").exists());
    }

    #[test]
    fn app_launch_config_rejects_invalid_version_relative_or_missing_executable() {
        let temp = tempfile::tempdir().unwrap();
        let invalid_version = AppLaunchConfig {
            version: 2,
            executable_path: temp.path().join("missing"),
            background_args: vec![BACKGROUND_LAUNCH_ARG.into()],
        };
        assert!(validate_app_launch_config(&invalid_version).is_err());

        let relative = AppLaunchConfig {
            version: APP_LAUNCH_CONFIG_VERSION,
            executable_path: PathBuf::from("pedelec-app"),
            background_args: vec![BACKGROUND_LAUNCH_ARG.into()],
        };
        assert!(validate_app_launch_config(&relative).is_err());
    }

    #[test]
    fn corrupt_app_launch_config_has_a_diagnostic_stage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app-launch.json");
        fs::write(&path, b"not json").unwrap();

        let err = read_app_launch_config(&path).unwrap_err();

        assert_eq!(err.code, error_codes::CORE_RUNTIME_UNAVAILABLE);
        assert_eq!(err.details.unwrap()["stage"], "load_app_launch_config");
    }
}
