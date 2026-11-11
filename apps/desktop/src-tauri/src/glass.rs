//! The window's own glass where the system has a material for it: Mica on Windows 11, used only where Windows accepts it.

/// Applies the material and says whether it took; macOS frosts the window from its config instead.
#[tauri::command]
pub(crate) fn window_glass(window: tauri::WebviewWindow) -> bool {
    applied(&window)
}

#[cfg(windows)]
fn applied(window: &tauri::WebviewWindow) -> bool {
    use windows::Win32::Graphics::Dwm::{
        DWM_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DwmSetWindowAttribute,
    };
    let Ok(hwnd) = window.hwnd() else {
        return false;
    };
    let material = DWMSBT_MAINWINDOW;
    // SAFETY: the window handle is this live window's, and the value is a DWM_SYSTEMBACKDROP_TYPE of the size passed.
    let set = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            std::ptr::from_ref(&material).cast(),
            std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        )
    };
    // Before Windows 11 22H2 the attribute is unknown, Windows refuses it, and the window stays solid.
    set.is_ok()
}

#[cfg(not(windows))]
fn applied(_window: &tauri::WebviewWindow) -> bool {
    false
}
