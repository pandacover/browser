// Release builds on Windows are GUI apps (no console window); debug builds keep the console for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! The browser (`browser.rs`, `library.rs`, `session.rs`, `theme.rs`) is the same everywhere;
//! `platform` is the native shell around it: Win32 and WebView2 on Windows (`src/win`), AppKit
//! and WebKit on macOS (`src/mac`). Both offer the same `Host` (the window, its panes, timers)
//! and `View` (one webview).

mod browser;
mod library;
mod session;
mod theme;

#[cfg(windows)]
#[path = "win/mod.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "mac/mod.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("This browser runs on Windows (WebView2) and macOS (WebKit).");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    platform::run()
}
