//! Light/dark palette shared by the native window and the HTML UI.

/// A color as (red, green, blue).
pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Window and sidebar color. Must match `--strip` in ui/chrome.css.
    pub strip: Rgb,
    /// New tab page background. Must match `--bg` in ui/newtab.html.
    pub page: Rgb,
    /// Command bar background. Must match `--panel` in ui/command.css.
    pub panel: Rgb,
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        if dark {
            Palette { dark, strip: (0x18, 0x18, 0x1b), page: (0x21, 0x21, 0x25), panel: (0x26, 0x26, 0x2b) }
        } else {
            Palette { dark, strip: (0xe8, 0xe8, 0xec), page: (0xfa, 0xfa, 0xfb), panel: (0xff, 0xff, 0xff) }
        }
    }
}
