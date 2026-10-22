//! Keep the fullscreen transparent HUD classified as an auxiliary window.
use windows::Win32::Foundation::{
    GetLastError, SetLastError, HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowThreadProcessId, SetWindowLongPtrW, GWL_EXSTYLE, STYLESTRUCT,
    WM_NCDESTROY, WM_STYLECHANGING, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

const AUXILIARY_BITS: u32 = WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;

fn persistent_overlay_style(style: u32) -> u32 {
    style | AUXILIARY_BITS
}

const SUBCLASS_ID: usize = 0xB100_0002;

unsafe extern "system" fn overlay_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_STYLECHANGING && wparam.0 as i32 == GWL_EXSTYLE.0 && lparam.0 != 0 {
        // Tao rebuilds the extended style when cursor passthrough changes,
        // dropping the metadata written by re_assert_topmost. Preserve only
        // these two auxiliary-window bits; leave hit-testing/rendering alone.
        let styles = &mut *(lparam.0 as *mut STYLESTRUCT);
        styles.styleNew = persistent_overlay_style(styles.styleNew);
    } else if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(overlay_subclass), subclass_id);
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}

/// Install from the window's owning UI thread during startup.
pub fn install(hwnd: HWND) -> Result<(), String> {
    unsafe {
        if GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() {
            return Err("Overlay metadata hook must be installed on its UI thread".into());
        }
        if !SetWindowSubclass(hwnd, Some(overlay_subclass), SUBCLASS_ID, 0).as_bool() {
            return Err("Could not install overlay metadata hook".into());
        }
        let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, persistent_overlay_style(old) as isize);
        let error = GetLastError();
        if previous == 0 && error.0 != 0 {
            let _ = RemoveWindowSubclass(hwnd, Some(overlay_subclass), SUBCLASS_ID);
            return Err(format!(
                "Could not initialize overlay metadata: {}",
                error.0
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tauri::Manager;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };

    #[test]
    fn interactive_overlay_keeps_its_auxiliary_window_metadata() {
        // Measured while dragging the left HUD on Win11 26300.9711:
        // rect 0,0,2560,1440; Tao rewrote the style to 0x00040118.
        let interactive = 0x0004_0118;
        assert_eq!(
            persistent_overlay_style(interactive),
            interactive | AUXILIARY_BITS
        );
    }

    #[test]
    fn auxiliary_policy_preserves_the_cursor_and_rendering_flags() {
        for style in [0, 0x0004_0118, 0x000c_0138, 0xffff_ffff] {
            let protected = persistent_overlay_style(style);
            assert_eq!(protected & !AUXILIARY_BITS, style & !AUXILIARY_BITS);
            assert_eq!(protected & AUXILIARY_BITS, AUXILIARY_BITS);
        }
    }

    #[test]
    #[ignore = "real hidden WebView2 style-transition regression; run explicitly"]
    fn native_overlay_metadata_survives_tauri_cursor_mode_changes() {
        let mut context = tauri::generate_context!();
        context.config_mut().identifier = "com.sehaz.bloom.overlay-style-test".into();
        context.config_mut().app.windows.clear();
        let app = tauri::Builder::default()
            .any_thread()
            .setup(|app| {
                let window = tauri::WebviewWindowBuilder::new(
                    app,
                    "overlay-style-test",
                    tauri::WebviewUrl::External("about:blank".parse().unwrap()),
                )
                .visible(false)
                .focused(false)
                .decorations(false)
                .transparent(true)
                .skip_taskbar(true)
                .build()?;
                install(window.hwnd()?).map_err(std::io::Error::other)?;
                Ok(())
            })
            .build(context)
            .unwrap();
        let handle = app.handle().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let waiting = Instant::now();
                let window = loop {
                    if let Some(window) = handle.get_webview_window("overlay-style-test") {
                        break window;
                    }
                    assert!(
                        waiting.elapsed() < Duration::from_secs(5),
                        "Hidden test window did not initialize"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(!window.is_visible().unwrap());
                let initial =
                    unsafe { GetWindowLongPtrW(window.hwnd().unwrap(), GWL_EXSTYLE) } as u32;
                assert_eq!(initial & AUXILIARY_BITS, AUXILIARY_BITS);
                for ignore in [true, false, true, false] {
                    window.set_ignore_cursor_events(ignore).unwrap();
                    let start = Instant::now();
                    let style = loop {
                        let style =
                            unsafe { GetWindowLongPtrW(window.hwnd().unwrap(), GWL_EXSTYLE) }
                                as u32;
                        if (style & WS_EX_TRANSPARENT.0 != 0) == ignore {
                            break style;
                        }
                        assert!(
                            start.elapsed() < Duration::from_secs(5),
                            "Cursor mode did not change"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    };
                    assert_eq!(
                        style & AUXILIARY_BITS,
                        AUXILIARY_BITS,
                        "Tao dropped the overlay metadata during its style rewrite"
                    );
                    assert_eq!(style & WS_EX_LAYERED.0 != 0, ignore);
                    assert!(!window.is_visible().unwrap());
                }
            }));
            tx.send(result.is_ok()).unwrap();
            handle.exit(0);
        });
        let exit_code = app.run_return(|_, _| {});
        assert_eq!(exit_code, 0);
        worker.join().unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "Hidden overlay style test failed"
        );
    }
}
