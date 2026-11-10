// Release builds are GUI apps (no console window); debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod theme;

use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::{CreateSolidBrush, HBRUSH},
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::*,
};

const CLASS_NAME: PCWSTR = w!("SingularityBrowserWindow");
const MIN_WIDTH_DIP: i32 = 480;
const MIN_HEIGHT_DIP: i32 = 320;

fn main() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let palette = theme::Palette::current();
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: LoadIconW(None, IDI_APPLICATION)?,
            // Paint the empty window in the UI's own color so there is no white flash on start.
            hbrBackground: HBRUSH(CreateSolidBrush(palette.strip).0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("Singularity"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            None,
        )?;

        // Size the window for the monitor's DPI (CW_USEDEFAULT ignores scaling).
        let dpi = GetDpiForWindow(hwnd) as i32;
        let scale = |v: i32| v * dpi / 96;
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            scale(1280),
            scale(820),
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        theme::apply_to_window(hwnd, &palette);

        let urls: Vec<String> = std::env::args().skip(1).collect();
        browser::start(hwnd, urls, palette)?;

        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CoUninitialize();
    }
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            browser::WM_APP_EVENT => {
                browser::drain_events();
                LRESULT(0)
            }
            WM_SIZE => {
                let minimized = wparam.0 as u32 == SIZE_MINIMIZED;
                browser::with_browser_or_defer(move |b| b.on_resize(minimized));
                LRESULT(0)
            }
            WM_MOVE => {
                browser::with_browser(|b| b.on_move());
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // Windows suggests a new window rect for the new monitor's DPI; WebView2
                // rescales its own content automatically.
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd) as i32;
                info.ptMinTrackSize.x = MIN_WIDTH_DIP * dpi / 96;
                info.ptMinTrackSize.y = MIN_HEIGHT_DIP * dpi / 96;
                LRESULT(0)
            }
            WM_SETFOCUS => {
                browser::with_browser(|b| b.focus_content());
                LRESULT(0)
            }
            WM_TIMER => {
                browser::with_browser(|b| b.on_timer(wparam.0));
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                browser::shutdown();
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
