//! Browser state: one shared WebView2 environment, one UI webview (tab strip + toolbar,
//! written in HTML under `ui/`), and one webview per tab.
//!
//! Threading model: everything runs on the UI thread. WebView2 callbacks never touch
//! `Browser` directly; they push an [`Event`] onto a queue and post `WM_APP_EVENT`, and the
//! window procedure drains the queue. This avoids re-entrancy problems when a WebView2 call
//! fires another event synchronously.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
        UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};
// Both glob imports above export these; we want the Windows ones.
use windows::core::{Error, Result};

use crate::theme::Palette;

pub const WM_APP_EVENT: u32 = WM_APP + 1;

const UI_HOST: &str = "singularity.ui";
const UI_URL: &str = "https://singularity.ui/chrome.html";
const NEW_TAB_URL: &str = "https://singularity.ui/newtab.html";
/// Height of the HTML tab strip + toolbar, in device-independent pixels.
/// Must match `--ui-height` in ui/chrome.css.
const UI_HEIGHT_DIP: i32 = 80;
const TIMER_SUSPEND: usize = 1;
const SUSPEND_CHECK_MS: u32 = 15_000;
/// Background tabs idle this long are frozen with `TrySuspend` (override with
/// the `SINGULARITY_SUSPEND_SECS` environment variable).
const DEFAULT_SUSPEND_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_CLOSED_TABS: usize = 25;

type TabId = u32;

pub enum Event {
    EnvironmentReady(Result<ICoreWebView2Environment>),
    UiControllerReady(Result<ICoreWebView2Controller>),
    TabControllerReady(TabId, Result<ICoreWebView2Controller>),
    UiMessage(String),
    TabUpdated(TabId),
    TabLoading(TabId, bool),
    OpenInNewTab(String),
    TabCloseRequested(TabId),
    TabFullscreenChanged(TabId),
    TabSuspended(TabId, bool),
    TabCrashed(TabId),
    Command(Command),
    Resize(bool),
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    NewTab,
    CloseTab,
    ReopenClosedTab,
    FocusAddress,
    NextTab,
    PrevTab,
    /// 0-based index; `usize::MAX` selects the last tab.
    SelectTab(usize),
    Reload,
    Back,
    Forward,
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static MAIN_HWND: Cell<HWND> = Cell::new(HWND::default());
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
}

/// Queue an event for the UI thread. Safe to call from any WebView2 callback.
pub fn post(event: Event) {
    QUEUE.with(|q| q.borrow_mut().push_back(event));
    let hwnd = MAIN_HWND.with(|h| h.get());
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_EVENT, WPARAM(0), LPARAM(0));
    }
}

pub fn drain_events() {
    loop {
        let Some(event) = QUEUE.with(|q| q.borrow_mut().pop_front()) else {
            break;
        };
        let deferred = BROWSER.with(|b| match b.try_borrow_mut() {
            Ok(mut guard) => {
                if let Some(browser) = guard.as_mut() {
                    browser.handle(event);
                }
                None
            }
            Err(_) => Some(event),
        });
        if let Some(event) = deferred {
            // Re-entered while the browser was busy: try again on the next message.
            QUEUE.with(|q| q.borrow_mut().push_front(event));
            post_wakeup();
            return;
        }
    }
    with_browser(|b| b.flush_ui_state());
}

fn post_wakeup() {
    let hwnd = MAIN_HWND.with(|h| h.get());
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_EVENT, WPARAM(0), LPARAM(0));
    }
}

/// Run `f` on the browser if it isn't already borrowed further up the stack.
pub fn with_browser(f: impl FnOnce(&mut Browser)) -> bool {
    BROWSER.with(|b| match b.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(browser) = guard.as_mut() {
                f(browser);
            }
            true
        }
        Err(_) => false,
    })
}

/// Resize must never be lost, so if the browser is busy, replay it later.
pub fn with_browser_or_defer(f: impl FnOnce(&mut Browser) + Copy) {
    if !with_browser(f) {
        post(Event::Resize(false));
    }
}

pub fn start(hwnd: HWND, urls: Vec<String>, palette: Palette) -> Result<()> {
    MAIN_HWND.with(|h| h.set(hwnd));
    let suspend_after = std::env::var("SINGULARITY_SUSPEND_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SUSPEND_AFTER);

    let mut browser = Browser {
        hwnd,
        palette,
        env: None,
        ui: None,
        ui_view: None,
        ui_ready: false,
        ui_dirty: true,
        focus_address_when_ready: false,
        tabs: Vec::new(),
        active: None,
        next_id: 1,
        closed: Vec::new(),
        fullscreen: None,
        minimized: false,
        suspend_after,
        ui_dir: ui_dir(),
    };

    if urls.is_empty() {
        browser.open_tab(NEW_TAB_URL.to_string(), true, false);
    } else {
        for (i, url) in urls.iter().enumerate() {
            browser.open_tab(normalize_cli_url(url), i == 0, false);
        }
    }
    BROWSER.with(|b| *b.borrow_mut() = Some(browser));

    create_environment()?;
    unsafe {
        SetTimer(Some(hwnd), TIMER_SUSPEND, SUSPEND_CHECK_MS, None);
    }
    Ok(())
}

pub fn shutdown() {
    BROWSER.with(|b| {
        if let Ok(mut guard) = b.try_borrow_mut() {
            if let Some(browser) = guard.take() {
                browser.close_all();
            }
        }
    });
}

fn create_environment() -> Result<()> {
    let user_data = user_data_dir();
    let options: ICoreWebView2EnvironmentOptions = CoreWebView2EnvironmentOptions::default().into();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(|hr, env| {
        post(Event::EnvironmentReady(
            hr.and_then(|_| env.ok_or_else(|| Error::from(E_POINTER))),
        ));
        Ok(())
    }));
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &HSTRING::from(user_data),
            &options,
            &handler,
        )
    }
}

fn user_data_dir() -> String {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base)
        .join("Singularity")
        .join("User Data")
        .to_string_lossy()
        .into_owned()
}

/// The UI folder ships next to the exe; during development it's read straight from the
/// source tree, so editing ui/*.html|css|js only needs a Ctrl+Shift+F5 (reload UI), not a rebuild.
fn ui_dir() -> String {
    let next_to_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ui")))
        .filter(|d| d.join("chrome.html").exists());
    next_to_exe
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"))
        .to_string_lossy()
        .into_owned()
}

fn normalize_cli_url(url: &str) -> String {
    if url.contains("://") || url.starts_with("about:") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

struct Tab {
    id: TabId,
    controller: Option<ICoreWebView2Controller>,
    view: Option<ICoreWebView2>,
    /// URL to load once the controller exists.
    pending_url: Option<String>,
    url: String,
    title: String,
    favicon: String,
    loading: bool,
    can_back: bool,
    can_forward: bool,
    crashed: bool,
    last_active: Instant,
    suspended: bool,
    suspending: bool,
}

struct FullscreenRestore {
    style: isize,
    placement: WINDOWPLACEMENT,
}

pub struct Browser {
    hwnd: HWND,
    palette: Palette,
    env: Option<ICoreWebView2Environment>,
    ui: Option<ICoreWebView2Controller>,
    ui_view: Option<ICoreWebView2>,
    ui_ready: bool,
    ui_dirty: bool,
    focus_address_when_ready: bool,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    next_id: TabId,
    closed: Vec<String>,
    fullscreen: Option<FullscreenRestore>,
    minimized: bool,
    suspend_after: Duration,
    ui_dir: String,
}

impl Browser {
    fn handle(&mut self, event: Event) {
        match event {
            Event::EnvironmentReady(Ok(env)) => {
                self.env = Some(env);
                self.create_ui_controller();
                let pending: Vec<TabId> = self.tabs.iter().map(|t| t.id).collect();
                for id in pending {
                    self.create_tab_controller(id);
                }
            }
            Event::EnvironmentReady(Err(e)) => fatal(&format!(
                "Could not start the Microsoft Edge WebView2 runtime.\n\n{e}\n\n\
                 Install it from https://developer.microsoft.com/microsoft-edge/webview2/"
            )),
            Event::UiControllerReady(Ok(c)) => {
                if let Err(e) = self.setup_ui(c) {
                    fatal(&format!("Could not create the browser UI: {e}"));
                }
            }
            Event::UiControllerReady(Err(e)) => fatal(&format!("Could not create the browser UI: {e}")),
            Event::TabControllerReady(id, Ok(c)) => {
                if let Err(e) = self.setup_tab(id, c) {
                    log(&format!("tab {id}: setup failed: {e}"));
                }
            }
            Event::TabControllerReady(id, Err(e)) => log(&format!("tab {id}: controller failed: {e}")),
            Event::UiMessage(msg) => self.on_ui_message(&msg),
            Event::TabUpdated(id) => self.refresh_tab(id),
            Event::TabLoading(id, loading) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.loading = loading;
                    if loading {
                        tab.crashed = false;
                    }
                }
                self.refresh_tab(id);
            }
            Event::OpenInNewTab(url) => {
                self.open_tab(url, true, true);
            }
            Event::TabCloseRequested(id) => self.close_tab(id),
            Event::TabFullscreenChanged(id) => self.on_fullscreen_changed(id),
            Event::TabSuspended(id, ok) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.suspending = false;
                    tab.suspended = ok;
                    if !ok {
                        // Page refused (e.g. playing audio); try again after another idle period.
                        tab.last_active = Instant::now();
                    }
                }
                self.ui_dirty = true;
            }
            Event::TabCrashed(id) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.crashed = true;
                    tab.loading = false;
                    tab.title = "Page crashed – press F5 to reload".into();
                }
                self.ui_dirty = true;
            }
            Event::Command(cmd) => self.run(cmd),
            Event::Resize(minimized) => self.on_resize(minimized),
        }
    }

    // ---------------------------------------------------------------- UI webview

    fn create_ui_controller(&self) {
        let Some(env) = &self.env else { return };
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(|hr, c| {
            post(Event::UiControllerReady(
                hr.and_then(|_| c.ok_or_else(|| Error::from(E_POINTER))),
            ));
            Ok(())
        }));
        if let Err(e) = unsafe { env.CreateCoreWebView2Controller(self.hwnd, &handler) } {
            fatal(&format!("Could not create the browser UI: {e}"));
        }
    }

    fn setup_ui(&mut self, controller: ICoreWebView2Controller) -> Result<()> {
        unsafe {
            let view = controller.CoreWebView2()?;
            let settings = view.Settings()?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetIsZoomControlEnabled(false)?;
            if !cfg!(debug_assertions) {
                settings.SetAreDefaultContextMenusEnabled(false)?;
                settings.SetAreDevToolsEnabled(false)?;
            }
            self.set_background(&controller);

            view.cast::<ICoreWebView2_3>()?.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(UI_HOST),
                &HSTRING::from(&self.ui_dir),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
            )?;

            let mut token = 0i64;
            view.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                    if let Some(args) = args {
                        let mut source = PWSTR::null();
                        args.Source(&mut source)?;
                        if !take_pwstr(source).starts_with(UI_URL) {
                            return Ok(());
                        }
                        let mut msg = PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut msg).is_ok() {
                            post(Event::UiMessage(take_pwstr(msg)));
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
                        if !uri.starts_with(&format!("https://{UI_HOST}/")) {
                            args.SetCancel(true)?;
                            post(Event::OpenInNewTab(uri));
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
                        post(Event::OpenInNewTab(take_pwstr(uri)));
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            register_accelerators(&controller)?;
            view.Navigate(&HSTRING::from(UI_URL))?;
            controller.SetIsVisible(true)?;
            self.ui_view = Some(view);
        }
        self.ui = Some(controller);
        self.layout();
        Ok(())
    }

    fn set_background(&self, controller: &ICoreWebView2Controller) {
        let (r, g, b) = self.palette.strip_rgb();
        if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
            let _ = unsafe { c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: r, G: g, B: b }) };
        }
    }

    fn on_ui_message(&mut self, raw: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(raw) else {
            log(&format!("bad UI message: {raw}"));
            return;
        };
        let id = msg["id"].as_u64().map(|v| v as TabId);
        match msg["cmd"].as_str().unwrap_or_default() {
            "ready" => {
                self.ui_ready = true;
                self.ui_dirty = true;
                if std::mem::take(&mut self.focus_address_when_ready) {
                    self.focus_address();
                }
            }
            "navigate" => {
                if let Some(url) = msg["url"].as_str() {
                    self.navigate_active(url.to_string());
                }
            }
            "back" => self.run(Command::Back),
            "forward" => self.run(Command::Forward),
            "reload" => self.run(Command::Reload),
            "stop" => {
                if let Some(view) = self.active_view() {
                    let _ = unsafe { view.Stop() };
                }
            }
            "newTab" => self.run(Command::NewTab),
            "reopenTab" => self.run(Command::ReopenClosedTab),
            "closeTab" => {
                if let Some(id) = id {
                    self.close_tab(id);
                }
            }
            "activate" => {
                if let Some(id) = id {
                    self.activate(id);
                    self.focus_content();
                }
            }
            "moveTab" => {
                if let (Some(id), Some(to)) = (id, msg["to"].as_u64()) {
                    self.move_tab(id, to as usize);
                }
            }
            "focusContent" => self.focus_content(),
            "reloadUi" => {
                if let Some(view) = &self.ui_view {
                    self.ui_ready = false;
                    let _ = unsafe { view.Reload() };
                }
            }
            other => log(&format!("unknown UI command: {other}")),
        }
    }

    /// Send the whole tab list to the UI, at most once per batch of events.
    fn flush_ui_state(&mut self) {
        if !self.ui_dirty || !self.ui_ready {
            return;
        }
        let Some(view) = &self.ui_view else { return };
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "url": if t.url == NEW_TAB_URL { "" } else { t.url.as_str() },
                    "favicon": t.favicon,
                    "loading": t.loading,
                    "canBack": t.can_back,
                    "canForward": t.can_forward,
                    "suspended": t.suspended,
                    "crashed": t.crashed,
                    "newTab": t.url == NEW_TAB_URL || t.url.is_empty(),
                })
            })
            .collect();
        let state = json!({ "type": "state", "active": self.active, "tabs": tabs });
        let _ = unsafe { view.PostWebMessageAsJson(&HSTRING::from(state.to_string())) };
        self.ui_dirty = false;
    }

    fn send_ui(&self, message: Value) {
        if let (true, Some(view)) = (self.ui_ready, &self.ui_view) {
            let _ = unsafe { view.PostWebMessageAsJson(&HSTRING::from(message.to_string())) };
        }
    }

    // ---------------------------------------------------------------- tabs

    fn open_tab(&mut self, url: String, activate: bool, next_to_active: bool) -> TabId {
        let id = self.next_id;
        self.next_id += 1;
        let tab = Tab {
            id,
            controller: None,
            view: None,
            pending_url: Some(url.clone()),
            url,
            title: String::new(),
            favicon: String::new(),
            loading: true,
            can_back: false,
            can_forward: false,
            crashed: false,
            last_active: Instant::now(),
            suspended: false,
            suspending: false,
        };
        let pos = match (next_to_active, self.active_index()) {
            (true, Some(i)) => i + 1,
            _ => self.tabs.len(),
        };
        self.tabs.insert(pos, tab);
        if self.env.is_some() {
            self.create_tab_controller(id);
        }
        if activate {
            self.activate(id);
        }
        self.ui_dirty = true;
        id
    }

    fn create_tab_controller(&self, id: TabId) {
        let Some(env) = &self.env else { return };
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, c| {
            post(Event::TabControllerReady(
                id,
                hr.and_then(|_| c.ok_or_else(|| Error::from(E_POINTER))),
            ));
            Ok(())
        }));
        if let Err(e) = unsafe { env.CreateCoreWebView2Controller(self.hwnd, &handler) } {
            log(&format!("tab {id}: CreateCoreWebView2Controller failed: {e}"));
        }
    }

    fn setup_tab(&mut self, id: TabId, controller: ICoreWebView2Controller) -> Result<()> {
        if self.tab(id).is_none() {
            // Closed before its webview finished starting.
            let _ = unsafe { controller.Close() };
            return Ok(());
        }
        self.set_background(&controller);
        let view = unsafe { controller.CoreWebView2()? };
        unsafe {
            // Lets the built-in new tab page load; DENY_CORS keeps web pages from reading it.
            view.cast::<ICoreWebView2_3>()?.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(UI_HOST),
                &HSTRING::from(&self.ui_dir),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS,
            )?;
            register_tab_events(id, &view)?;
            register_accelerators(&controller)?;
        }

        let active = self.active == Some(id);
        let bounds = self.content_bounds();
        unsafe {
            controller.SetBounds(bounds)?;
            controller.SetIsVisible(active && !self.minimized)?;
            if !active {
                set_memory_target(&view, false);
            }
        }

        let tab = self.tab_mut(id).expect("checked above");
        let url = tab.pending_url.take().unwrap_or_else(|| NEW_TAB_URL.into());
        tab.controller = Some(controller);
        tab.view = Some(view.clone());
        if let Err(e) = unsafe { view.Navigate(&HSTRING::from(url.as_str())) } {
            log(&format!("tab {id}: navigate to {url} failed: {e}"));
        }
        if active && url != NEW_TAB_URL {
            self.focus_content();
        }
        Ok(())
    }

    fn activate(&mut self, id: TabId) {
        if self.active == Some(id) || self.tab(id).is_none() {
            return;
        }
        if self.fullscreen.is_some() {
            self.set_fullscreen(false);
        }
        let previous = self.active.replace(id);
        let bounds = self.content_bounds();
        let minimized = self.minimized;
        let now = Instant::now();

        // Show the new tab before hiding the old one so there's never an empty frame.
        if let Some(tab) = self.tab_mut(id) {
            tab.last_active = now;
            tab.suspended = false;
            if let (Some(c), Some(v)) = (&tab.controller, &tab.view) {
                unsafe {
                    let _ = c.SetBounds(bounds);
                    let _ = c.SetIsVisible(!minimized); // also resumes a suspended page
                }
                set_memory_target(v, true);
            }
        }
        if let Some(prev) = previous.and_then(|p| self.tab_mut(p)) {
            prev.last_active = now;
            if let (Some(c), Some(v)) = (&prev.controller, &prev.view) {
                let _ = unsafe { c.SetIsVisible(false) };
                set_memory_target(v, false);
            }
        }
        self.update_window_title();
        self.ui_dirty = true;
    }

    fn close_tab(&mut self, id: TabId) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let tab = self.tabs.remove(index);
        if !tab.url.is_empty() && tab.url != NEW_TAB_URL {
            self.closed.push(tab.url.clone());
            if self.closed.len() > MAX_CLOSED_TABS {
                self.closed.remove(0);
            }
        }
        if let Some(c) = &tab.controller {
            let _ = unsafe { c.Close() };
        }
        if self.active == Some(id) {
            self.active = None;
            if self.fullscreen.is_some() {
                self.set_fullscreen(false);
            }
            let next = self.tabs.get(index).or_else(|| self.tabs.last()).map(|t| t.id);
            match next {
                Some(next) => {
                    self.activate(next);
                    self.focus_content();
                }
                None => unsafe {
                    // Closing the last tab closes the window, like other browsers.
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                },
            }
        }
        self.ui_dirty = true;
    }

    fn move_tab(&mut self, id: TabId, to: usize) {
        let Some(from) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let tab = self.tabs.remove(from);
        let to = to.min(self.tabs.len());
        self.tabs.insert(to, tab);
        self.ui_dirty = true;
    }

    fn navigate_active(&mut self, url: String) {
        let Some(id) = self.active else { return };
        let Some(tab) = self.tab_mut(id) else { return };
        match (&tab.view, &tab.controller) {
            (Some(view), Some(controller)) => {
                if let Err(e) = unsafe { view.Navigate(&HSTRING::from(url.as_str())) } {
                    log(&format!("navigate to {url} failed: {e}"));
                    return;
                }
                // Focus the page directly: `focus_content` would still see the new-tab URL
                // (the tab's URL only updates once navigation starts) and refocus the address bar.
                let _ = unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
            }
            _ => tab.pending_url = Some(url),
        }
    }

    /// Pull the latest URL, title, history state and favicon from a tab's webview.
    fn refresh_tab(&mut self, id: TabId) {
        let Some(tab) = self.tab_mut(id) else { return };
        let Some(view) = tab.view.clone() else { return };
        unsafe {
            let mut s = PWSTR::null();
            if view.Source(&mut s).is_ok() {
                tab.url = take_pwstr(s);
            }
            let mut s = PWSTR::null();
            if view.DocumentTitle(&mut s).is_ok() && !tab.crashed {
                tab.title = take_pwstr(s);
            }
            let mut b = BOOL::default();
            if view.CanGoBack(&mut b).is_ok() {
                tab.can_back = b.as_bool();
            }
            let mut b = BOOL::default();
            if view.CanGoForward(&mut b).is_ok() {
                tab.can_forward = b.as_bool();
            }
            if let Ok(v15) = view.cast::<ICoreWebView2_15>() {
                let mut s = PWSTR::null();
                if v15.FaviconUri(&mut s).is_ok() {
                    tab.favicon = take_pwstr(s);
                }
            }
        }
        if self.active == Some(id) {
            self.update_window_title();
        }
        self.ui_dirty = true;
    }

    fn run(&mut self, cmd: Command) {
        match cmd {
            Command::NewTab => {
                self.open_tab(NEW_TAB_URL.into(), true, false);
                self.focus_address();
            }
            Command::CloseTab => {
                if let Some(id) = self.active {
                    self.close_tab(id);
                }
            }
            Command::ReopenClosedTab => {
                if let Some(url) = self.closed.pop() {
                    self.open_tab(url, true, false);
                }
            }
            Command::FocusAddress => self.focus_address(),
            Command::NextTab | Command::PrevTab => {
                let n = self.tabs.len();
                if let (Some(i), true) = (self.active_index(), n > 1) {
                    let j = if matches!(cmd, Command::NextTab) { (i + 1) % n } else { (i + n - 1) % n };
                    let id = self.tabs[j].id;
                    self.activate(id);
                    self.focus_content();
                }
            }
            Command::SelectTab(i) => {
                let target = if i == usize::MAX { self.tabs.last() } else { self.tabs.get(i) };
                if let Some(id) = target.map(|t| t.id) {
                    self.activate(id);
                    self.focus_content();
                }
            }
            Command::Reload => {
                if let Some(view) = self.active_view() {
                    let _ = unsafe { view.Reload() };
                }
            }
            Command::Back => {
                if let Some(view) = self.active_view() {
                    let _ = unsafe { view.GoBack() };
                }
            }
            Command::Forward => {
                if let Some(view) = self.active_view() {
                    let _ = unsafe { view.GoForward() };
                }
            }
        }
    }

    // ---------------------------------------------------------------- focus

    fn focus_address(&mut self) {
        match (&self.ui, self.ui_ready) {
            (Some(ui), true) => {
                let _ = unsafe { ui.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
                self.send_ui(json!({ "type": "focusAddress" }));
            }
            _ => self.focus_address_when_ready = true,
        }
    }

    pub fn focus_content(&mut self) {
        let Some(tab) = self.active.and_then(|id| self.tab(id)) else { return };
        if tab.url == NEW_TAB_URL || tab.pending_url.as_deref() == Some(NEW_TAB_URL) {
            // A blank tab is for typing an address.
            self.focus_address();
            return;
        }
        if let Some(c) = &tab.controller {
            let _ = unsafe { c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
        }
    }

    // ---------------------------------------------------------------- window

    fn ui_height(&self) -> i32 {
        if self.fullscreen.is_some() {
            return 0;
        }
        let dpi = unsafe { GetDpiForWindow(self.hwnd) } as i32;
        UI_HEIGHT_DIP * dpi / 96
    }

    fn client_rect(&self) -> RECT {
        let mut rc = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut rc) };
        rc
    }

    fn content_bounds(&self) -> RECT {
        let rc = self.client_rect();
        RECT { left: 0, top: self.ui_height(), right: rc.right, bottom: rc.bottom }
    }

    fn layout(&mut self) {
        let rc = self.client_rect();
        let ui_height = self.ui_height();
        unsafe {
            if let Some(ui) = &self.ui {
                let _ = ui.SetBounds(RECT { left: 0, top: 0, right: rc.right, bottom: ui_height });
                let _ = ui.SetIsVisible(ui_height > 0 && !self.minimized);
            }
            if let Some(c) = self.active.and_then(|id| self.tab(id)).and_then(|t| t.controller.as_ref()) {
                let _ = c.SetBounds(self.content_bounds());
            }
        }
    }

    pub fn on_resize(&mut self, minimized: bool) {
        if minimized != self.minimized {
            self.minimized = minimized;
            // Hidden webviews stop rendering, which saves CPU and battery while minimized.
            if let Some(c) = self.active.and_then(|id| self.tab(id)).and_then(|t| t.controller.as_ref()) {
                let _ = unsafe { c.SetIsVisible(!minimized) };
            }
        }
        if !minimized {
            self.layout();
        }
    }

    pub fn on_move(&mut self) {
        // Keeps dropdowns, tooltips and IME windows positioned correctly.
        unsafe {
            if let Some(ui) = &self.ui {
                let _ = ui.NotifyParentWindowPositionChanged();
            }
            if let Some(c) = self.active.and_then(|id| self.tab(id)).and_then(|t| t.controller.as_ref()) {
                let _ = c.NotifyParentWindowPositionChanged();
            }
        }
    }

    fn update_window_title(&self) {
        let title = match self.active.and_then(|id| self.tab(id)) {
            Some(t) if !t.title.is_empty() && t.url != NEW_TAB_URL => format!("{} - Singularity", t.title),
            _ => "Singularity".to_string(),
        };
        let _ = unsafe { SetWindowTextW(self.hwnd, &HSTRING::from(title)) };
    }

    fn on_fullscreen_changed(&mut self, id: TabId) {
        if self.active != Some(id) {
            return;
        }
        let Some(view) = self.active_view() else { return };
        let mut b = BOOL::default();
        if unsafe { view.ContainsFullScreenElement(&mut b) }.is_ok() {
            self.set_fullscreen(b.as_bool());
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        if on == self.fullscreen.is_some() {
            return;
        }
        let hwnd = self.hwnd;
        unsafe {
            if on {
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
        self.layout();
    }

    // ---------------------------------------------------------------- memory

    /// Freeze background tabs that have been idle for a while. A suspended tab keeps its
    /// page in memory but stops running script and timers, and WebView2 can trim its
    /// memory further; it resumes instantly when shown again.
    pub fn on_timer(&mut self, timer_id: usize) {
        if timer_id != TIMER_SUSPEND {
            return;
        }
        let now = Instant::now();
        let active = self.active;
        let suspend_after = self.suspend_after;
        for tab in &mut self.tabs {
            let idle = now.duration_since(tab.last_active) >= suspend_after;
            if Some(tab.id) == active || tab.suspended || tab.suspending || !idle {
                continue;
            }
            let Some(v3) = tab.view.as_ref().and_then(|v| v.cast::<ICoreWebView2_3>().ok()) else {
                continue;
            };
            let id = tab.id;
            let handler = TrySuspendCompletedHandler::create(Box::new(move |hr, ok| {
                post(Event::TabSuspended(id, hr.is_ok() && ok));
                Ok(())
            }));
            tab.suspending = unsafe { v3.TrySuspend(&handler) }.is_ok();
        }
    }

    fn close_all(self) {
        unsafe {
            for tab in &self.tabs {
                if let Some(c) = &tab.controller {
                    let _ = c.Close();
                }
            }
            if let Some(ui) = &self.ui {
                let _ = ui.Close();
            }
        }
    }

    // ---------------------------------------------------------------- helpers

    fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active?;
        self.tabs.iter().position(|t| t.id == id)
    }

    fn active_view(&self) -> Option<ICoreWebView2> {
        self.active.and_then(|id| self.tab(id)).and_then(|t| t.view.clone())
    }
}

/// Background tabs ask WebView2 to keep memory low; the visible tab gets normal limits.
fn set_memory_target(view: &ICoreWebView2, visible: bool) {
    if let Ok(v19) = view.cast::<ICoreWebView2_19>() {
        let level = if visible {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        } else {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        };
        let _ = unsafe { v19.SetMemoryUsageTargetLevel(level) };
    }
}

unsafe fn register_tab_events(id: TabId, view: &ICoreWebView2) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        view.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_DocumentTitleChanged(
            &DocumentTitleChangedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_HistoryChanged(
            &HistoryChangedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabUpdated(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, _| {
                post(Event::TabLoading(id, true));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabLoading(id, false));
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
                    post(Event::OpenInNewTab(take_pwstr(uri)));
                }
                Ok(())
            })),
            &mut token,
        )?;
        view.add_WindowCloseRequested(
            &WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabCloseRequested(id));
                Ok(())
            })),
            &mut token,
        )?;
        view.add_ContainsFullScreenElementChanged(
            &ContainsFullScreenElementChangedEventHandler::create(Box::new(move |_, _| {
                post(Event::TabFullscreenChanged(id));
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
                        post(Event::TabCrashed(id));
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        if let Ok(v15) = view.cast::<ICoreWebView2_15>() {
            v15.add_FaviconChanged(
                &FaviconChangedEventHandler::create(Box::new(move |_, _| {
                    post(Event::TabUpdated(id));
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
                    post(Event::Command(cmd));
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
        (true, false, false, VK_L) | (false, false, true, VK_D) | (false, false, false, VK_F6) => FocusAddress,
        (true, false, false, VK_TAB) | (true, false, false, VK_NEXT) => NextTab,
        (true, true, false, VK_TAB) | (true, false, false, VK_PRIOR) => PrevTab,
        (true, false, false, VK_R) | (false, false, false, VK_F5) => Reload,
        (false, false, true, VK_LEFT) => Back,
        (false, false, true, VK_RIGHT) => Forward,
        (true, false, false, k) if (0x31..=0x38).contains(&k.0) => SelectTab((k.0 - 0x31) as usize),
        (true, false, false, k) if k.0 == 0x39 => SelectTab(usize::MAX),
        _ => return None,
    })
}

fn log(msg: &str) {
    if cfg!(debug_assertions) {
        eprintln!("[singularity] {msg}");
    }
}

fn fatal(msg: &str) {
    log(msg);
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("Singularity"), MB_OK | MB_ICONERROR);
        PostQuitMessage(1);
    }
}
