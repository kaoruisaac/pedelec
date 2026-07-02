use crate::pedelec_core::{
    CheckOllamaConnectionInput, CheckOllamaConnectionOutput, CoreRuntimeOwner, CreateThreadInput,
    CreateThreadOutput, EndThreadInput, ListOllamaModelsInput, OllamaModelOption, PedelecError,
    PedelecSettings, ProviderInfo, SendTextInput, SendTextOutput, SharedCoreRuntime,
    SubmitToolResultInput, UpdateSettingsInput,
};
use crate::pedelec_ipc::{start_core_ipc_server, start_provider_process};
use crate::pedelec_native_registration::register_chrome_native_messaging_host;
use crate::pedelec_paths::{
    ensure_user_path_contains_pedelec_dir, install_pedelec_agent_from_path,
    install_pedelec_native_host_from_path, install_pedelec_tool_from_path,
    pedelec_agent_binary_name, pedelec_native_host_binary_name, pedelec_tool_binary_name,
    prepend_pedelec_dir_to_process_path,
};
use std::path::PathBuf;
use std::thread;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Emitter, Manager, RunEvent, State};

const MAIN_WINDOW_LABEL: &str = "main";

pub fn run() {
    let runtime_owner = CoreRuntimeOwner::new();
    let runtime = runtime_owner.runtime();
    let runtime_for_setup = runtime.clone();
    let runtime_for_exit = runtime.clone();

    tauri::Builder::default()
        .manage(runtime_owner)
        .invoke_handler(tauri::generate_handler![
            create_thread,
            get_settings,
            update_settings,
            check_ollama_connection,
            list_ollama_models,
            list_providers,
            send_text,
            submit_tool_result,
            end_thread
        ])
        .setup(move |app| {
            let pedelec_tool_source = bundled_binary_path(app, pedelec_tool_binary_name())?;
            let pedelec_agent_source = bundled_binary_path(app, pedelec_agent_binary_name())?;
            let native_host_source = bundled_binary_path(app, pedelec_native_host_binary_name())?;
            let _pedelec_tool_path =
                install_pedelec_tool_from_path(&pedelec_tool_source).map_err(|err| {
                    tauri::Error::from(std::io::Error::other(format!(
                        "cannot install pedelec-cli: {}",
                        err.message
                    )))
                })?;
            let _pedelec_agent_path = install_pedelec_agent_from_path(&pedelec_agent_source)
                .map_err(|err| {
                    tauri::Error::from(std::io::Error::other(format!(
                        "cannot install pedelec-agent: {}",
                        err.message
                    )))
                })?;
            let _native_host_path = install_pedelec_native_host_from_path(&native_host_source)
                .map_err(|err| {
                    tauri::Error::from(std::io::Error::other(format!(
                        "cannot install pedelec-native-host: {}",
                        err.message
                    )))
                })?;
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
                _pedelec_tool_path.to_string_lossy()
            );
            #[cfg(debug_assertions)]
            eprintln!(
                "pedelec-agent installed at {}",
                _pedelec_agent_path.to_string_lossy()
            );
            #[cfg(debug_assertions)]
            eprintln!(
                "pedelec-native-host installed at {}",
                _native_host_path.to_string_lossy()
            );
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
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
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
