use crate::directory_picker::TauriCoreIpcPlatformServices;
use crate::effort_wizard::{cleanup_stale_probe_runs, EffortWizardOwner};
use crate::pedelec_binary_install::{
    ensure_user_path_contains_pedelec_dir, install_pedelec_agent_from_path,
    install_pedelec_native_host_from_path, install_pedelec_tool_from_path,
    pedelec_agent_binary_name, pedelec_native_host_binary_name, pedelec_tool_binary_name,
    prepend_pedelec_dir_to_process_path, write_app_launch_config_for_current_exe,
    BinaryInstallOutcome,
};
use crate::pedelec_native_registration::register_chrome_native_messaging_host;
use crate::pedelec_upload::start_asset_upload_server;
use crate::provider_installer::{
    open as open_installer, OpenProviderInstallerInput, OpenProviderInstallerOutput,
};
use crate::provider_terminal::{
    open as open_provider_terminal_window, OpenProviderTerminalInput, OpenProviderTerminalOutput,
};
use pedelec_core::{
    error_codes, refresh_shared_providers, start_initial_provider_scan,
    wait_for_provider_readiness, CheckOllamaConnectionInput, CheckOllamaConnectionOutput,
    CoreRuntimeOwner, CreateThreadInput, CreateThreadOutput, EndThreadInput, ListOllamaModelsInput,
    OllamaModelOption, PedelecError, PedelecSettings, PrepareThreadInput, PrepareThreadOutput,
    ProviderInfo, SendTextInput, SendTextOutput, SharedCoreRuntime, SubmitToolResultInput,
    UpdateSettingsInput,
};
use pedelec_ipc::{
    prepare_provider_process, start_core_ipc_server_with_services, start_debug_provider_process,
    start_provider_process,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Emitter, Manager, RunEvent, State};
use tauri_plugin_opener::OpenerExt;

const MAIN_WINDOW_LABEL: &str = "main";

pub fn run() {
    let background_launch = is_background_launch(std::env::args_os());
    let runtime_owner = CoreRuntimeOwner::new();
    let runtime = runtime_owner.runtime();
    let runtime_for_setup = runtime.clone();
    let runtime_for_exit = runtime.clone();
    let effort_wizard_owner = EffortWizardOwner::new();
    let effort_wizard_for_exit = effort_wizard_owner.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if should_show_window_for_second_instance(args) {
                show_main_window(app);
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(runtime_owner)
        .manage(effort_wizard_owner)
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
            open_thread_workspace,
            restart_app,
            send_text,
            debug_send_text,
            prepare_thread,
            submit_tool_result,
            end_thread,
            monitor_end_thread,
            crate::effort_wizard::get_effort_wizard_bootstrap,
            crate::effort_wizard::get_effort_wizard_state,
            crate::effort_wizard::start_effort_wizard,
            crate::effort_wizard::apply_effort_wizard_settings,
            crate::effort_wizard::reset_effort_wizard
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if background_launch {
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let _startup_cleanup_errors = runtime_for_setup
                .lock()
                .unwrap()
                .cleanup_stale_workspaces_for_app_start();
            #[cfg(debug_assertions)]
            for err in _startup_cleanup_errors {
                eprintln!(
                    "workspace cleanup failed during app startup: {}",
                    err.message
                );
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
            // Provider detection is part of backend initialization, but it is
            // intentionally detached from UI/Core IPC startup.
            cleanup_stale_probe_runs();
            start_initial_provider_scan(runtime_for_setup.clone());
            // A failed data plane must not prevent the desktop/control plane from starting.
            let _asset_upload_server = start_asset_upload_server(runtime_for_setup.clone());
            let platform_services =
                Arc::new(TauriCoreIpcPlatformServices::new(app.handle().clone()));
            let _ipc_handle =
                start_core_ipc_server_with_services(runtime_for_setup.clone(), platform_services)
                    .map_err(|err| {
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

                #[cfg(target_os = "macos")]
                if window.label() == MAIN_WINDOW_LABEL {
                    let _ = window
                        .app_handle()
                        .set_activation_policy(tauri::ActivationPolicy::Accessory);
                }

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
                    effort_wizard_for_exit.cancel_active();
                    let _errors = runtime_for_exit.lock().unwrap().cleanup_for_app_exit();
                    #[cfg(debug_assertions)]
                    for err in _errors {
                        eprintln!("workspace cleanup failed during app exit: {}", err.message);
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
    let runtime = state.runtime();
    wait_for_provider_readiness(&runtime)?;
    let settings = runtime.lock().unwrap().update_settings(input);
    settings
}

#[tauri::command]
fn list_providers(state: State<'_, CoreRuntimeOwner>) -> Result<Vec<ProviderInfo>, PedelecError> {
    let runtime = state.runtime();
    wait_for_provider_readiness(&runtime)?;
    let providers = runtime.lock().unwrap().list_providers();
    Ok(providers)
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
    let runtime = state.runtime();
    wait_for_provider_readiness(&runtime)?;
    let executable = runtime
        .lock()
        .unwrap()
        .provider_executable_path(&input.provider)?;
    open_provider_terminal_window(input.provider, executable)
}

fn validated_thread_workspace_path(
    thread_id: &str,
    workspace_path: Option<PathBuf>,
) -> Result<PathBuf, PedelecError> {
    let workspace_path = workspace_path.ok_or_else(|| {
        PedelecError::with_details(
            error_codes::THREAD_NOT_FOUND,
            "thread workspace could not be resolved because the thread no longer exists",
            serde_json::json!({ "threadId": thread_id }),
        )
    })?;

    if !workspace_path.is_dir() {
        return Err(PedelecError::with_details(
            error_codes::WORKSPACE_PATH_INVALID,
            "thread workspace path is not a directory or no longer exists",
            serde_json::json!({
                "threadId": thread_id,
                "pathStatus": "missing_or_not_directory",
            }),
        ));
    }

    Ok(workspace_path)
}

#[tauri::command]
fn open_thread_workspace(
    app: tauri::AppHandle,
    state: State<'_, CoreRuntimeOwner>,
    thread_id: String,
) -> Result<(), PedelecError> {
    let workspace_path = {
        let runtime = state.runtime();
        let runtime = runtime.lock().unwrap();
        validated_thread_workspace_path(&thread_id, runtime.thread_workspace_path(&thread_id))?
    };

    app.opener()
        .open_path(
            workspace_path.to_string_lossy().into_owned(),
            None::<String>,
        )
        .map_err(|error| {
            PedelecError::with_details(
                error_codes::WORKSPACE_OPEN_FAILED,
                "cannot open thread workspace",
                serde_json::json!({
                    "threadId": thread_id,
                    "pathStatus": "validated_directory",
                    "error": error.to_string(),
                }),
            )
        })
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
    state
        .runtime()
        .lock()
        .unwrap()
        .authorize_thread_access(&input.thread_id, None)?;
    start_provider_process(state.runtime(), input)
}

#[tauri::command]
fn debug_send_text(
    state: State<'_, CoreRuntimeOwner>,
    input: SendTextInput,
) -> Result<SendTextOutput, PedelecError> {
    debug_start_provider_process(state.runtime(), input)
}

fn debug_start_provider_process(
    runtime: SharedCoreRuntime,
    input: SendTextInput,
) -> Result<SendTextOutput, PedelecError> {
    start_debug_provider_process(runtime, input)
}

#[tauri::command]
fn prepare_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: PrepareThreadInput,
) -> Result<PrepareThreadOutput, PedelecError> {
    state
        .runtime()
        .lock()
        .unwrap()
        .authorize_thread_access(&input.thread_id, None)?;
    prepare_provider_process(state.runtime(), input)
}

#[tauri::command]
fn submit_tool_result(
    state: State<'_, CoreRuntimeOwner>,
    input: SubmitToolResultInput,
) -> Result<(), PedelecError> {
    state
        .runtime()
        .lock()
        .unwrap()
        .authorize_thread_access(&input.thread_id, None)?;
    state.runtime().lock().unwrap().submit_tool_result(input)
}

#[tauri::command]
fn end_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: EndThreadInput,
) -> Result<(), PedelecError> {
    state
        .runtime()
        .lock()
        .unwrap()
        .authorize_thread_access(&input.thread_id, None)?;
    state.runtime().lock().unwrap().end_thread(input)
}

#[tauri::command]
fn monitor_end_thread(
    state: State<'_, CoreRuntimeOwner>,
    input: EndThreadInput,
) -> Result<(), PedelecError> {
    end_thread_from_monitor(&state.runtime(), input)
}

fn end_thread_from_monitor(
    runtime: &SharedCoreRuntime,
    input: EndThreadInput,
) -> Result<(), PedelecError> {
    runtime.lock().unwrap().end_thread(input)
}

fn forward_thread_events_to_tauri(app: tauri::AppHandle, runtime: SharedCoreRuntime) {
    let event_rx = runtime.lock().unwrap().subscribe_all_threads();
    thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            let _ = app.emit("thread_event", event);
        }
    });
}

#[cfg(test)]
mod workspace_open_tests {
    use super::*;
    use std::fs;

    #[test]
    fn accepts_an_existing_workspace_directory() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_path = temp.path().join("t000123");
        fs::create_dir(&workspace_path).unwrap();

        assert_eq!(
            validated_thread_workspace_path("t000123", Some(workspace_path.clone())).unwrap(),
            workspace_path
        );
    }

    #[test]
    fn reports_missing_thread_as_thread_not_found() {
        let error = validated_thread_workspace_path("t000123", None).unwrap_err();

        assert_eq!(error.code, error_codes::THREAD_NOT_FOUND);
        assert_eq!(
            error.details,
            Some(serde_json::json!({ "threadId": "t000123" }))
        );
    }

    #[test]
    fn rejects_a_missing_or_non_directory_workspace_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing_path = temp.path().join("missing");
        let missing_error =
            validated_thread_workspace_path("t000123", Some(missing_path.clone())).unwrap_err();

        assert_eq!(missing_error.code, error_codes::WORKSPACE_PATH_INVALID);
        assert_eq!(
            missing_error.details,
            Some(serde_json::json!({
                "threadId": "t000123",
                "pathStatus": "missing_or_not_directory",
            }))
        );

        let file_path = temp.path().join("file");
        fs::write(&file_path, "not a directory").unwrap();
        let file_error = validated_thread_workspace_path("t000123", Some(file_path)).unwrap_err();

        assert_eq!(file_error.code, error_codes::WORKSPACE_PATH_INVALID);
    }
}

#[cfg(test)]
mod debug_send_text_tests {
    use super::*;
    use pedelec_core::{
        CommandSpec, CoreRuntime, EffortLevel, ProviderAdapterState, ProviderCode, ThreadState,
        ThreadStatus, WorkspaceManager,
    };
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[test]
    fn normal_thread_access_still_requires_the_matching_sdk_origin() {
        let (runtime, _temp) = runtime_with_sdk_thread(ThreadStatus::Idle);
        let runtime = runtime.lock().unwrap();

        assert!(runtime
            .authorize_thread_access("t000001", Some("https://example.com"))
            .is_ok());
        assert_eq!(
            runtime
                .authorize_thread_access("t000001", Some("https://other.example"))
                .unwrap_err()
                .code,
            error_codes::THREAD_ACCESS_DENIED
        );
        assert_eq!(
            runtime
                .authorize_thread_access("t000001", None)
                .unwrap_err()
                .code,
            error_codes::THREAD_ACCESS_DENIED
        );
    }

    #[test]
    fn monitor_thread_stop_bypasses_sdk_origin_authorization() {
        let (runtime, _temp) = runtime_with_sdk_thread(ThreadStatus::Idle);

        end_thread_from_monitor(
            &runtime,
            EndThreadInput {
                thread_id: "t000001".into(),
            },
        )
        .expect("Monitor stop should end an SDK-owned thread");

        assert_eq!(
            runtime.lock().unwrap().thread_status("t000001"),
            Some(ThreadStatus::Ended)
        );
    }

    #[test]
    fn debug_send_text_skips_origin_authorization_but_uses_normal_send_start() {
        let (runtime, _temp) = runtime_with_sdk_thread(ThreadStatus::Idle);
        let workspace_path = runtime
            .lock()
            .unwrap()
            .thread_workspace_path("t000001")
            .unwrap();
        runtime.lock().unwrap().test_provider_command = Some(test_provider_command(
            workspace_path,
            "What did you just change?",
        ));

        let output = debug_start_provider_process(
            Arc::clone(&runtime),
            SendTextInput {
                thread_id: "t000001".into(),
                message: "What did you just change?".into(),
            },
        )
        .expect("debug send should reach the provider start path");

        assert_eq!(output.thread_id, "t000001");
        assert_eq!(
            runtime.lock().unwrap().thread_status("t000001"),
            Some(ThreadStatus::Running)
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .thread_manager
                .thread("t000001")
                .unwrap()
                .sdk_origin
                .as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn debug_send_text_keeps_busy_thread_protection() {
        let (runtime, _temp) = runtime_with_sdk_thread(ThreadStatus::Running);

        let error = debug_start_provider_process(
            runtime,
            SendTextInput {
                thread_id: "t000001".into(),
                message: "This must be rejected while running.".into(),
            },
        )
        .unwrap_err();

        assert_eq!(error.code, error_codes::THREAD_BUSY);
    }

    fn runtime_with_sdk_thread(
        status: ThreadStatus,
    ) -> (Arc<Mutex<CoreRuntime>>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspaces");
        let workspace_path = workspace_root.join("t000001");
        std::fs::create_dir_all(&workspace_path).unwrap();

        let mut runtime = CoreRuntime::default();
        runtime.provider_readiness.mark_ready_for_test();
        runtime.workspace_manager = WorkspaceManager::with_workspace_root(&workspace_root);
        let now = chrono::Utc::now();
        runtime.thread_manager.insert_thread(
            ThreadState {
                thread_id: "t000001".into(),
                provider: ProviderCode::Codex,
                effort_level: EffortLevel::Default,
                effort_args: Vec::new(),
                workspace_path,
                skills: Vec::new(),
                status,
                process_id: None,
                created_at: now,
                updated_at: now,
                sdk_origin: Some("https://example.com".into()),
            },
            ProviderAdapterState {
                provider_session_id: None,
                last_process_id: None,
                has_user_message: false,
            },
        );

        (Arc::new(Mutex::new(runtime)), temp)
    }

    fn test_provider_command(cwd: PathBuf, message: &str) -> CommandSpec {
        #[cfg(windows)]
        let (program, args) = (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                "[Console]::In.ReadToEnd() | Out-Null; Start-Sleep -Seconds 1".to_string(),
            ],
        );
        #[cfg(not(windows))]
        let (program, args) = (
            "sh".to_string(),
            vec!["-c".to_string(), "cat >/dev/null; sleep 1".to_string()],
        );

        CommandSpec {
            program,
            args,
            cwd,
            env: Vec::new(),
            prompt: message.to_string(),
            stdin: message.to_string(),
        }
    }
}
