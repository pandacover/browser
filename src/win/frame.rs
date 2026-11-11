//! Custom window frame. The window keeps its native resize borders, snapping, shadow and
//! rounded corners, but has no title bar: the sidebar draws the window buttons, and the
//! page sits as a rounded card inside a thin margin.
//!
//! Also the child "panes" that hold webviews, and the command bar's popup window.

use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Dwm::{
                DWMWA_CLOAK, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
                DwmSetWindowAttribute,
            },
            Gdi::{BeginPaint, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HBRUSH, PAINTSTRUCT, SetWindowRgn},
        },
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::*,
};

use super::TOP_RESIZE_DIP;
use crate::browser::CARD_MARGIN_DIP;

const PANE_CLASS: PCWSTR = w!("BrowserPane");
const POPUP_CLASS: PCWSTR = w!("BrowserPopup");

pub fn scale(hwnd: HWND, dip: i32) -> i32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) } as i32;
    dip * dpi / 96
}

/// Panes are plain child windows that hold webviews. They let us control stacking order
/// (the sidebar can float over the page) and clip the page to rounded corners.
pub fn register_classes(instance: HINSTANCE, background: HBRUSH, popup_proc: WNDPROC) -> Result<()> {
    let pane = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(pane_proc),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hbrBackground: background,
        lpszClassName: PANE_CLASS,
        ..Default::default()
    };
    let popup = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: popup_proc,
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hbrBackground: background,
        lpszClassName: POPUP_CLASS,
        ..Default::default()
    };
    for class in [&pane, &popup] {
        if unsafe { RegisterClassExW(class) } == 0 {
            return Err(Error::from_thread());
        }
    }
    Ok(())
}

pub fn create_pane(parent: HWND) -> Result<HWND> {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PANE_CLASS,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            None,
            None,
        )
    }
}

/// The color a pane paints where its webview hasn't drawn yet (for example the strip
/// that appears for a frame when the page grows, before the page has been laid out again).
pub fn set_pane_color(pane: HWND, rgb: (u8, u8, u8)) {
    let color = rgb.0 as isize | (rgb.1 as isize) << 8 | (rgb.2 as isize) << 16;
    unsafe {
        // +1 so that black isn't mistaken for "no color set".
        if GetWindowLongPtrW(pane, GWLP_USERDATA) != color + 1 {
            SetWindowLongPtrW(pane, GWLP_USERDATA, color + 1);
        }
    }
}

extern "system" fn pane_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_ERASEBKGND {
            let color = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if color != 0 {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let brush = CreateSolidBrush(COLORREF((color - 1) as u32));
                FillRect(windows::Win32::Graphics::Gdi::HDC(wparam.0 as _), &rc, brush);
                let _ = DeleteObject(brush.into());
                return LRESULT(1);
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// A pane's last placement, so layout only touches what actually changed. Re-setting a
/// window region or a webview's bounds makes it redraw, which shows up as flicker.
#[derive(Default, Clone, Copy, PartialEq)]
pub struct PaneShape {
    pub width: i32,
    pub height: i32,
    pub radius: i32,
}

/// Clip a pane to rounded corners for its new size. Done before the pane moves (without
/// redrawing), so the move and the new shape show up in the same frame.
pub fn shape_pane(pane: HWND, last: &mut PaneShape, width: i32, height: i32, radius: i32) {
    let shape = PaneShape { width, height, radius };
    if *last == shape {
        return;
    }
    *last = shape;
    unsafe {
        // The window takes ownership of the region.
        let region = (radius > 0).then(|| CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2));
        SetWindowRgn(pane, region, false);
    }
}

/// Move several panes at once, so the screen never shows some moved and others not.
/// Each move can also show (`Some(true)`) or hide (`Some(false)`) its pane.
pub fn move_panes(moves: &[(HWND, RECT, HWND, Option<bool>)]) {
    unsafe {
        let Ok(mut dwp) = BeginDeferWindowPos(moves.len() as i32) else { return };
        for &(pane, r, after, show) in moves {
            let visibility = match show {
                Some(true) => SWP_SHOWWINDOW,
                Some(false) => SWP_HIDEWINDOW,
                None => SET_WINDOW_POS_FLAGS(0),
            };
            match DeferWindowPos(
                dwp,
                pane,
                Some(after),
                r.left,
                r.top,
                (r.right - r.left).max(0),
                (r.bottom - r.top).max(0),
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | visibility,
            ) {
                Ok(next) => dwp = next,
                Err(_) => return,
            }
        }
        let _ = EndDeferWindowPos(dwp);
    }
}

// ------------------------------------------------------------------ command bar popup

/// The command bar lives in its own top-level window, owned by the browser window, so
/// Windows draws it with real rounded corners and a soft shadow over the page. It stays
/// "shown" but cloaked while closed, so its page keeps rendering and opens instantly.
pub fn create_popup(owner: HWND, dark: bool) -> Result<HWND> {
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            POPUP_CLASS,
            w!("Command bar"),
            // The thick frame gives DWM's shadow and rounded corners; WM_NCCALCSIZE hides it.
            WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN,
            -32000,
            -32000,
            100,
            100,
            Some(owner),
            None,
            None,
            None,
        )?;
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            size_of_val(&round) as u32,
        );
        let dark = BOOL::from(dark);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        set_cloaked(hwnd, true);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        Ok(hwnd)
    }
}

pub fn set_cloaked(hwnd: HWND, cloaked: bool) {
    let value = BOOL::from(cloaked);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &value as *const _ as *const _, 4);
    }
}

/// Window procedure pieces shared by the popup: no visible frame, no resizing.
pub fn popup_default(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            WM_NCHITTEST => LRESULT(HTCLIENT as isize),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ------------------------------------------------------------------ main window frame

fn has_frame(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    style & WS_THICKFRAME.0 != 0
}

fn is_maximized(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

/// WM_NCCALCSIZE: drop the title bar but keep the invisible resize borders on the left,
/// right and bottom.
pub fn on_nccalcsize(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if wparam.0 == 0 || !has_frame(hwnd) {
            return DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
        }
        let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
        let top = params.rgrc[0].top;
        DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
        params.rgrc[0].top = top;
        if is_maximized(hwnd) {
            // A maximized window hangs over the screen edge by its border width.
            let dpi = GetDpiForWindow(hwnd);
            params.rgrc[0].top +=
                GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        }
        LRESULT(0)
    }
}

/// WM_NCHITTEST for the parts of the window no webview covers: the margins around the card.
pub fn on_nchittest(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let hit = DefWindowProcW(hwnd, WM_NCHITTEST, wparam, lparam);
        if hit.0 as u32 != HTCLIENT || !has_frame(hwnd) {
            return hit;
        }
        let mut pt = POINT { x: (lparam.0 & 0xffff) as i16 as i32, y: ((lparam.0 >> 16) & 0xffff) as i16 as i32 };
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let margin = scale(hwnd, CARD_MARGIN_DIP);
        let edge = scale(hwnd, TOP_RESIZE_DIP);
        let corner = scale(hwnd, 16);
        if is_maximized(hwnd) {
            return LRESULT(if pt.y < margin { HTCAPTION } else { HTCLIENT } as isize);
        }
        let left = pt.x < corner;
        let right = pt.x >= rc.right - corner;
        let code = if pt.y < edge {
            if left { HTTOPLEFT } else if right { HTTOPRIGHT } else { HTTOP }
        } else if pt.y >= rc.bottom - margin {
            if left { HTBOTTOMLEFT } else if right { HTBOTTOMRIGHT } else { HTBOTTOM }
        } else if pt.x >= rc.right - margin {
            HTRIGHT
        } else if pt.y < margin {
            HTCAPTION
        } else {
            // Left margin when the sidebar is hidden: hovering it reveals the sidebar.
            HTCLIENT
        };
        LRESULT(code as isize)
    }
}
