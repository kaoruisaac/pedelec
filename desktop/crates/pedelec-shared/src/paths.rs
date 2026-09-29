use crate::error::{error_codes, PedelecError};
use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::path::{Component, Prefix};

pub const APP_LAUNCH_CONFIG_VERSION: u32 = 1;
pub const BACKGROUND_LAUNCH_ARG: &str = "--background";

/// Formats an authoritative filesystem path for provider, UI, and diagnostic
/// representations without changing the path used for filesystem operations.
pub fn path_for_external_use(path: &Path) -> String {
    #[cfg(windows)]
    {
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return path.to_string_lossy().into_owned();
        };

        let rest = components.as_path();
        let external = match prefix.kind() {
            Prefix::VerbatimDisk(disk) => {
                let mut external = OsString::new();
                external.push(format!("{}:", disk as char));
                external.push(rest.as_os_str());
                external
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut external = OsString::from(r"\\");
                external.push(server);
                external.push(r"\");
                external.push(share);
                external.push(rest.as_os_str());
                external
            }
            _ => return path.to_string_lossy().into_owned(),
        };

        return external.to_string_lossy().into_owned();
    }

    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppLaunchConfig {
    pub version: u32,
    pub executable_path: PathBuf,
    pub background_args: Vec<String>,
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
pub fn pedelec_tool_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-cli.exe"
    } else {
        "pedelec-cli"
    }
}
pub fn pedelec_deno_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-deno.exe"
    } else {
        "pedelec-deno"
    }
}
pub fn pedelec_agent_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-agent.exe"
    } else {
        "pedelec-agent"
    }
}
pub fn pedelec_native_host_binary_name() -> &'static str {
    if cfg!(windows) {
        "pedelec-native-host.exe"
    } else {
        "pedelec-native-host"
    }
}
/// File name of the managed Deno executable for this host.
/// This is intentionally distinct from the public `pedelec-deno` helper.
pub fn deno_executable_file_name() -> &'static str {
    if cfg!(windows) {
        "deno.exe"
    } else {
        "deno"
    }
}

pub fn managed_deno_runtimes_root(pedelec_home: &Path) -> PathBuf {
    pedelec_home.join("runtimes").join("deno")
}

pub fn managed_deno_runtime_dir(
    pedelec_home: &Path,
    version: &str,
    target: &str,
) -> Result<PathBuf, PedelecError> {
    validate_runtime_component("version", version)?;
    validate_runtime_component("target", target)?;
    Ok(managed_deno_runtimes_root(pedelec_home)
        .join(version)
        .join(target))
}

pub fn managed_deno_executable_path(
    pedelec_home: &Path,
    version: &str,
    target: &str,
    executable_name: &str,
) -> Result<PathBuf, PedelecError> {
    validate_executable_name(executable_name)?;
    Ok(managed_deno_runtime_dir(pedelec_home, version, target)?.join(executable_name))
}

/// Deno-owned cache for one managed runtime. This is a path only; callers create it.
pub fn managed_deno_cache_dir(
    pedelec_home: &Path,
    version: &str,
    target: &str,
) -> Result<PathBuf, PedelecError> {
    Ok(managed_deno_runtime_dir(pedelec_home, version, target)?.join("cache"))
}

/// Attempt-scoped downloads and extracts live here, never in a finalized version directory.
pub fn managed_deno_partial_dir(
    pedelec_home: &Path,
    version: &str,
    target: &str,
) -> Result<PathBuf, PedelecError> {
    validate_runtime_component("version", version)?;
    validate_runtime_component("target", target)?;
    Ok(managed_deno_runtimes_root(pedelec_home)
        .join(".partial")
        .join(version)
        .join(target))
}

fn validate_runtime_component(label: &str, value: &str) -> Result<(), PedelecError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0'])
        || value.starts_with('.')
    {
        return Err(PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            format!("invalid Deno runtime {label}"),
        ));
    }
    Ok(())
}

fn validate_executable_name(value: &str) -> Result<(), PedelecError> {
    if value != "deno" && value != "deno.exe" {
        return Err(PedelecError::new(
            error_codes::CORE_RUNTIME_UNAVAILABLE,
            "invalid Deno runtime executable name",
        ));
    }
    Ok(())
}
pub fn pedelec_tool_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_tool_binary_name()))
}
pub fn pedelec_deno_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_deno_binary_name()))
}
pub fn pedelec_agent_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_agent_binary_name()))
}
pub fn pedelec_native_host_install_path() -> Result<PathBuf, PedelecError> {
    Ok(pedelec_home_dir()?.join(pedelec_native_host_binary_name()))
}
pub fn path_value_with_default_pedelec_dir() -> Result<OsString, PedelecError> {
    let home = pedelec_home_dir()?;
    let current = env::var_os("PATH");
    let mut paths = current
        .as_deref()
        .map(env::split_paths)
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if !paths.iter().any(|path| path == &home) {
        paths.insert(0, home);
    }
    env::join_paths(paths)
        .map_err(|err| PedelecError::new(error_codes::IPC_UNAVAILABLE, err.to_string()))
}
pub fn write_app_launch_config_for_current_exe() -> Result<PathBuf, PedelecError> {
    let executable_path = env::current_exe()
        .map_err(|err| launch_error("cannot resolve desktop executable", err.to_string()))?;
    write_app_launch_config(
        &app_launch_config_path()?,
        &AppLaunchConfig {
            version: APP_LAUNCH_CONFIG_VERSION,
            executable_path,
            background_args: vec![BACKGROUND_LAUNCH_ARG.into()],
        },
    )
}
pub fn write_app_launch_config(
    path: &Path,
    config: &AppLaunchConfig,
) -> Result<PathBuf, PedelecError> {
    let parent = path.parent().ok_or_else(|| {
        launch_error(
            "launch config path has no parent",
            path.display().to_string(),
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| launch_error("cannot create launch config directory", err.to_string()))?;
    let payload = serde_json::to_vec_pretty(config)
        .map_err(|err| launch_error("cannot serialize launch config", err.to_string()))?;
    fs::write(path, payload)
        .map_err(|err| launch_error("cannot write launch config", err.to_string()))?;
    Ok(path.to_path_buf())
}
pub fn read_app_launch_config(path: &Path) -> Result<AppLaunchConfig, PedelecError> {
    let config = serde_json::from_slice(
        &fs::read(path)
            .map_err(|err| launch_error("cannot read launch config", err.to_string()))?,
    )
    .map_err(|err| launch_error("launch config is not valid JSON", err.to_string()))?;
    validate_app_launch_config(&config)?;
    Ok(config)
}
pub fn validate_app_launch_config(config: &AppLaunchConfig) -> Result<(), PedelecError> {
    if config.version != APP_LAUNCH_CONFIG_VERSION
        || !config.executable_path.is_absolute()
        || !config.executable_path.is_file()
        || config.background_args != vec![BACKGROUND_LAUNCH_ARG.to_string()]
    {
        return Err(launch_error(
            "invalid launch config",
            config.executable_path.display().to_string(),
        ));
    }
    Ok(())
}
fn launch_error(reason: impl Into<String>, detail: impl Into<String>) -> PedelecError {
    PedelecError::with_details(
        error_codes::CORE_RUNTIME_UNAVAILABLE,
        "pedelec-app is not running",
        serde_json::json!({ "reason": reason.into(), "detail": detail.into() }),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        deno_executable_file_name, managed_deno_cache_dir, managed_deno_executable_path,
        managed_deno_partial_dir, path_for_external_use,
    };
    use std::path::Path;

    #[test]
    fn managed_deno_executable_is_version_and_target_specific() {
        let home = Path::new("/home/user/.pedelec");
        let executable_name = deno_executable_file_name();
        let path = managed_deno_executable_path(
            home,
            "2.9.5",
            "x86_64-unknown-linux-gnu",
            executable_name,
        )
        .unwrap();
        assert_eq!(
            path,
            home.join("runtimes")
                .join("deno")
                .join("2.9.5")
                .join("x86_64-unknown-linux-gnu")
                .join(executable_name)
        );
        assert_ne!(executable_name, "pedelec-deno");
        assert_ne!(executable_name, "pedelec-deno.exe");
    }

    #[test]
    fn managed_deno_cache_is_version_and_target_specific() {
        let home = Path::new("/home/user/.pedelec");
        let workspace = Path::new("/home/user/project");
        let cache = managed_deno_cache_dir(home, "2.9.5", "x86_64-pc-windows-msvc").unwrap();
        let runtime_dir =
            super::managed_deno_runtime_dir(home, "2.9.5", "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(cache, runtime_dir.join("cache"));
        assert!(cache.starts_with(&runtime_dir));
        assert_ne!(
            managed_deno_cache_dir(home, "2.9.6", "x86_64-pc-windows-msvc").unwrap(),
            cache
        );
        assert_ne!(
            managed_deno_cache_dir(home, "2.9.5", "aarch64-apple-darwin").unwrap(),
            cache
        );
        assert!(!cache.starts_with(workspace));
        assert!(managed_deno_cache_dir(home, "..", "x86_64-pc-windows-msvc").is_err());
        assert!(managed_deno_cache_dir(home, "2.9.5", "a/b").is_err());
        assert!(managed_deno_cache_dir(home, ".2.9.5", "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn partial_runtime_files_stay_outside_the_final_directory() {
        let home = Path::new("/home/user/.pedelec");
        let partial = managed_deno_partial_dir(home, "2.9.5", "x86_64-pc-windows-msvc").unwrap();
        let final_dir =
            super::managed_deno_runtime_dir(home, "2.9.5", "x86_64-pc-windows-msvc").unwrap();
        assert!(partial
            .components()
            .any(|component| component.as_os_str() == ".partial"));
        assert!(!final_dir.starts_with(&partial));
        assert!(!partial.starts_with(&final_dir));
    }

    #[test]
    fn runtime_path_components_reject_traversal() {
        let home = Path::new("/home/user/.pedelec");
        assert!(
            managed_deno_executable_path(home, "..", "x86_64-pc-windows-msvc", "deno.exe").is_err()
        );
        assert!(managed_deno_executable_path(home, "2.9.5", "a/b", "deno.exe").is_err());
        assert!(managed_deno_executable_path(
            home,
            "2.9.5",
            "x86_64-pc-windows-msvc",
            "pedelec-deno"
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn externalizes_verbatim_drive_path() {
        assert_eq!(
            path_for_external_use(Path::new(r"\\?\C:\foo\bar")),
            r"C:\foo\bar"
        );
    }

    #[cfg(windows)]
    #[test]
    fn externalizes_verbatim_unc_path() {
        assert_eq!(
            path_for_external_use(Path::new(r"\\?\UNC\server\share\foo")),
            r"\\server\share\foo"
        );
    }

    #[cfg(windows)]
    #[test]
    fn externalizes_verbatim_drive_path_with_non_ascii_components() {
        assert_eq!(
            path_for_external_use(Path::new(r"\\?\C:\Users\kaoru\OneDrive\桌面\test")),
            r"C:\Users\kaoru\OneDrive\桌面\test"
        );
    }

    #[test]
    fn preserves_normal_path() {
        let path = if cfg!(windows) {
            Path::new(r"C:\foo\bar")
        } else {
            Path::new("/foo/bar")
        };
        assert_eq!(path_for_external_use(path), path.to_string_lossy());
    }

    #[cfg(windows)]
    #[test]
    fn preserves_normal_unc_path() {
        let path = Path::new(r"\\server\share\foo");
        assert_eq!(path_for_external_use(path), path.to_string_lossy());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_path_presentation_is_identity() {
        let path = Path::new("/tmp/桌面/test");
        assert_eq!(path_for_external_use(path), path.to_string_lossy());
    }
}
