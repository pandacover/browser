//! Windows: a Win32 window around Microsoft Edge WebView2 webviews.
//!
//! One shared WebView2 environment (one browser process, GPU process and network process;
//! each tab gets its own renderer). The window has no title bar (`frame.rs`); the sidebar,
//! the page cards and the split's second card are child "panes", and the command bar is a
//! popup window owned by the main window.
//!
//! WebView2 callbacks only queue a `browser::Event` and post `WM_APP_EVENT`; the window
//! procedure drains the queue.

mod frame;

use std::{cell::Cell, time::Duration};

use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Dwm::{DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmFlush, DwmSetWindowAttribute},
            Gdi::{
                ClientToScreen, CreateSolidBrush, GetMonitorInfoW, HBRUSH, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
                MONITORINFO, MonitorFromRect, MonitorFromWindow, ScreenToClient,
            },
        },
        System::{
            Com::*,
            LibraryLoader::GetModuleHandleW,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        },
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};
// Both glob imports above export these; we want the Windows ones.
use windows::core::{Error, Result};

use crate::{
    browser::{self, Command, Event, FRAME_MESSAGE, PageState, Rect, Surface, TabId, TimerId, WindowAction},
    session,
    theme::{Palette, Rgb},
};
use frame::PaneShape;

/// Errors the shared browser code sees.
type Res<T> = std::result::Result<T, String>;

const WM_APP_EVENT: u32 = WM_APP + 1;
/// Next frame of the sidebar slide; posted once per screen refresh while it moves.
const WM_APP_ANIMATE: u32 = WM_APP + 2;

const CLASS_NAME: PCWSTR = w!("BrowserWindow");
const MIN_WIDTH_DIP: i32 = 480;
const MIN_HEIGHT_DIP: i32 = 320;

const UI_HOST: &str = "browser.ui";
pub const UI_ORIGIN: &str = "https://browser.ui/";
pub const UI_URL: &str = "https://browser.ui/chrome.html";
pub const COMMAND_URL: &str = "https://browser.ui/command.html";
pub const NEW_TAB_URL: &str = "https://browser.ui/newtab.html";
/// Strip along the top edge that resizes the window. Webviews start below it, because
/// the window only sees the mouse where no webview covers it.
pub const TOP_RESIZE_DIP: i32 = 4;
/// Before the sidebar hides, widen the page and wait until it has drawn at that size
/// (`Browser::prerender_hide`); otherwise WebView2 flashes its background in the new strip.
pub const PRERENDER_HIDE: bool = true;
/// WebView2 reports favicons itself (`FaviconChanged`), so pages aren't asked.
pub const FAVICON_SCRIPT: Option<&str> = None;

/// The shared WebView2 environment.
pub type Env = ICoreWebView2Environment;

thread_local! {
    static MAIN_HWND: Cell<HWND> = Cell::new(HWND::default());
}

pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let palette = Palette::new(system_uses_dark_mode());
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        // Paint empty areas in the UI's own color so there is no white flash on start.
        let background = HBRUSH(CreateSolidBrush(colorref(palette.strip)).0);
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: LoadIconW(None, IDI_APPLICATION)?,
            hbrBackground: background,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(Error::from_thread().into());
        }
        frame::register_classes(instance, background, Some(popup_proc))?;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("New Tab"),
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
        MAIN_HWND.with(|h| h.set(hwnd));

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
            // FRAMECHANGED applies our WM_NCCALCSIZE (no title bar) right away.
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        apply_to_window(hwnd, &palette);

        let urls: Vec<String> = std::env::args().skip(1).collect();
        let host = Host::new(hwnd, &palette)?;
        // Puts the window back where it was last time (if the session is restored).
        let maximized = browser::start(host, urls, palette)?;

        let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOWDEFAULT });

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
            WM_APP_EVENT => {
                browser::drain_events();
                LRESULT(0)
            }
            WM_APP_ANIMATE => {
                browser::with_browser(|b| b.animate());
                LRESULT(0)
            }
            WM_NCCALCSIZE => frame::on_nccalcsize(hwnd, wparam, lparam),
            WM_NCHITTEST => frame::on_nchittest(hwnd, wparam, lparam),
            WM_SIZE => {
                browser::with_browser_or_defer(|b| b.on_resize());
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xffff) as i16 as i32;
                browser::with_browser(|b| b.on_mouse_move(x));
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
                browser::with_browser(|b| b.on_window_focus());
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

/// The command bar's window. Losing activation (a click on the page, Alt+Tab) closes it.
extern "system" fn popup_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_ACTIVATE && (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
        browser::post(Event::CommandBarDeactivated);
    }
    frame::popup_default(hwnd, msg, wparam, lparam)
}

/// Wake the window procedure to drain the event queue.
pub fn wake() {
    let hwnd = MAIN_HWND.with(|h| h.get());
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_EVENT, WPARAM(0), LPARAM(0));
    }
}

/// Window commands from the UI. They aren't posted as messages because WebView2 can swallow
/// posted WM_SYSCOMMANDs while it moves focus into a page.
pub fn run_window_actions(actions: Vec<WindowAction>) {
    let hwnd = MAIN_HWND.with(|h| h.get());
    let syscommand = |cmd: u32| unsafe {
        SendMessageW(hwnd, WM_SYSCOMMAND, Some(WPARAM(cmd as usize)), Some(LPARAM(0)));
    };
    for action in actions {
        match action {
            WindowAction::Minimize => syscommand(SC_MINIMIZE),
            WindowAction::Maximize => syscommand(SC_MAXIMIZE),
            WindowAction::Restore => syscommand(SC_RESTORE),
            WindowAction::Close => syscommand(SC_CLOSE),
            WindowAction::Drag => unsafe {
                let _ = ReleaseCapture();
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let lparam = ((pt.y as u16 as isize) << 16) | (pt.x as u16 as isize);
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(lparam)));
            },
        }
    }
}

pub fn fatal(msg: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("Browser"), MB_OK | MB_ICONERROR);
        PostQuitMessage(1);
    }
}

/// Start the shared WebView2 environment; `Event::EnvironmentReady` follows.
pub fn start_environment() -> Res<()> {
    let user_data = session::user_data_dir();
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        // Thin scrollbars that appear over the page, like Edge's.
        options.set_scroll_bar_style(COREWEBVIEW2_SCROLLBAR_STYLE_FLUENT_OVERLAY);
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(|hr, env| {
        browser::post(Event::EnvironmentReady(hr.and_then(|_| env.ok_or_else(|| Error::from(E_POINTER))).map_err(|e| {
            format!(
                "Could not start the Microsoft Edge WebView2 runtime.\n\n{e}\n\n\
                 Install it from https://developer.microsoft.com/microsoft-edge/webview2/"
            )
        })));
        Ok(())
    }));
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &HSTRING::from(user_data.as_os_str()), &options, &handler)
    }
    .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ theme

fn colorref((r, g, b): Rgb) -> COLORREF {
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

/// Dark title bar in dark mode, and a caption color that blends into the sidebar.
fn apply_to_window(hwnd: HWND, palette: &Palette) {
    let caption = colorref(palette.strip);
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
            &caption as *const _ as *const _,
            size_of::<COLORREF>() as u32,
        );
    }
}

fn to_rect(r: Rect) -> RECT {
    RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

fn from_rect(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

// ------------------------------------------------------------------ the window

struct FullscreenRestore {
    style: isize,
    placement: WINDOWPLACEMENT,
}

/// The main window, its panes and the command bar's popup.
pub struct Host {
    hwnd: HWND,
    env: Option<Env>,
    /// Holds the sidebar's webview.
    sidebar_pane: HWND,
    /// The page cards, clipped to rounded corners. Tab webviews live in the first, except the
    /// right-hand tab of a split, which lives in the second.
    panes: [HWND; 2],
    pane_shapes: [PaneShape; 2],
    command_popup: HWND,
    /// Set while in full screen: how to put the window back.
    fullscreen: Option<FullscreenRestore>,
}

impl Host {
    fn new(hwnd: HWND, palette: &Palette) -> Result<Host> {
        // Created bottom to top: the sidebar stacks above the page so it can slide over it.
        let content_pane = frame::create_pane(hwnd)?;
        // The right-hand card of split view; hidden until a split is shown.
        let split_pane = frame::create_pane(hwnd)?;
        unsafe {
            let _ = ShowWindow(split_pane, SW_HIDE);
        }
        let sidebar_pane = frame::create_pane(hwnd)?;
        let command_popup = frame::create_popup(hwnd, palette.dark)?;
        Ok(Host {
            hwnd,
            env: None,
            sidebar_pane,
            panes: [content_pane, split_pane],
            pane_shapes: [PaneShape::default(); 2],
            command_popup,
            fullscreen: None,
        })
    }

    pub fn set_env(&mut self, env: Env) {
        self.env = Some(env);
    }

    /// Start creating a webview in `parent`; `make` turns the result into the event to post.
    fn create_view(&self, parent: HWND, make: impl Fn(Res<View>) -> Event + 'static) -> Res<()> {
        let Some(env) = &self.env else { return Ok(()) };
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, c| {
            let view = hr.and_then(|_| c.ok_or_else(|| Error::from(E_POINTER))).and_then(View::new);
            browser::post(make(view.map_err(|e| e.to_string())));
            Ok(())
        }));
        unsafe { env.CreateCoreWebView2Controller(parent, &handler) }.map_err(|e| e.to_string())
    }

    pub fn create_ui_view(&self, surface: Surface, _ui_dir: &str, _background: Rgb) -> Res<()> {
        let parent = match surface {
            Surface::Sidebar => self.sidebar_pane,
            Surface::CommandBar => self.command_popup,
        };
        self.create_view(parent, move |view| Event::UiViewReady(surface, view))
    }

    pub fn create_tab_view(&self, id: TabId, pane: usize, _ui_dir: &str, _background: Rgb) -> Res<()> {
        self.create_view(self.panes[pane], move |view| Event::TabViewReady(id, view))
    }

    /// Move a tab's webview into card pane `pane`.
    pub fn move_to_pane(&self, view: &View, pane: usize) -> Res<()> {
        unsafe { view.controller.SetParentWindow(self.panes[pane]) }.map_err(|e| e.to_string())
    }

    /// Device-independent pixels to physical pixels at the window's DPI.
    pub fn scale(&self, dip: i32) -> i32 {
        frame::scale(self.hwnd, dip)
    }

    pub fn client_rect(&self) -> Rect {
        let mut rc = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut rc) };
        from_rect(rc)
    }

    /// A repeating timer; `Browser::on_timer(id)` runs each time until it's killed.
    pub fn set_timer(&self, id: TimerId, ms: u32) {
        unsafe {
            SetTimer(Some(self.hwnd), id, ms, None);
        }
    }

    pub fn kill_timer(&self, id: TimerId) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), id);
        }
    }

    pub fn close_window(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    pub fn set_title(&self, title: &str) {
        let _ = unsafe { SetWindowTextW(self.hwnd, &HSTRING::from(title)) };
    }

    pub fn is_minimized(&self) -> bool {
        unsafe { IsIconic(self.hwnd).as_bool() }
    }

    pub fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd).as_bool() }
    }

    /// The cursor, in client coordinates.
    pub fn cursor_pos(&self) -> (i32, i32) {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(self.hwnd, &mut pt);
        }
        (pt.x, pt.y)
    }

    pub fn left_button_down(&self) -> bool {
        unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 }
    }

    /// Cover the whole monitor without a frame, or put the window back.
    pub fn set_fullscreen(&mut self, on: bool) {
        let hwnd = self.hwnd;
        unsafe {
            if on {
                if self.fullscreen.is_some() {
                    return;
                }
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                let mut placement = WINDOWPLACEMENT {
                    length: size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                let _ = GetWindowPlacement(hwnd, &mut placement);
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(monitor, &mut info);
                self.fullscreen = Some(FullscreenRestore { style, placement });
                SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW.0 as isize));
                let m = info.rcMonitor;
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    m.left,
                    m.top,
                    m.right - m.left,
                    m.bottom - m.top,
                    SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            } else if let Some(restore) = self.fullscreen.take() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, restore.style);
                let _ = SetWindowPlacement(hwnd, &restore.placement);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            }
        }
    }

    /// The window's normal (restored) rect in screen pixels, and whether it's maximized.
    pub fn placement(&self) -> (Rect, bool) {
        let placement = match &self.fullscreen {
            Some(restore) => restore.placement,
            None => {
                let mut p = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                let _ = unsafe { GetWindowPlacement(self.hwnd, &mut p) };
                p
            }
        };
        let maximized =
            placement.showCmd == SW_SHOWMAXIMIZED.0 as u32 || placement.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0;
        (from_rect(placement.rcNormalPosition), maximized)
    }

    /// Put the (still hidden) window where it was last time, if that's on a screen now.
    pub fn restore_placement(&self, r: Rect) -> bool {
        let rect = to_rect(r);
        let on_screen = unsafe { !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid() };
        if !on_screen {
            return false;
        }
        let placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            showCmd: SW_HIDE.0 as u32,
            rcNormalPosition: rect,
            ..Default::default()
        };
        unsafe {
            let _ = SetWindowPlacement(self.hwnd, &placement);
        }
        true
    }

    /// The color card pane `pane` shows where its webview hasn't drawn yet.
    pub fn set_pane_color(&self, pane: usize, rgb: Rgb) {
        frame::set_pane_color(self.panes[pane], rgb);
    }

    /// Clip card pane `pane` to rounded corners at its new size.
    pub fn shape_pane(&mut self, pane: usize, width: i32, height: i32, radius: i32) {
        frame::shape_pane(self.panes[pane], &mut self.pane_shapes[pane], width, height, radius);
    }

    /// Move the card panes and the sidebar together; `split_change` shows or hides the
    /// second card.
    pub fn move_panes(&self, cards: [Rect; 2], split_change: Option<bool>, sidebar: Rect) {
        frame::move_panes(&[
            (self.panes[0], to_rect(cards[0]), HWND_BOTTOM, None),
            (self.panes[1], to_rect(cards[1]), HWND_BOTTOM, split_change),
            (self.sidebar_pane, to_rect(sidebar), HWND_TOP, None),
        ]);
    }

    /// One frame of the sidebar's slide, paced by the compositor.
    pub fn slide_sidebar(&self, r: Rect) {
        unsafe {
            let _ = SetWindowPos(self.sidebar_pane, None, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            if DwmFlush().is_err() {
                std::thread::sleep(Duration::from_millis(8));
            }
        }
    }

    /// Run `Browser::animate` again soon (after the current messages).
    pub fn request_animate(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_APP_ANIMATE, WPARAM(0), LPARAM(0));
        }
    }

    /// Put the command bar's window at `r` (client coordinates), or off screen while it's
    /// closed (cloaked, still rendering).
    pub fn place_command_bar(&self, r: Rect, shown: bool) {
        let mut origin = POINT { x: r.left, y: r.top };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut origin);
        }
        if !shown {
            origin = POINT { x: -20000, y: -20000 };
        }
        unsafe {
            let _ = SetWindowPos(
                self.command_popup,
                None,
                origin.x,
                origin.y,
                r.width(),
                r.height(),
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    pub fn show_command_window(&self) {
        frame::set_cloaked(self.command_popup, false);
        unsafe {
            // Only take the foreground if we already have it (never steal it from another app).
            if GetForegroundWindow() == self.hwnd {
                let _ = SetForegroundWindow(self.command_popup);
            }
        }
    }

    pub fn hide_command_window(&self) {
        frame::set_cloaked(self.command_popup, true);
    }

    /// After the command bar is hidden: give activation back to the main window.
    pub fn command_window_hidden(&self) {
        unsafe {
            if GetForegroundWindow() == self.command_popup {
                let _ = SetForegroundWindow(self.hwnd);
            } else if GetActiveWindow() == self.command_popup {
                let _ = SetActiveWindow(self.hwnd);
            }
        }
    }

    /// The command bar's window is the active one.
    pub fn command_bar_active(&self) -> bool {
        unsafe { GetActiveWindow() == self.command_popup }
    }
}

// ------------------------------------------------------------------ webviews

/// One WebView2 webview.
#[derive(Clone)]
pub struct View {
    controller: ICoreWebView2Controller,
    view: ICoreWebView2,
}

impl View {
    fn new(controller: ICoreWebView2Controller) -> Result<View> {
        let view = unsafe { controller.CoreWebView2()? };
        Ok(View { controller, view })
    }

    /// Settings and events for the sidebar or the command bar.
    pub fn configure_ui(&self, surface: Surface, ui_dir: &str, background: Rgb) -> Res<()> {
        self.configure_ui_inner(surface, ui_dir, background).map_err(|e| e.to_string())
    }

    fn configure_ui_inner(&self, surface: Surface, ui_dir: &str, background: Rgb) -> Result<()> {
        let (controller, view) = (&self.controller, &self.view);
        unsafe {
            let settings = view.Settings()?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetIsZoomControlEnabled(false)?;
            // Lets `app-region: drag` in the sidebar move the window like a title bar.
            if let Ok(s9) = settings.cast::<ICoreWebView2Settings9>() {
                s9.SetIsNonClientRegionSupportEnabled(true)?;
            }
            if !cfg!(debug_assertions) {
                settings.SetAreDefaultContextMenusEnabled(false)?;
                settings.SetAreDevToolsEnabled(false)?;
            }
            set_background(controller, background);

            view.cast::<ICoreWebView2_3>()?.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(UI_HOST),
                &HSTRING::from(ui_dir),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
            )?;

            let mut token = 0i64;
            view.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        let mut source = PWSTR::null();
                        args.Source(&mut source)?;
                        if !take_pwstr(source).starts_with(surface.url()) {
                            return Ok(());
                        }
                        let mut msg = PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut msg).is_ok() {
                            browser::post(Event::UiMessage(surface, take_pwstr(msg)));
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            // The UI webview must only ever show our own pages; anything else opens as a tab.
            view.add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(|_, args| {
                    if let Some(args) = args {
                        let mut uri = PWSTR::null();
                        args.Uri(&mut uri)?;
                        let uri = take_pwstr(uri);
                        if !uri.starts_with(UI_ORIGIN) {
                            args.SetCancel(true)?;
                            browser::post(Event::OpenInNewTab(uri));
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            view.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                    if let Some(args) = args {
                        let mut uri = PWSTR::null();
                        args.Uri(&mut uri)?;
                        args.SetHandled(true)?;
                        browser::post(Event::OpenInNewTab(take_pwstr(uri)));
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            register_accelerators(controller)?;
        }
        Ok(())
    }

    /// Settings and events for a tab's webview.
    pub fn configure_tab(&self, id: TabId, ui_dir: &str) -> Res<()> {
        self.configure_tab_inner(id, ui_dir).map_err(|e| e.to_string())
    }

    fn configure_tab_inner(&self, id: TabId, ui_dir: &str) -> Result<()> {
        let (controller, view) = (&self.controller, &self.view);
        unsafe {
            // Lets the built-in new tab page load; DENY_CORS keeps web pages from reading it.
            view.cast::<ICoreWebView2_3>()?.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(UI_HOST),
                &HSTRING::from(ui_dir),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS,
            )?;
            if let Ok(s4) = view.Settings()?.cast::<ICoreWebView2Settings4>() {
                // Offer to save passwords and fill in forms, like other browsers.
                let _ = s4.SetIsPasswordAutosaveEnabled(true);
                let _ = s4.SetIsGeneralAutofillEnabled(true);
            }
            register_tab_events(id, view)?;
            register_accelerators(controller)?;
            let mut token = 0i64;
            controller.add_GotFocus(
                &FocusChangedEventHandler::create(Box::new(move |_, _| {
                    browser::post(Event::TabFocused(id));
                    Ok(())
                })),
                &mut token,
            )?;
        }
        Ok(())
    }

    pub fn navigate(&self, url: &str) -> Res<()> {
        unsafe { self.view.Navigate(&HSTRING::from(url)) }.map_err(|e| e.to_string())
    }

    pub fn reload(&self) {
        let _ = unsafe { self.view.Reload() };
    }

    pub fn stop(&self) {
        let _ = unsafe { self.view.Stop() };
    }

    pub fn back(&self) {
        let _ = unsafe { self.view.GoBack() };
    }

    pub fn forward(&self) {
        let _ = unsafe { self.view.GoForward() };
    }

    /// Bounds relative to the webview's pane (or popup).
    pub fn set_bounds(&self, r: Rect) {
        let _ = unsafe { self.controller.SetBounds(to_rect(r)) };
    }

    /// Hidden webviews stop rendering; showing a suspended one resumes it.
    pub fn set_visible(&self, visible: bool) {
        let _ = unsafe { self.controller.SetIsVisible(visible) };
    }

    pub fn set_background(&self, rgb: Rgb) {
        set_background(&self.controller, rgb);
    }

    pub fn focus(&self) {
        let _ = unsafe { self.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
    }

    pub fn close(&self) {
        let _ = unsafe { self.controller.Close() };
    }

    /// The page receives it as a `message` event whose `data` is the parsed JSON.
    pub fn post_json(&self, json: &str) {
        let _ = unsafe { self.view.PostWebMessageAsJson(&HSTRING::from(json)) };
    }

    pub fn eval(&self, js: &str) {
        let _ = unsafe { self.view.ExecuteScript(&HSTRING::from(js), None) };
    }

    /// Run a script and post `event(id, result)`, the result as JSON.
    pub fn eval_with_result(&self, js: &str, id: TabId, event: fn(TabId, String) -> Event) {
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |hr, result| {
            if hr.is_ok() {
                browser::post(event(id, result));
            }
            Ok(())
        }));
        let _ = unsafe { self.view.ExecuteScript(&HSTRING::from(js), &handler) };
    }

    pub fn state(&self) -> PageState {
        let view = &self.view;
        let mut state = PageState::default();
        unsafe {
            let mut s = PWSTR::null();
            if view.Source(&mut s).is_ok() {
                state.url = Some(take_pwstr(s));
            }
            let mut s = PWSTR::null();
            if view.DocumentTitle(&mut s).is_ok() {
                state.title = Some(take_pwstr(s));
            }
            let mut b = BOOL::default();
            if view.CanGoBack(&mut b).is_ok() {
                state.can_back = Some(b.as_bool());
            }
            let mut b = BOOL::default();
            if view.CanGoForward(&mut b).is_ok() {
                state.can_forward = Some(b.as_bool());
            }
            if let Ok(v8) = view.cast::<ICoreWebView2_8>() {
                let mut b = BOOL::default();
                if v8.IsDocumentPlayingAudio(&mut b).is_ok() {
                    state.audible = Some(b.as_bool());
                }
                let mut b = BOOL::default();
                if v8.IsMuted(&mut b).is_ok() {
                    state.muted = Some(b.as_bool());
                }
            }
            if let Ok(v15) = view.cast::<ICoreWebView2_15>() {
                let mut s = PWSTR::null();
                if v15.FaviconUri(&mut s).is_ok() {
                    state.favicon = Some(take_pwstr(s));
                }
            }
        }
        state
    }

    pub fn toggle_mute(&self) {
        if let Ok(v8) = self.view.cast::<ICoreWebView2_8>() {
            let mut muted = BOOL::default();
            unsafe {
                if v8.IsMuted(&mut muted).is_ok() {
                    let _ = v8.SetIsMuted(!muted.as_bool());
                }
            }
        }
    }

    /// Background tabs ask WebView2 to keep memory low; the visible tab gets normal limits.
    pub fn set_memory_target(&self, visible: bool) {
        if let Ok(v19) = self.view.cast::<ICoreWebView2_19>() {
            let level = if visible {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
            } else {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
            };
            let _ = unsafe { v19.SetMemoryUsageTargetLevel(level) };
        }
    }

    /// Freeze the page; `Event::TabSuspended` follows. False if it couldn't be asked.
    pub fn try_suspend(&self, id: TabId) -> bool {
        let Ok(v3) = self.view.cast::<ICoreWebView2_3>() else { return false };
        let handler = TrySuspendCompletedHandler::create(Box::new(move |hr, ok| {
            browser::post(Event::TabSuspended(id, hr.is_ok() && ok));
            Ok(())
        }));
        unsafe { v3.TrySuspend(&handler) }.is_ok()
    }

    pub fn contains_fullscreen(&self) -> Option<bool> {
        let mut b = BOOL::default();
        unsafe { self.view.ContainsFullScreenElement(&mut b) }.ok().map(|_| b.as_bool())
    }

    /// Keeps dropdowns, tooltips and IME windows positioned correctly.
    pub fn notify_parent_moved(&self) {
        let _ = unsafe { self.controller.NotifyParentWindowPositionChanged() };
    }
}

fn set_background(controller: &ICoreWebView2Controller, (r, g, b): Rgb) {
    if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
        let _ = unsafe { c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: r, G: g, B: b }) };
    }
}

unsafe fn register_tab_events(id: TabId, view: &ICoreWebView2) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        view.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_DocumentTitleChanged(
            &DocumentTitleChangedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_HistoryChanged(
            &HistoryChangedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let mut uri = String::new();
                if let Some(args) = args {
                    let mut s = PWSTR::null();
                    if args.Uri(&mut s).is_ok() {
                        uri = take_pwstr(s);
                    }
                }
                browser::post(Event::TabNavigating(id, uri));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabLoaded(id));
                Ok(())
            })),
            &mut token,
        )?;
        // Popups and target=_blank links open as tabs.
        view.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    args.SetHandled(true)?;
                    browser::post(Event::OpenInNewTab(take_pwstr(uri)));
                }
                Ok(())
            })),
            &mut token,
        )?;
        view.add_WindowCloseRequested(
            &WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabCloseRequested(id));
                Ok(())
            })),
            &mut token,
        )?;
        // Only our own frame signal is used; anything else a page posts is ignored.
        view.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut msg = PWSTR::null();
                    if args.TryGetWebMessageAsString(&mut msg).is_ok() {
                        let msg = take_pwstr(msg);
                        if msg.starts_with(FRAME_MESSAGE) && msg.len() < 40 {
                            browser::post(Event::TabFramed(msg));
                        }
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        view.add_ContainsFullScreenElementChanged(
            &ContainsFullScreenElementChangedEventHandler::create(Box::new(move |_, _| {
                browser::post(Event::TabFullscreenChanged(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                    args.ProcessFailedKind(&mut kind)?;
                    if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
                        || kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
                    {
                        browser::post(Event::TabCrashed(id));
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        if let Ok(v8) = view.cast::<ICoreWebView2_8>() {
            v8.add_IsDocumentPlayingAudioChanged(
                &IsDocumentPlayingAudioChangedEventHandler::create(Box::new(move |_, _| {
                    browser::post(Event::TabUpdated(id));
                    Ok(())
                })),
                &mut token,
            )?;
            v8.add_IsMutedChanged(
                &IsMutedChangedEventHandler::create(Box::new(move |_, _| {
                    browser::post(Event::TabUpdated(id));
                    Ok(())
                })),
                &mut token,
            )?;
        }
        if let Ok(v15) = view.cast::<ICoreWebView2_15>() {
            v15.add_FaviconChanged(
                &FaviconChangedEventHandler::create(Box::new(move |_, _| {
                    browser::post(Event::TabUpdated(id));
                    Ok(())
                })),
                &mut token,
            )?;
        }
    }
    Ok(())
}

/// Browser keyboard shortcuts, active whether focus is in a page or in the UI.
unsafe fn register_accelerators(controller: &ICoreWebView2Controller) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        controller.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(|_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                args.KeyEventKind(&mut kind)?;
                if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                    && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
                {
                    return Ok(());
                }
                let mut vk = 0u32;
                args.VirtualKey(&mut vk)?;
                if let Some(cmd) = command_for_key(VIRTUAL_KEY(vk as u16)) {
                    args.SetHandled(true)?;
                    browser::post(Event::Command(cmd));
                }
                Ok(())
            })),
            &mut token,
        )
    }
}

fn command_for_key(vk: VIRTUAL_KEY) -> Option<Command> {
    let down = |k: VIRTUAL_KEY| unsafe { GetKeyState(k.0 as i32) } < 0;
    let (ctrl, shift, alt) = (down(VK_CONTROL), down(VK_SHIFT), down(VK_MENU));
    use Command::*;
    Some(match (ctrl, shift, alt, vk) {
        (true, false, false, VK_T) => NewTab,
        (true, true, false, VK_T) => ReopenClosedTab,
        (true, false, false, VK_W) | (true, false, false, VK_F4) => CloseTab,
        (true, false, false, VK_L) | (false, false, true, VK_D) | (false, false, false, VK_F6) => EditAddress,
        (true, false, false, VK_TAB) | (true, false, false, VK_NEXT) => NextTab,
        (true, true, false, VK_TAB) | (true, false, false, VK_PRIOR) => PrevTab,
        (true, false, false, VK_R) | (false, false, false, VK_F5) => Reload,
        (false, false, true, VK_LEFT) => Back,
        (false, false, true, VK_RIGHT) => Forward,
        (true, false, false, VK_S) => ToggleSidebar,
        (false, false, false, VK_F11) => ToggleFullscreen,
        (true, false, false, VK_D) => ToggleBookmark,
        // The key left of Enter on US keyboards (backslash).
        (true, false, false, VK_OEM_5) => ToggleSplit,
        (true, false, false, k) if (0x31..=0x38).contains(&k.0) => SelectTab((k.0 - 0x31) as usize),
        (true, false, false, k) if k.0 == 0x39 => SelectTab(usize::MAX),
        (false, false, true, k) if (0x31..=0x39).contains(&k.0) => SelectSpace((k.0 - 0x31) as usize),
        _ => return None,
    })
}
