use crate::error::{error_codes, PedelecError};
use serde::{Deserialize, Serialize};
use std::{env, ffi::OsString, fs, path::{Path, PathBuf}};

pub const APP_LAUNCH_CONFIG_VERSION: u32 = 1;
pub const BACKGROUND_LAUNCH_ARG: &str = "--background";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppLaunchConfig { pub version: u32, pub executable_path: PathBuf, pub background_args: Vec<String> }

pub fn pedelec_home_dir() -> Result<PathBuf, PedelecError> { dirs::home_dir().map(|home| home.join(".pedelec")).ok_or_else(|| PedelecError::new(error_codes::IPC_UNAVAILABLE, "cannot resolve user home directory")) }
pub fn app_launch_config_path() -> Result<PathBuf, PedelecError> { Ok(pedelec_home_dir()?.join("app-launch.json")) }
pub fn pedelec_tool_binary_name() -> &'static str { if cfg!(windows) { "pedelec-cli.exe" } else { "pedelec-cli" } }
pub fn pedelec_agent_binary_name() -> &'static str { if cfg!(windows) { "pedelec-agent.exe" } else { "pedelec-agent" } }
pub fn pedelec_native_host_binary_name() -> &'static str { if cfg!(windows) { "pedelec-native-host.exe" } else { "pedelec-native-host" } }
pub fn pedelec_tool_install_path() -> Result<PathBuf, PedelecError> { Ok(pedelec_home_dir()?.join(pedelec_tool_binary_name())) }
pub fn pedelec_agent_install_path() -> Result<PathBuf, PedelecError> { Ok(pedelec_home_dir()?.join(pedelec_agent_binary_name())) }
pub fn pedelec_native_host_install_path() -> Result<PathBuf, PedelecError> { Ok(pedelec_home_dir()?.join(pedelec_native_host_binary_name())) }
pub fn path_value_with_default_pedelec_dir() -> Result<OsString, PedelecError> { let home = pedelec_home_dir()?; let current = env::var_os("PATH"); let mut paths = current.as_deref().map(env::split_paths).map(Iterator::collect::<Vec<_>>).unwrap_or_default(); if !paths.iter().any(|path| path == &home) { paths.insert(0, home); } env::join_paths(paths).map_err(|err| PedelecError::new(error_codes::IPC_UNAVAILABLE, err.to_string())) }
pub fn write_app_launch_config_for_current_exe() -> Result<PathBuf, PedelecError> { let executable_path = env::current_exe().map_err(|err| launch_error("cannot resolve desktop executable", err.to_string()))?; write_app_launch_config(&app_launch_config_path()?, &AppLaunchConfig { version: APP_LAUNCH_CONFIG_VERSION, executable_path, background_args: vec![BACKGROUND_LAUNCH_ARG.into()] }) }
pub fn write_app_launch_config(path: &Path, config: &AppLaunchConfig) -> Result<PathBuf, PedelecError> { let parent = path.parent().ok_or_else(|| launch_error("launch config path has no parent", path.display().to_string()))?; fs::create_dir_all(parent).map_err(|err| launch_error("cannot create launch config directory", err.to_string()))?; let payload = serde_json::to_vec_pretty(config).map_err(|err| launch_error("cannot serialize launch config", err.to_string()))?; fs::write(path, payload).map_err(|err| launch_error("cannot write launch config", err.to_string()))?; Ok(path.to_path_buf()) }
pub fn read_app_launch_config(path: &Path) -> Result<AppLaunchConfig, PedelecError> { let config = serde_json::from_slice(&fs::read(path).map_err(|err| launch_error("cannot read launch config", err.to_string()))?).map_err(|err| launch_error("launch config is not valid JSON", err.to_string()))?; validate_app_launch_config(&config)?; Ok(config) }
pub fn validate_app_launch_config(config: &AppLaunchConfig) -> Result<(), PedelecError> { if config.version != APP_LAUNCH_CONFIG_VERSION || !config.executable_path.is_absolute() || !config.executable_path.is_file() || config.background_args != vec![BACKGROUND_LAUNCH_ARG.to_string()] { return Err(launch_error("invalid launch config", config.executable_path.display().to_string())); } Ok(()) }
fn launch_error(reason: impl Into<String>, detail: impl Into<String>) -> PedelecError { PedelecError::with_details(error_codes::CORE_RUNTIME_UNAVAILABLE, "pedelec-app is not running", serde_json::json!({ "reason": reason.into(), "detail": detail.into() })) }
