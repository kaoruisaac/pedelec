use pedelec_core::{error_codes, PedelecError};
use pedelec_ipc::CoreIpcPlatformServices;
use std::path::PathBuf;
use tauri::AppHandle;
#[cfg(not(target_os = "macos"))]
use tauri_plugin_dialog::DialogExt;
#[cfg(any(not(target_os = "macos"), test))]
use tauri_plugin_dialog::FilePath;

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
    use objc2::MainThreadMarker;
    use std::sync::mpsc::sync_channel;

    // Core IPC invokes this synchronous service on a worker thread. Never
    // block the AppKit main thread waiting for work scheduled onto itself.
    if MainThreadMarker::new().is_some() {
        return Err(picker_failed(
            "folder picker cannot block the AppKit main thread",
        ));
    }

    let restore_accessory = matches!(
        mac_picker_policy(main_window_visibility(app)),
        MacPickerPolicy::RestoreAccessory
    );
    let _policy_guard = MacPickerPolicyGuard::new(app, restore_accessory);

    if restore_accessory {
        app.set_activation_policy(tauri::ActivationPolicy::Regular)
            .map_err(|err| picker_failed(format!("could not enable app activation: {err}")))?;
    }

    let (sender, receiver) = sync_channel(1);
    app.run_on_main_thread(move || {
        let result = match MainThreadMarker::new() {
            Some(marker) => pick_directory_appkit(marker),
            None => Err(picker_failed(
                "folder picker did not run on the AppKit main thread",
            )),
        };
        // The channel carries only owned path/error data, never AppKit objects.
        let _ = sender.send(result);
    })
    .map_err(|err| picker_failed(format!("could not schedule folder picker: {err}")))?;

    // runModal runs AppKit's own modal event loop; the user may take as long as
    // necessary to choose a folder. The IPC worker has no picker timeout.
    receiver
        .recv()
        .map_err(|err| picker_failed(format!("folder picker result was interrupted: {err}")))?
}

#[cfg(target_os = "macos")]
fn pick_directory_appkit(marker: objc2::MainThreadMarker) -> Result<Option<PathBuf>, PedelecError> {
    use objc2_app_kit::{NSApplication, NSModalResponseOK, NSOpenPanel};
    use std::ffi::{CStr, OsString};
    use std::os::unix::ffi::OsStringExt;

    let application = NSApplication::sharedApplication(marker);
    request_macos_activation(&application);

    // Unlike the Tauri/rfd picker, runModal presents an independent
    // application-modal NSOpenPanel, not a sheet attached to the main window.
    let panel = NSOpenPanel::openPanel(marker);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);

    let response = panel.runModal();
    if response != NSModalResponseOK {
        return mac_modal_result(response, None);
    }

    let url = panel
        .URL()
        .ok_or_else(|| picker_failed("native folder picker returned no directory URL"))?;
    if !url.isFileURL() {
        return Err(picker_failed(
            "native folder picker returned a non-file URL",
        ));
    }

    // NSURL's filesystem representation preserves the selected path without
    // converting through a lossy UTF-8 String.
    let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
    if bytes.is_empty() {
        return Err(picker_failed(
            "native folder picker returned an empty directory path",
        ));
    }
    mac_modal_result(
        response,
        Some(PathBuf::from(OsString::from_vec(bytes.to_vec()))),
    )
}

// NSModalResponseOK = 1 and NSModalResponseCancel = 0. An abort or
// unexpected response is a picker failure, never a user cancellation.
#[cfg(any(target_os = "macos", test))]
fn mac_modal_result(
    response: isize,
    selected_path: Option<PathBuf>,
) -> Result<Option<PathBuf>, PedelecError> {
    match response {
        0 => Ok(None),
        1 => selected_path
            .map(Some)
            .ok_or_else(|| picker_failed("native folder picker returned no selected path")),
        other => Err(picker_failed(format!(
            "native folder picker failed with modal response {other}"
        ))),
    }
}

#[cfg(any(target_os = "macos", test))]
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
fn request_macos_activation(application: &objc2_app_kit::NSApplication) {
    use objc2_foundation::NSProcessInfo;

    let major_version = NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion as usize;

    match mac_activation_strategy(major_version) {
        MacActivationStrategy::Modern => application.activate(),
        MacActivationStrategy::Legacy => request_legacy_macos_activation(application),
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn request_legacy_macos_activation(application: &objc2_app_kit::NSApplication) {
    application.activateIgnoringOtherApps(true);
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum MacActivationStrategy {
    Modern,
    Legacy,
}

#[cfg(any(target_os = "macos", test))]
fn mac_activation_strategy(major_version: usize) -> MacActivationStrategy {
    if major_version >= 14 {
        MacActivationStrategy::Modern
    } else {
        MacActivationStrategy::Legacy
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

#[cfg(any(not(target_os = "macos"), test))]
fn dialog_result_to_path(file_path: Option<FilePath>) -> Result<Option<PathBuf>, PedelecError> {
    let Some(file_path) = file_path else {
        return Ok(None);
    };
    file_path_to_path(file_path)
}

#[cfg(any(not(target_os = "macos"), test))]
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
    fn macos_modal_result_returns_selected_directory() {
        let path = PathBuf::from("/Users/example/workspace");
        assert_eq!(mac_modal_result(1, Some(path.clone())).unwrap(), Some(path));
    }

    #[test]
    fn macos_modal_result_treats_cancel_as_none() {
        assert_eq!(mac_modal_result(0, None).unwrap(), None);
    }

    #[test]
    fn macos_modal_result_rejects_missing_selected_path() {
        let error = mac_modal_result(1, None).unwrap_err();
        assert_eq!(error.code, error_codes::DIRECTORY_PICKER_FAILED);
    }

    #[test]
    fn macos_modal_result_reports_abort_as_error() {
        let error = mac_modal_result(-1001, None).unwrap_err();
        assert_eq!(error.code, error_codes::DIRECTORY_PICKER_FAILED);
    }

    #[test]
    fn macos_14_uses_modern_activation() {
        assert_eq!(mac_activation_strategy(14), MacActivationStrategy::Modern);
    }

    #[test]
    fn macos_newer_than_14_uses_modern_activation() {
        assert_eq!(mac_activation_strategy(15), MacActivationStrategy::Modern);
    }

    #[test]
    fn macos_13_uses_legacy_activation() {
        assert_eq!(mac_activation_strategy(13), MacActivationStrategy::Legacy);
    }

    #[test]
    fn macos_older_than_13_uses_legacy_activation() {
        assert_eq!(mac_activation_strategy(12), MacActivationStrategy::Legacy);
        assert_eq!(mac_activation_strategy(10), MacActivationStrategy::Legacy);
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
