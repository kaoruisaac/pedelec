use crate::pedelec_core::{
    refresh_shared_providers, CheckOllamaConnectionInput, CheckOllamaConnectionOutput,
    CoreRuntimeOwner, CreateThreadInput, CreateThreadOutput, EndThreadInput, ListOllamaModelsInput,
    OllamaModelOption, PedelecError, PedelecSettings, PrepareThreadInput, PrepareThreadOutput,
    ProviderInfo, SendTextInput, SendTextOutput, SharedCoreRuntime, SubmitToolResultInput,
    UpdateSettingsInput,
};
use crate::pedelec_ipc::{prepare_provider_process, start_core_ipc_server, start_provider_process};
use crate::pedelec_native_registration::register_chrome_native_messaging_host;
use crate::pedelec_paths::{
    ensure_user_path_contains_pedelec_dir, install_pedelec_agent_from_path,
    install_pedelec_native_host_from_path, install_pedelec_tool_from_path,
    pedelec_agent_binary_name, pedelec_native_host_binary_name, pedelec_tool_binary_name,
    prepend_pedelec_dir_to_process_path, write_app_launch_config_for_current_exe,
    BinaryInstallOutcome,
};
use crate::pedelec_upload::start_asset_upload_server;
use crate::provider_installer::{
    open as open_installer, OpenProviderInstallerInput, OpenProviderInstallerOutput,
};
use crate::provider_terminal::{
    open as open_provider_terminal_window, OpenProviderTerminalInput, OpenProviderTerminalOutput,
};
use std::path::PathBuf;
use std::thread;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Emitter, Manager, RunEvent, State};

const MAIN_WINDOW_LABEL: &str = "main";

pub fn run() {
    let background_launch = is_background_launch(std::env::args_os());
    let runtime_owner = CoreRuntimeOwner::new();
    let runtime = runtime_owner.runtime();
    let runtime_for_setup = runtime.clone();
    let runtime_for_exit = runtime.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if should_show_window_for_second_instance(args) {
                show_main_window(app);
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(runtime_owner)
        .invoke_handler(tauri::generate_handler![
            create_thread,
            get_settings,
            update_settings,
            check_ollama_connection,
            list_ollama_models,
            list_providers,
            refresh_providers,
            open_provider_installer,
            open_provider_terminal,
            restart_app,
            send_text,
            prepare_thread,
            submit_tool_result,
            end_thread
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if background_launch {
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            write_app_launch_config_for_current_exe().map_err(|err| {
                tauri::Error::from(std::io::Error::other(format!(
                    "cannot write desktop launch config: {}",
                    err.message
                )))
            })?;
            let pedelec_tool_source = bundled_binary_path(app, pedelec_tool_binary_name())?;
            let pedelec_agent_source = bundled_binary_path(app, pedelec_agent_binary_name())?;
            let pedelec_tool_outcome = install_pedelec_tool_from_path(&pedelec_tool_source)
                .map_err(|err| {
                    tauri::Error::from(std::io::Error::other(format!(
                        "cannot install pedelec-cli: {}",
                        err.message
                    )))
                })?;
            let pedelec_agent_outcome = install_pedelec_agent_from_path(&pedelec_agent_source)
                .map_err(|err| {
                    tauri::Error::from(std::io::Error::other(format!(
                        "cannot install pedelec-agent: {}",
                        err.message
                    )))
                })?;
            let native_host_outcome = if native_messaging_plan(background_launch).install {
                let native_host_source =
                    bundled_binary_path(app, pedelec_native_host_binary_name())?;
                Some(
                    install_pedelec_native_host_from_path(&native_host_source).map_err(|err| {
                        tauri::Error::from(std::io::Error::other(format!(
                            "cannot install pedelec-native-host: {}",
                            err.message
                        )))
                    })?,
                )
            } else {
                None
            };
            prepend_pedelec_dir_to_process_path().map_err(|err| {
                tauri::Error::from(std::io::Error::other(format!(
                    "cannot update app PATH for pedelec-cli: {}",
                    err.message
                )))
            })?;
            ensure_user_path_contains_pedelec_dir().map_err(|err| {
                tauri::Error::from(std::io::Error::other(format!(
                    "cannot update user PATH for pedelec-cli: {}",
                    err.message
                )))
            })?;
            // A failed data plane must not prevent the desktop/control plane from starting.
            let _asset_upload_server = start_asset_upload_server(runtime_for_setup.clone());
            let _ipc_handle = start_core_ipc_server(runtime_for_setup.clone()).map_err(|err| {
                tauri::Error::from(std::io::Error::other(format!(
                    "cannot start Core IPC server: {}",
                    err.message
                )))
            })?;
            forward_thread_events_to_tauri(app.handle().clone(), runtime_for_setup.clone());
            #[cfg(debug_assertions)]
            eprintln!(
                "Core IPC listening at {} (runtime: {})",
                _ipc_handle.runtime_file.endpoint,
                _ipc_handle.runtime_file_path.to_string_lossy()
            );
            #[cfg(debug_assertions)]
            eprintln!(
                "pedelec-cli installed at {}",
                install_outcome_message(&pedelec_tool_outcome)
            );
            #[cfg(debug_assertions)]
            eprintln!(
                "pedelec-agent installed at {}",
                install_outcome_message(&pedelec_agent_outcome)
            );
            if let Some(native_host_outcome) = &native_host_outcome {
                #[cfg(debug_assertions)]
                eprintln!(
                    "pedelec-native-host {}",
                    install_outcome_message(native_host_outcome)
                );
            }
            if native_messaging_plan(background_launch).register {
                match register_chrome_native_messaging_host() {
                    Ok(_registration) => {
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "Chrome native messaging host {} registered (manifest: {}, binary: {})",
                            _registration.host_name,
                            _registration.manifest_path.to_string_lossy(),
                            _registration.native_host_path.to_string_lossy()
                        );
                    }
                    Err(_err) => {
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "Chrome native messaging auto-registration skipped: {}",
                            _err.message
                        );
                    }
                }
            }

            let show = MenuItem::with_id(app, "show", "Show Window", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            let icon = app
                .default_window_icon()
                .ok_or_else(|| tauri::Error::from(std::io::Error::other("missing app icon")))?;

            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(icon.clone())
                .menu(&menu)
                .tooltip("Pedelec")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            if !background_launch {
                show_main_window(app.handle());
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_, event| {
            if let RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                } else {
                    let _errors = runtime_for_exit.lock().unwrap().cleanup_for_app_exit();
                    #[cfg(debug_assertions)]
                    for err in _errors {
                        eprintln!("sandbox cleanup failed during app exit: {}", err.message);
                    }
                }
            }
        });
}

fn bundled_binary_path(app: &App, binary_name: &str) -> Result<PathBuf, tauri::Error> {
    app.path()
        .resolve(format!("binaries/{binary_name}"), BaseDirectory::Resource)
}

fn show_main_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn is_background_launch(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter().any(|arg| arg == "--background")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeMessagingPlan {
    install: bool,
    register: bool,
}

fn native_messaging_plan(background_launch: bool) -> NativeMessagingPlan {
    NativeMessagingPlan {
        install: !background_launch,
        register: !background_launch,
    }
}

fn install_outcome_message(outcome: &BinaryInstallOutcome) -> String {
    format!("{:?} at {}", outcome.status, outcome.path.to_string_lossy())
}

fn should_show_window_for_second_instance(args: Vec<String>) -> bool {
    !args.iter().any(|arg| arg == "--background")
}

#[cfg(test)]
mod launch_mode_tests {
    use super::*;

    #[test]
    fn background_launch_requires_the_exact_argument() {
        assert!(!is_background_launch([std::ffi::OsString::from(
            "pedelec-app"
        )]));
        assert!(is_background_launch([
            std::ffi::OsString::from("pedelec-app"),
            std::ffi::OsString::from("--background"),
        ]));
        assert!(!is_background_launch([std::ffi::OsString::from(
            "--background=true"
        )]));
    }

    #[test]
    fn second_background_instance_does_not_show_the_window() {
        assert!(!should_show_window_for_second_instance(vec![
            "--background".into()
        ]));
        assert!(should_show_window_for_second_instance(
            vec!["--open".into()]
        ));
    }

    #[test]
    fn background_launch_skips_only_native_messaging_initialization() {
        assert_eq!(
            native_messaging_plan(false),
            NativeMessagingPlan {
                install: true,
                register: true,
            }
        );
        assert_eq!(
            native_messaging_plan(true),
            NativeMessagingPlan {
                install: false,
                register: false,
            }
        );
    }
}

#[tauri::command]
fn create_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: CreateThreadInput,
) -> Result<CreateThreadOutput, PedelecError> {
    state.runtime().lock().unwrap().create_thread(input)
}

#[tauri::command]
fn get_settings(state: State<'_, CoreRuntimeOwner>) -> Result<PedelecSettings, PedelecError> {
    state.runtime().lock().unwrap().get_settings()
}

#[tauri::command]
fn update_settings(
    state: State<'_, CoreRuntimeOwner>,
    input: UpdateSettingsInput,
) -> Result<PedelecSettings, PedelecError> {
    state.runtime().lock().unwrap().update_settings(input)
}

#[tauri::command]
fn list_providers(state: State<'_, CoreRuntimeOwner>) -> Vec<ProviderInfo> {
    state.runtime().lock().unwrap().list_providers()
}

#[tauri::command]
fn open_provider_installer(
    input: OpenProviderInstallerInput,
) -> Result<OpenProviderInstallerOutput, PedelecError> {
    open_installer(input)
}

#[tauri::command]
fn open_provider_terminal(
    state: State<'_, CoreRuntimeOwner>,
    input: OpenProviderTerminalInput,
) -> Result<OpenProviderTerminalOutput, PedelecError> {
    let executable = state
        .runtime()
        .lock()
        .unwrap()
        .provider_executable_path(&input.provider)?;
    open_provider_terminal_window(input.provider, executable)
}

#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.request_restart();
}

#[tauri::command]
async fn refresh_providers(
    state: State<'_, CoreRuntimeOwner>,
) -> Result<Vec<ProviderInfo>, PedelecError> {
    let shared_runtime = state.runtime();
    let fallback_runtime = shared_runtime.clone();
    Ok(
        tauri::async_runtime::spawn_blocking(move || refresh_shared_providers(&shared_runtime))
            .await
            .unwrap_or_else(|_| fallback_runtime.lock().unwrap().list_providers()),
    )
}

#[tauri::command]
fn list_ollama_models(
    state: State<'_, CoreRuntimeOwner>,
    input: ListOllamaModelsInput,
) -> Result<Vec<OllamaModelOption>, PedelecError> {
    state.runtime().lock().unwrap().list_ollama_models(input)
}

#[tauri::command]
fn check_ollama_connection(
    state: State<'_, CoreRuntimeOwner>,
    input: CheckOllamaConnectionInput,
) -> CheckOllamaConnectionOutput {
    state
        .runtime()
        .lock()
        .unwrap()
        .check_ollama_connection(input)
}

#[tauri::command]
fn send_text(
    state: State<'_, CoreRuntimeOwner>,
    input: SendTextInput,
) -> Result<SendTextOutput, PedelecError> {
    start_provider_process(state.runtime(), input)
}

#[tauri::command]
fn prepare_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: PrepareThreadInput,
) -> Result<PrepareThreadOutput, PedelecError> {
    prepare_provider_process(state.runtime(), input)
}

#[tauri::command]
fn submit_tool_result(
    state: State<'_, CoreRuntimeOwner>,
    input: SubmitToolResultInput,
) -> Result<(), PedelecError> {
    state.runtime().lock().unwrap().submit_tool_result(input)
}

#[tauri::command]
fn end_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: EndThreadInput,
) -> Result<(), PedelecError> {
    state.runtime().lock().unwrap().end_thread(input)
}

fn forward_thread_events_to_tauri(app: tauri::AppHandle, runtime: SharedCoreRuntime) {
    let event_rx = runtime.lock().unwrap().subscribe_all_threads();
    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            let _ = app.emit("thread_event", event);
        }
    });
}
