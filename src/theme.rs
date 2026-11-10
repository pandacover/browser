//! Light/dark palette shared by the native window and the HTML UI.

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND},
        Graphics::Dwm::{DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    },
    core::{BOOL, w},
};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Tab strip / title bar color. Must match `--strip` in ui/chrome.css.
    pub strip: COLORREF,
}

impl Palette {
    pub fn current() -> Self {
        let dark = system_uses_dark_mode();
        Palette {
            dark,
            strip: if dark { rgb(0x1f, 0x1f, 0x23) } else { rgb(0xe9, 0xe9, 0xee) },
        }
    }

    /// (r, g, b) of the strip color, for WebView2's default background.
    pub fn strip_rgb(&self) -> (u8, u8, u8) {
        let c = self.strip.0;
        (c as u8, (c >> 8) as u8, (c >> 16) as u8)
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn system_uses_dark_mode() -> bool {
    let mut value: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 0
}

/// Dark title bar in dark mode, and a caption color that blends into the tab strip.
pub fn apply_to_window(hwnd: HWND, palette: &Palette) {
    unsafe {
        let dark = BOOL::from(palette.dark);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            size_of::<BOOL>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &palette.strip as *const _ as *const _,
            size_of::<COLORREF>() as u32,
        );
    }
}
