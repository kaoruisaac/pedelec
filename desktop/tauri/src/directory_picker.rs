use pedelec_core::{error_codes, PedelecError};
use pedelec_ipc::CoreIpcPlatformServices;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, FilePath};

#[cfg(target_os = "macos")]
use tauri::Manager;

#[cfg(windows)]
use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
#[cfg(windows)]
use std::num::NonZeroIsize;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

#[cfg(target_os = "macos")]
const MAIN_WINDOW_LABEL: &str = "main";

#[derive(Clone)]
pub struct TauriCoreIpcPlatformServices {
    app: AppHandle,
}

impl TauriCoreIpcPlatformServices {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl CoreIpcPlatformServices for TauriCoreIpcPlatformServices {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PedelecError> {
        #[cfg(windows)]
        {
            return dialog_result_to_path(pick_directory_windows(&self.app));
        }

        #[cfg(target_os = "macos")]
        {
            return pick_directory_macos(&self.app);
        }

        #[cfg(not(any(windows, target_os = "macos")))]
        {
            dialog_result_to_path(self.app.dialog().file().blocking_pick_folder())
        }
    }
}

#[cfg(windows)]
fn pick_directory_windows(app: &AppHandle) -> Option<FilePath> {
    let foreground_window = foreground_window_handle();

    match windows_picker_parent_strategy(foreground_window.as_ref()) {
        WindowsPickerParentStrategy::ForegroundWindow => app
            .dialog()
            .file()
            .set_parent(
                foreground_window
                    .as_ref()
                    .expect("foreground window handle"),
            )
            .blocking_pick_folder(),
        WindowsPickerParentStrategy::Unparented => {
            // GetForegroundWindow can legitimately return null. Keep the existing
            // unparented picker behavior instead of turning that into a picker error.
            app.dialog().file().blocking_pick_folder()
        }
    }
}

#[cfg(windows)]
fn foreground_window_handle() -> Option<ForegroundWindowHandle> {
    let hwnd = unsafe { GetForegroundWindow() };
    ForegroundWindowHandle::from_hwnd(hwnd as isize)
}

#[cfg(windows)]
#[derive(Debug)]
struct ForegroundWindowHandle {
    window_handle: RawWindowHandle,
    display_handle: RawDisplayHandle,
}

#[cfg(windows)]
impl ForegroundWindowHandle {
    fn from_hwnd(hwnd: isize) -> Option<Self> {
        let hwnd = NonZeroIsize::new(hwnd)?;

        Some(Self {
            window_handle: RawWindowHandle::Win32(Win32WindowHandle::new(hwnd)),
            display_handle: RawDisplayHandle::Windows(WindowsDisplayHandle::new()),
        })
    }
}

#[cfg(windows)]
impl HasWindowHandle for ForegroundWindowHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        // The HWND is obtained immediately before the native dialog is created,
        // and the returned borrowed handle never outlives this wrapper.
        Ok(unsafe { WindowHandle::borrow_raw(self.window_handle) })
    }
}

#[cfg(windows)]
impl HasDisplayHandle for ForegroundWindowHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.display_handle) })
    }
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum WindowsPickerParentStrategy {
    ForegroundWindow,
    Unparented,
}

#[cfg(windows)]
fn windows_picker_parent_strategy(
    foreground_window: Option<&ForegroundWindowHandle>,
) -> WindowsPickerParentStrategy {
    if foreground_window.is_some() {
        WindowsPickerParentStrategy::ForegroundWindow
    } else {
        WindowsPickerParentStrategy::Unparented
    }
}

#[cfg(target_os = "macos")]
fn pick_directory_macos(app: &AppHandle) -> Result<Option<PathBuf>, PedelecError> {
    let restore_accessory = matches!(
        mac_picker_policy(main_window_visibility(app)),
        MacPickerPolicy::RestoreAccessory
    );
    let _policy_guard = MacPickerPolicyGuard::new(app, restore_accessory);

    if restore_accessory {
        app.set_activation_policy(tauri::ActivationPolicy::Regular)
            .map_err(|err| picker_failed(format!("could not enable app activation: {err}")))?;
    }

    // This handoff is intentionally separate from the blocking picker call. The
    // picker can remain open for as long as the user needs without a timeout.
    activate_macos(app)?;

    dialog_result_to_path(app.dialog().file().blocking_pick_folder())
}

#[cfg(target_os = "macos")]
fn picker_failed(message: impl Into<String>) -> PedelecError {
    PedelecError::new(error_codes::DIRECTORY_PICKER_FAILED, message)
}

#[cfg(target_os = "macos")]
fn main_window_visibility(app: &AppHandle) -> Option<bool> {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        eprintln!(
            "directory picker could not inspect the main window; keeping the current activation policy"
        );
        return None;
    };

    match window.is_visible() {
        Ok(is_visible) => Some(is_visible),
        Err(err) => {
            eprintln!(
                "directory picker could not read main window visibility: {err}; keeping the current activation policy"
            );
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn activate_macos(app: &AppHandle) -> Result<(), PedelecError> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};
    use std::sync::Arc;
    use std::time::Duration;

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_on_main_thread = Arc::clone(&cancelled);
    let (sender, receiver) = sync_channel(1);

    let schedule_result = app.run_on_main_thread(move || {
        let result = if cancelled_on_main_thread.load(Ordering::Acquire) {
            Err("activation handoff was cancelled".to_string())
        } else if let Some(marker) = MainThreadMarker::new() {
            NSApplication::sharedApplication(marker).activate();
            Ok(())
        } else {
            Err("activation handoff did not run on the AppKit main thread".to_string())
        };

        let _ = sender.send(result);
    });

    if let Err(err) = schedule_result {
        cancelled.store(true, Ordering::Release);
        return Err(picker_failed(format!(
            "could not schedule app activation: {err}"
        )));
    }

    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(picker_failed(message)),
        Err(RecvTimeoutError::Timeout) => {
            cancelled.store(true, Ordering::Release);
            Err(picker_failed("timed out waiting for app activation"))
        }
        Err(RecvTimeoutError::Disconnected) => {
            cancelled.store(true, Ordering::Release);
            Err(picker_failed("app activation handoff was interrupted"))
        }
    }
}

#[cfg(target_os = "macos")]
struct MacPickerPolicyGuard {
    app: AppHandle,
    restore_accessory: bool,
}

#[cfg(target_os = "macos")]
impl MacPickerPolicyGuard {
    fn new(app: &AppHandle, restore_accessory: bool) -> Self {
        Self {
            app: app.clone(),
            restore_accessory,
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacPickerPolicyGuard {
    fn drop(&mut self) {
        if self.restore_accessory {
            if let Err(err) = self
                .app
                .set_activation_policy(tauri::ActivationPolicy::Accessory)
            {
                eprintln!("directory picker could not restore Accessory policy: {err}");
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum MacPickerPolicy {
    KeepCurrent,
    RestoreAccessory,
}

#[cfg(any(target_os = "macos", test))]
fn mac_picker_policy(main_window_visible: Option<bool>) -> MacPickerPolicy {
    match main_window_visible {
        Some(false) => MacPickerPolicy::RestoreAccessory,
        // If visibility cannot be read, do not change the app's activation policy
        // permanently just because a directory picker was requested.
        Some(true) | None => MacPickerPolicy::KeepCurrent,
    }
}

fn dialog_result_to_path(file_path: Option<FilePath>) -> Result<Option<PathBuf>, PedelecError> {
    let Some(file_path) = file_path else {
        return Ok(None);
    };
    file_path_to_path(file_path)
}

fn file_path_to_path(file_path: FilePath) -> Result<Option<PathBuf>, PedelecError> {
    file_path.into_path().map(Some).map_err(|err| {
        PedelecError::new(
            error_codes::DIRECTORY_PICKER_FAILED,
            format!("could not read the selected directory path: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn converts_cancelled_dialog_to_none() {
        assert_eq!(dialog_result_to_path(None).unwrap(), None);
    }

    #[test]
    fn converts_path_file_path_without_loss() {
        let path = PathBuf::from("C:\\workspace\\project");
        assert_eq!(
            file_path_to_path(FilePath::Path(path.clone())).unwrap(),
            Some(path)
        );
    }

    #[test]
    fn converts_file_url_file_path() {
        let path = std::env::temp_dir().join("pedelec-directory-picker-test");
        let url = Url::from_file_path(&path).unwrap();
        assert_eq!(file_path_to_path(FilePath::Url(url)).unwrap(), Some(path));
    }

    #[test]
    fn rejects_non_file_url_without_exposing_a_path() {
        let error = file_path_to_path(FilePath::Url(
            Url::parse("https://example.com/project").unwrap(),
        ))
        .unwrap_err();
        assert_eq!(error.code, error_codes::DIRECTORY_PICKER_FAILED);
        assert!(!error.message.contains("example.com"));
    }

    #[test]
    fn hidden_main_window_restores_accessory_policy() {
        assert_eq!(
            mac_picker_policy(Some(false)),
            MacPickerPolicy::RestoreAccessory
        );
    }

    #[test]
    fn visible_main_window_keeps_regular_policy() {
        assert_eq!(mac_picker_policy(Some(true)), MacPickerPolicy::KeepCurrent);
    }

    #[test]
    fn unknown_main_window_visibility_keeps_current_policy() {
        assert_eq!(mac_picker_policy(None), MacPickerPolicy::KeepCurrent);
    }

    #[cfg(windows)]
    #[test]
    fn windows_picker_uses_foreground_parent_when_available() {
        let foreground_window = ForegroundWindowHandle::from_hwnd(1).unwrap();
        assert_eq!(
            windows_picker_parent_strategy(Some(&foreground_window)),
            WindowsPickerParentStrategy::ForegroundWindow
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_picker_falls_back_to_unparented_dialog_without_an_error() {
        assert_eq!(
            windows_picker_parent_strategy(None),
            WindowsPickerParentStrategy::Unparented
        );
    }
}
