//! Browser state, the same on every platform: two UI webviews written in HTML under `ui/`
//! (the sidebar, and the command bar that floats over the page), and one webview per tab.
//! Everything native (the window, its panes, timers, the webviews themselves) goes through
//! `platform::Host` and `platform::View`: WebView2 and Win32 on Windows (`src/win`), WebKit and
//! AppKit on macOS (`src/mac`).
//!
//! Layout: the sidebar sits on the left; the active tab is a rounded card to its right.
//! Each lives in its own "pane" (a child window on Windows, a view on macOS), so the sidebar
//! can slide out over the page when it's hidden ("peek"). In split view a second card pane sits
//! beside the first, and the right-hand tab's webview is moved into it. The command bar floats
//! above everything.
//!
//! Tabs belong to spaces; the sidebar shows the current space's pinned tabs, then its
//! "today" tabs. Pinned tabs remember their own page and only unload when closed. Today tabs
//! left unseen for a while (12 hours) are archived: closed, and kept in the library with
//! history and bookmarks (`library.rs`), which the command bar searches.
//!
//! No flicker: the UI webviews never stop rendering. A hidden sidebar is only moved out of
//! sight, and a closed command bar is only parked out of sight, so showing them never shows an
//! empty frame. The command bar is revealed only once its page says it has drawn the new content.
//! Panes move together in one batch, and nothing is re-set unless it changed.
//!
//! Threading model: everything runs on the UI thread. Webview callbacks never touch
//! `Browser` directly; they push an [`Event`] onto a queue and wake the event loop, which
//! drains the queue. This avoids re-entrancy problems when a webview call fires another
//! event synchronously.

use std::{
    cell::RefCell,
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::{
    library::{Library, Page},
    platform::{self, COMMAND_URL, Host, NEW_TAB_URL, UI_ORIGIN, UI_URL, View},
    session,
    theme::{Palette, Rgb},
};

/// Gap around the page card, in device-independent pixels.
pub const CARD_MARGIN_DIP: i32 = 8;
/// Corner radius of the page card.
pub const CARD_RADIUS_DIP: i32 = 8;
/// Sidebar width in device-independent pixels; the user can drag its edge.
const SIDEBAR_DEFAULT_DIP: i32 = 252;
const SIDEBAR_MIN_DIP: i32 = 200;
const SIDEBAR_MAX_DIP: i32 = 420;
const TIMER_SUSPEND: TimerId = 1;
/// Watches the cursor while the hidden sidebar is slid out.
const TIMER_PEEK: TimerId = 2;
const TIMER_SESSION: TimerId = 3;
/// Shows the command bar even if its page never confirmed it had drawn.
const TIMER_PRESENT: TimerId = 4;
/// Hides the sidebar even if the page never confirmed it had drawn at its new size.
const TIMER_HIDE: TimerId = 5;
const HIDE_TIMEOUT_MS: u32 = 150;
/// Prefix of the message a page sends (through a script we run) once it has drawn a frame.
pub const FRAME_MESSAGE: &str = "__browser_frame_";
const PEEK_TICK_MS: u32 = 10;
const SESSION_SAVE_MS: u32 = 2000;
const PRESENT_TIMEOUT_MS: u32 = 250;
const SLIDE_SHOW: Duration = Duration::from_millis(200);
const SLIDE_HIDE: Duration = Duration::from_millis(170);
/// How long the cursor may stray outside the slid-out sidebar before it slides back.
const PEEK_LINGER: Duration = Duration::from_millis(300);
/// Command bar size, in device-independent pixels (the height follows its content).
const COMMAND_WIDTH_DIP: i32 = 680;
const COMMAND_MIN_HEIGHT_DIP: i32 = 58;
/// The command bar's webview is always this tall, so a longer list of results never has to
/// be laid out again while it shows; the window around it just reveals more of it.
const COMMAND_MAX_HEIGHT_DIP: i32 = 520;
const SUSPEND_CHECK_MS: u32 = 15_000;
/// Background tabs idle this long are frozen (WebView2's `TrySuspend`; override with
/// the `BROWSER_SUSPEND_SECS` environment variable).
const DEFAULT_SUSPEND_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_CLOSED_TABS: usize = 25;
/// Today tabs not seen for this long are archived (override with `BROWSER_ARCHIVE_SECS`).
const DEFAULT_ARCHIVE_AFTER_SECS: u64 = 12 * 60 * 60;
/// History changes with every page, so it's written at most this often (and on exit).
const LIBRARY_SAVE_EVERY: Duration = Duration::from_secs(10);
/// History and bookmark results in the command bar (a few extra, since the bar drops any
/// that are already open as tabs).
const COMMAND_SUGGESTIONS: usize = 10;
const ARCHIVE_RESULTS: usize = 40;
/// Space colors; the sidebar offers the same list (`SPACE_COLORS` in ui/chrome.js).
const SPACE_COLORS: [&str; 8] = ["#5b6cf0", "#e5484d", "#30a46c", "#f08c2e", "#8e4ec6", "#0d9fb8", "#d6409f", "#71717a"];
const MAX_SPACE_NAME: usize = 40;
const WHITE: Rgb = (255, 255, 255);
/// Reads the page's background color, so the frame or two before a resized page is drawn
/// again is filled with a matching color instead of a contrasting strip.
/// Many sites paint their background on a full-size element rather than <body>, so this looks
/// at what's behind the right edge first (where a growing page opens up), skipping small things.
const BACKGROUND_SCRIPT: &str = r#"(() => {
  const solid = (c) => /^rgb\(/.test(c) || /^rgba\(.*,\s*1\)$/.test(c);
  const behind = (e, minHeight) => {
    for (; e && e.nodeType === 1; e = e.parentElement) {
      const c = getComputedStyle(e).backgroundColor;
      if (solid(c) && e.getBoundingClientRect().height >= minHeight) return c;
    }
    return "";
  };
  const edge = document.elementFromPoint(Math.max(0, innerWidth - 4), innerHeight / 2);
  return behind(edge, innerHeight / 2) || behind(document.body, 0) || behind(document.documentElement, 0) ||
    (matchMedia("(prefers-color-scheme: dark)").matches &&
      /dark/.test(getComputedStyle(document.documentElement).colorScheme) ? "rgb(18, 18, 18)" : "rgb(255, 255, 255)");
})()"#;

pub type TabId = u32;
type SpaceId = u32;
pub type TimerId = usize;

/// A rectangle in physical pixels (Windows) or points (macOS), relative to the window's
/// client area unless said otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// What a tab's webview reports about its page. Each field is `None` if it couldn't be read
/// (or the platform can't tell).
#[derive(Default)]
pub struct PageState {
    pub url: Option<String>,
    pub title: Option<String>,
    pub can_back: Option<bool>,
    pub can_forward: Option<bool>,
    pub audible: Option<bool>,
    pub muted: Option<bool>,
    pub favicon: Option<String>,
}

/// Not every engine reports everything: WebKit (macOS) has no crash, `window.close()`, full
/// screen or suspend events here, and tab pages there can't send the frame signal.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub enum Event {
    EnvironmentReady(Result<platform::Env, String>),
    UiViewReady(Surface, Result<View, String>),
    TabViewReady(TabId, Result<View, String>),
    UiMessage(Surface, String),
    TabUpdated(TabId),
    TabNavigating(TabId, String),
    TabLoaded(TabId),
    TabBackground(TabId, String),
    /// The page's icon URL as JSON, from `platform::FAVICON_SCRIPT`.
    TabFavicon(TabId, String),
    /// A page posted FRAME_MESSAGE + nonce.
    TabFramed(String),
    OpenInNewTab(String),
    TabCloseRequested(TabId),
    TabFullscreenChanged(TabId),
    TabSuspended(TabId, bool),
    TabCrashed(TabId),
    /// A tab's webview took keyboard focus (in split view, a click on the other side).
    TabFocused(TabId),
    Command(Command),
    CommandBarDeactivated,
    Resize,
}

/// The browser's own HTML surfaces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Surface {
    Sidebar,
    CommandBar,
}

impl Surface {
    pub fn url(self) -> &'static str {
        match self {
            Surface::Sidebar => UI_URL,
            Surface::CommandBar => COMMAND_URL,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    NewTab,
    CloseTab,
    ReopenClosedTab,
    /// Ctrl+L: the command bar, editing the current page's address.
    EditAddress,
    NextTab,
    PrevTab,
    /// 0-based index; `usize::MAX` selects the last tab.
    SelectTab(usize),
    Reload,
    Back,
    Forward,
    ToggleSidebar,
    ToggleFullscreen,
    /// Ctrl+D: bookmark the page, or remove its bookmark.
    ToggleBookmark,
    /// Ctrl+\: show the tab beside the last one used, or separate a split.
    ToggleSplit,
    /// Alt+1…9: 0-based index into the spaces.
    SelectSpace(usize),
}

/// Window commands from the UI. They run after the current batch of events, once `Browser`
/// is no longer borrowed: minimize/maximize/move re-enter the event handling (and moving
/// runs a modal loop on Windows).
pub enum WindowAction {
    Minimize,
    Maximize,
    Restore,
    Close,
    /// Start moving the window from where the cursor is (the sidebar's top row).
    Drag,
}

/// What the command bar was opened for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CommandMode {
    /// Ctrl+T: what you pick opens in a new tab.
    NewTab,
    /// Ctrl+L: edit the current page's address.
    Current,
    /// Search archived tabs and reopen one.
    Archive,
}

impl CommandMode {
    fn name(self) -> &'static str {
        match self {
            CommandMode::NewTab => "newTab",
            CommandMode::Current => "current",
            CommandMode::Archive => "archive",
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
    static WINDOW_ACTIONS: RefCell<Vec<WindowAction>> = const { RefCell::new(Vec::new()) };
}

/// Queue an event for the UI thread. Safe to call from any webview callback.
pub fn post(event: Event) {
    QUEUE.with(|q| q.borrow_mut().push_back(event));
    platform::wake();
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
            // Re-entered while the browser was busy: try again on the next wakeup.
            QUEUE.with(|q| q.borrow_mut().push_front(event));
            platform::wake();
            return;
        }
    }
    with_browser(|b| b.flush_ui_state());
    let actions = WINDOW_ACTIONS.with(|a| std::mem::take(&mut *a.borrow_mut()));
    platform::run_window_actions(actions);
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
        post(Event::Resize);
    }
}

/// Returns whether the window should open maximized (as it was when last closed).
pub fn start(host: Host, urls: Vec<String>, palette: Palette) -> Result<bool, String> {
    let env_secs = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    let suspend_after = env_secs("BROWSER_SUSPEND_SECS").map(Duration::from_secs).unwrap_or(DEFAULT_SUSPEND_AFTER);
    let mut library = session::load_library();
    library.prune(now_secs());

    let mut browser = Browser {
        host,
        palette,
        env_ready: false,
        ui: None,
        ui_ready: false,
        ui_dirty: true,
        command: None,
        command_ready: false,
        command_open: false,
        command_shown: false,
        command_pending: None,
        command_height_dip: COMMAND_MIN_HEIGHT_DIP,
        command_bounds: Rect::default(),
        command_mode: CommandMode::NewTab,
        tabs: Vec::new(),
        active: None,
        next_id: 1,
        spaces: Vec::new(),
        space: 0,
        next_space_id: 1,
        split: None,
        closed: Vec::new(),
        library,
        library_saved_at: Instant::now(),
        archive_after: env_secs("BROWSER_ARCHIVE_SECS").unwrap_or(DEFAULT_ARCHIVE_AFTER_SECS),
        fullscreen: false,
        minimized: false,
        maximized: false,
        split_pane_shown: false,
        sidebar_size: (0, 0),
        sidebar_docked: true,
        peek: false,
        slide: None,
        peek_left_at: None,
        peek_armed: true,
        sidebar_width_dip: SIDEBAR_DEFAULT_DIP,
        suspend_after,
        ui_dir: ui_dir(),
        saved_session: String::new(),
        hide_pending: None,
        frame_nonce: 0,
    };

    let mut maximized = false;
    let saved = if urls.is_empty() { session::load() } else { None };
    if let Some(s) = &saved {
        if let Some(w) = s.sidebar_width {
            browser.sidebar_width_dip = w.clamp(SIDEBAR_MIN_DIP, SIDEBAR_MAX_DIP);
        }
        browser.sidebar_docked = s.sidebar_docked.unwrap_or(true);
        if let Some((r, max)) = s.window {
            let rect = Rect { left: r[0], top: r[1], right: r[2], bottom: r[3] };
            if rect.width() > 200 && rect.height() > 200 && browser.host.restore_placement(rect) {
                maximized = max;
            }
        }
    }
    match saved.filter(|s| !s.tabs.is_empty()) {
        Some(s) => browser.restore(s),
        None => {
            browser.add_space(String::new(), String::new());
            if urls.is_empty() {
                browser.open_tab(NEW_TAB_URL.to_string(), true, None);
                // Start with the command bar open, ready to type where to go.
                browser.command_pending = Some(CommandMode::Current);
            } else {
                let mut previous = None;
                for (i, url) in urls.iter().enumerate() {
                    previous = Some(browser.open_tab(normalize_cli_url(url), i == 0, previous));
                }
            }
        }
    }
    browser.saved_session = browser.session_json();
    browser.host.set_timer(TIMER_SUSPEND, SUSPEND_CHECK_MS);
    browser.host.set_timer(TIMER_SESSION, SESSION_SAVE_MS);
    BROWSER.with(|b| *b.borrow_mut() = Some(browser));

    platform::start_environment()?;
    Ok(maximized)
}

pub fn shutdown() {
    BROWSER.with(|b| {
        if let Ok(mut guard) = b.try_borrow_mut() {
            if let Some(mut browser) = guard.take() {
                browser.save_session();
                session::save_library(&mut browser.library);
                browser.close_all();
            }
        }
    });
}

/// The UI folder ships next to the exe (or, on macOS, in the app bundle's Resources folder);
/// during development it's read straight from the source tree, so editing ui/*.html|css|js
/// only needs a Ctrl+Shift+F5 (reload UI), not a rebuild.
fn ui_dir() -> String {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let shipped =
        exe_dir.into_iter().flat_map(|d| [d.join("ui"), d.join("../Resources/ui")]).find(|d| d.join("chrome.html").exists());
    shipped
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

/// "rgb(1, 2, 3)" -> (1, 2, 3)
fn parse_rgb(css: &str) -> Option<Rgb> {
    let inner = css.trim_matches('"').split_once('(')?.1.trim_end_matches(')');
    let mut n = inner.split(',').map(|p| p.trim().parse::<f32>().ok());
    let (r, g, b) = (n.next()??, n.next()??, n.next()??);
    Some((r as u8, g as u8, b as u8))
}

struct Tab {
    id: TabId,
    view: Option<View>,
    /// The webview is being created.
    creating: bool,
    /// URL to load once the webview exists.
    pending_url: Option<String>,
    /// Bounds last given to the webview.
    bounds: Rect,
    url: String,
    title: String,
    favicon: String,
    loading: bool,
    can_back: bool,
    can_forward: bool,
    crashed: bool,
    audible: bool,
    muted: bool,
    last_active: Instant,
    suspended: bool,
    suspending: bool,
    /// The page's background color.
    background: Rgb,
    space: SpaceId,
    /// Pinned: the tab's own page, which it goes back to when closed.
    pin: Option<Page>,
    /// When it was last on screen, in Unix seconds (for archiving; survives restarts).
    last_used: u64,
    /// Which card pane its webview lives in (0, or 1 for the right side of a split).
    pane: usize,
    /// Last URL counted as a visit in history.
    recorded_url: String,
}

impl Tab {
    fn page(&self) -> Page {
        Page::new(&self.url, &self.title, &self.favicon)
    }

    /// A pinned tab whose page isn't loaded (no webview).
    fn unloaded(&self) -> bool {
        self.pin.is_some() && self.view.is_none() && !self.creating
    }
}

struct Space {
    id: SpaceId,
    name: String,
    color: String,
    /// The tab that was showing when you left this space.
    active: Option<TabId>,
}

/// Two tabs side by side. It shows whenever one of them is the active tab.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Split {
    left: TabId,
    right: TabId,
}

impl Split {
    fn has(&self, id: TabId) -> bool {
        self.left == id || self.right == id
    }
}

#[derive(Clone, Copy)]
struct Slide {
    start: Instant,
    showing: bool,
}

impl Slide {
    fn duration(&self) -> Duration {
        if self.showing { SLIDE_SHOW } else { SLIDE_HIDE }
    }
}

pub struct Browser {
    /// The native window, its panes and timers.
    host: Host,
    palette: Palette,
    /// The webview engine has started; webviews can be created.
    env_ready: bool,
    ui: Option<View>,
    ui_ready: bool,
    ui_dirty: bool,
    /// The command bar (Ctrl+T / Ctrl+L), a separate webview floating over the page.
    command: Option<View>,
    command_ready: bool,
    /// Asked to open.
    command_open: bool,
    /// On screen (its page has drawn what it was opened with).
    command_shown: bool,
    /// Opened before its page finished loading.
    command_pending: Option<CommandMode>,
    command_height_dip: i32,
    command_bounds: Rect,
    command_mode: CommandMode,
    /// Every space's tabs. Within a space, pinned and today tabs each keep this order.
    tabs: Vec<Tab>,
    active: Option<TabId>,
    next_id: TabId,
    spaces: Vec<Space>,
    /// The space showing in the sidebar.
    space: SpaceId,
    next_space_id: SpaceId,
    split: Option<Split>,
    /// URLs of tabs closed by hand, for Ctrl+Shift+T.
    closed: Vec<String>,
    library: Library,
    library_saved_at: Instant,
    archive_after: u64,
    fullscreen: bool,
    minimized: bool,
    maximized: bool,
    /// The second card pane (the right side of a split) is shown.
    split_pane_shown: bool,
    sidebar_size: (i32, i32),
    /// Sidebar shown beside the page (false: hidden with Ctrl+S).
    sidebar_docked: bool,
    /// Hidden sidebar temporarily slid out over the page.
    peek: bool,
    /// The sidebar is sliding in or out.
    slide: Option<Slide>,
    /// When the cursor left the slid-out sidebar.
    peek_left_at: Option<Instant>,
    /// False right after the sidebar slides back, until the cursor leaves the window edge;
    /// stops it from sliding straight out again.
    peek_armed: bool,
    sidebar_width_dip: i32,
    suspend_after: Duration,
    ui_dir: String,
    /// Last session written to disk.
    saved_session: String,
    /// Hiding the sidebar is waiting for the page to draw at its wider size (the nonce of
    /// the frame message it waits for).
    hide_pending: Option<u32>,
    frame_nonce: u32,
}

impl Browser {
    fn handle(&mut self, event: Event) {
        match event {
            Event::EnvironmentReady(Ok(env)) => {
                self.host.set_env(env);
                self.env_ready = true;
                self.create_ui_view(Surface::Sidebar);
                self.create_ui_view(Surface::CommandBar);
                let visible = self.visible_tabs();
                let wanted: Vec<TabId> =
                    self.tabs.iter().filter(|t| !t.suspended || visible.contains(&t.id)).map(|t| t.id).collect();
                for id in wanted {
                    self.ensure_webview(id);
                }
            }
            Event::EnvironmentReady(Err(e)) => fatal(&e),
            Event::UiViewReady(surface, Ok(view)) => {
                if let Err(e) = self.setup_ui(surface, view) {
                    fatal(&format!("Could not create the browser UI: {e}"));
                }
            }
            Event::UiViewReady(_, Err(e)) => fatal(&format!("Could not create the browser UI: {e}")),
            Event::TabViewReady(id, Ok(view)) => {
                if let Err(e) = self.setup_tab(id, view) {
                    log(&format!("tab {id}: setup failed: {e}"));
                }
            }
            Event::TabViewReady(id, Err(e)) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.creating = false;
                }
                log(&format!("tab {id}: webview failed: {e}"));
            }
            Event::UiMessage(Surface::Sidebar, msg) => self.on_ui_message(&msg),
            Event::UiMessage(Surface::CommandBar, msg) => self.on_command_message(&msg),
            Event::TabUpdated(id) => self.refresh_tab(id),
            Event::TabNavigating(id, uri) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.loading = true;
                    tab.crashed = false;
                }
                // Until the new page says otherwise, assume the web's default white
                // (pages without a background are drawn on this color).
                let color = if uri.starts_with(UI_ORIGIN) { self.palette.page } else { WHITE };
                self.set_tab_background(id, color);
                self.refresh_tab(id);
            }
            Event::TabLoaded(id) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.loading = false;
                }
                self.read_tab_background(id);
                // Engines without a favicon API (WebKit) ask the page for its icon instead.
                if let (Some(script), Some(view)) = (platform::FAVICON_SCRIPT, self.tab(id).and_then(|t| t.view.clone())) {
                    view.eval_with_result(script, id, Event::TabFavicon);
                }
                self.refresh_tab(id);
            }
            Event::TabBackground(id, css) => {
                if let Some(color) = parse_rgb(&css) {
                    self.set_tab_background(id, color);
                }
            }
            Event::TabFavicon(id, json) => {
                let icon = serde_json::from_str::<String>(&json).unwrap_or_default();
                if let Some(tab) = self.tab_mut(id).filter(|t| t.favicon != icon) {
                    tab.favicon = icon;
                    let page = tab.page();
                    self.library.update_page(&page);
                    self.ui_dirty = true;
                }
            }
            Event::TabFramed(msg) => {
                if self.hide_pending.is_some_and(|n| msg == format!("{FRAME_MESSAGE}{n}")) {
                    self.finish_hide();
                }
            }
            Event::OpenInNewTab(url) => {
                self.open_tab(url, true, self.active);
            }
            Event::TabFocused(id) => {
                // A click into the other side of a split makes that side the active tab.
                if self.active != Some(id) && self.split_shown().is_some_and(|s| s.has(id)) {
                    self.active = Some(id);
                    self.remember_active();
                    self.update_window_title();
                    self.ui_dirty = true;
                }
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
            Event::CommandBarDeactivated => {
                // Ignore the deactivation we cause ourselves when closing it.
                if self.command_open && !self.host.command_bar_active() {
                    self.close_command_bar(false);
                }
            }
            Event::Resize => self.on_resize(),
        }
    }

    // ---------------------------------------------------------------- UI webviews

    fn surface_color(&self, surface: Surface) -> Rgb {
        match surface {
            Surface::Sidebar => self.palette.strip,
            Surface::CommandBar => self.palette.panel,
        }
    }

    fn create_ui_view(&self, surface: Surface) {
        if !self.env_ready {
            return;
        }
        if let Err(e) = self.host.create_ui_view(surface, &self.ui_dir, self.surface_color(surface)) {
            fatal(&format!("Could not create the browser UI: {e}"));
        }
    }

    fn setup_ui(&mut self, surface: Surface, view: View) -> Result<(), String> {
        view.configure_ui(surface, &self.ui_dir, self.surface_color(surface))?;
        view.navigate(surface.url())?;
        // Both stay visible (rendering) for good: hiding a webview drops its picture,
        // and showing it again flashes an empty frame.
        view.set_visible(true);
        match surface {
            Surface::Sidebar => {
                self.ui = Some(view);
                self.sidebar_size = (0, 0);
            }
            Surface::CommandBar => {
                self.command = Some(view);
                self.command_bounds = Rect::default();
            }
        }
        self.layout();
        Ok(())
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
            }
            "openCommandBar" => self.run(Command::EditAddress),
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
                    view.stop();
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
                    self.move_tab(id, to as usize, msg["pinned"].as_bool());
                }
            }
            "pin" | "unpin" => {
                if let Some(id) = id {
                    let pin = msg["cmd"] == "pin";
                    self.move_tab(id, if pin { usize::MAX } else { 0 }, Some(pin));
                }
            }
            "resetPin" => {
                let target = id.and_then(|id| self.tab(id)).and_then(|t| Some((t.view.clone()?, t.pin.clone()?)));
                if let Some((view, pin)) = target {
                    let _ = view.navigate(&pin.url);
                }
            }
            "setPinHere" => {
                if let Some(tab) = id.and_then(|id| self.tab_mut(id)).filter(|t| t.pin.is_some()) {
                    tab.pin = Some(tab.page());
                    self.ui_dirty = true;
                }
            }
            "splitWith" => {
                if let Some(id) = id {
                    self.split_with(id);
                }
            }
            "separate" => self.separate(),
            "toggleSplit" => self.run(Command::ToggleSplit),
            "clearToday" => self.clear_today(),
            "toggleBookmark" => self.run(Command::ToggleBookmark),
            "openArchive" => self.open_command_bar(CommandMode::Archive),
            "switchSpace" => {
                if let Some(space) = msg["space"].as_u64() {
                    self.switch_space(space as SpaceId);
                }
            }
            "newSpace" => {
                let n = self.spaces.len();
                let space = self.add_space(format!("Space {}", n + 1), SPACE_COLORS[n % SPACE_COLORS.len()].into());
                self.switch_space(space);
            }
            "renameSpace" => {
                let name: String = msg["name"].as_str().unwrap_or_default().trim().chars().take(MAX_SPACE_NAME).collect();
                let space = msg["space"].as_u64().map(|s| s as SpaceId);
                if let Some(sp) = space.and_then(|s| self.space_mut(s)).filter(|_| !name.is_empty()) {
                    sp.name = name;
                    self.ui_dirty = true;
                }
            }
            "spaceColor" => {
                let color = msg["color"].as_str().unwrap_or_default().to_string();
                let space = msg["space"].as_u64().map(|s| s as SpaceId);
                if let Some(sp) = space.and_then(|s| self.space_mut(s)).filter(|_| SPACE_COLORS.contains(&color.as_str())) {
                    sp.color = color;
                    self.ui_dirty = true;
                }
            }
            "deleteSpace" => {
                if let Some(space) = msg["space"].as_u64() {
                    self.delete_space(space as SpaceId);
                }
            }
            "moveToSpace" => {
                if let (Some(id), Some(space)) = (id, msg["space"].as_u64()) {
                    self.move_to_space(id, space as SpaceId);
                }
            }
            "toggleMute" => {
                if let Some(view) = id.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()) {
                    view.toggle_mute();
                }
            }
            "focusContent" => self.focus_content(),
            "toggleSidebar" => self.run(Command::ToggleSidebar),
            "sidebarWidth" => {
                if let Some(w) = msg["width"].as_f64() {
                    self.sidebar_width_dip = (w as i32).clamp(SIDEBAR_MIN_DIP, SIDEBAR_MAX_DIP);
                    self.layout();
                }
            }
            "minimize" => self.window_action(WindowAction::Minimize),
            "maximize" => self.window_action(if self.maximized { WindowAction::Restore } else { WindowAction::Maximize }),
            "close" => self.window_action(WindowAction::Close),
            // Fallback for `app-region: drag` (and the only way on macOS): start the native
            // window move from the cursor.
            "dragWindow" => self.window_action(WindowAction::Drag),
            "reloadUi" => {
                self.close_command_bar(false);
                self.ui_ready = false;
                self.command_ready = false;
                for view in [&self.ui, &self.command].into_iter().flatten() {
                    view.reload();
                }
            }
            other => log(&format!("unknown UI command: {other}")),
        }
    }

    // ---------------------------------------------------------------- command bar

    fn on_command_message(&mut self, raw: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(raw) else {
            log(&format!("bad command bar message: {raw}"));
            return;
        };
        match msg["cmd"].as_str().unwrap_or_default() {
            "ready" => {
                self.command_ready = true;
                if let Some(mode) = self.command_pending.take() {
                    self.open_command_bar(mode);
                }
            }
            // Typed text: answer with matching bookmarks and history (or archived tabs).
            "query" => {
                let text = msg["text"].as_str().unwrap_or_default();
                let reply = json!({
                    "type": "suggestions",
                    "seq": msg["seq"],
                    "items": self.suggestions(self.command_mode, text),
                });
                if let Some(view) = &self.command {
                    view.post_json(&reply.to_string());
                }
            }
            "restore" => {
                let Some(url) = msg["url"].as_str().map(str::to_string) else { return };
                self.close_command_bar(false);
                self.library.unarchive(&url);
                self.open_tab(url, true, None);
            }
            // The page has drawn what it was opened with: reveal it.
            "present" => {
                if let Some(h) = msg["height"].as_f64() {
                    self.command_height_dip = (h.ceil() as i32).clamp(COMMAND_MIN_HEIGHT_DIP, COMMAND_MAX_HEIGHT_DIP);
                }
                self.show_command_bar();
            }
            "height" => {
                if let Some(h) = msg["height"].as_f64() {
                    self.command_height_dip = (h.ceil() as i32).clamp(COMMAND_MIN_HEIGHT_DIP, COMMAND_MAX_HEIGHT_DIP);
                    self.place_command_bar();
                }
            }
            "submit" => {
                let Some(url) = msg["url"].as_str().map(str::to_string) else { return };
                self.close_command_bar(false);
                if msg["newTab"].as_bool().unwrap_or(false) || self.active.is_none() {
                    self.open_tab(url, true, None);
                } else {
                    self.navigate_active(url);
                }
            }
            "switchTab" => {
                self.close_command_bar(false);
                if let Some(id) = msg["id"].as_u64() {
                    self.activate(id as TabId);
                }
                self.focus_content();
            }
            "close" => self.close_command_bar(true),
            other => log(&format!("unknown command bar command: {other}")),
        }
    }

    /// Ctrl+T (open the result in a new tab), Ctrl+L (edit the current address) or the
    /// archive. The bar appears once its page answers "present".
    fn open_command_bar(&mut self, mode: CommandMode) {
        if !self.command_ready {
            self.command_pending = Some(mode);
            return;
        }
        if self.fullscreen {
            self.set_fullscreen(false);
        }
        let Some(view) = self.command.clone() else { return };
        let text = match self.active.and_then(|id| self.tab(id)) {
            Some(t) if mode == CommandMode::Current && t.url != NEW_TAB_URL => t.url.clone(),
            _ => String::new(),
        };
        let space_name = |id: SpaceId| self.spaces.iter().find(|s| s.id == id).map_or("", |s| s.name.as_str());
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .filter(|t| t.url != NEW_TAB_URL)
            .map(|t| {
                json!({
                    "id": t.id, "title": t.title, "url": t.url, "favicon": t.favicon,
                    "space": if self.spaces.len() > 1 && t.space != self.space { space_name(t.space) } else { "" },
                })
            })
            .collect();
        self.command_mode = mode;
        let message = json!({
            "type": "open",
            "mode": mode.name(),
            "text": text,
            "active": self.active,
            "tabs": tabs,
            "suggestions": self.suggestions(mode, if mode == CommandMode::Current { "" } else { &text }),
        });
        view.post_json(&message.to_string());
        self.command_open = true;
        self.host.set_timer(TIMER_PRESENT, PRESENT_TIMEOUT_MS);
    }

    fn show_command_bar(&mut self) {
        self.host.kill_timer(TIMER_PRESENT);
        if !self.command_open || self.minimized {
            return;
        }
        let first = !self.command_shown;
        self.command_shown = true;
        self.place_command_bar();
        if first {
            self.host.show_command_window();
            self.ui_dirty = true;
        }
        if let Some(c) = &self.command {
            c.focus();
        }
    }

    fn close_command_bar(&mut self, focus_page: bool) {
        self.command_pending = None;
        self.host.kill_timer(TIMER_PRESENT);
        if !self.command_open {
            return;
        }
        self.command_open = false;
        if self.command_shown {
            self.command_shown = false;
            self.host.hide_command_window();
            self.place_command_bar();
            self.host.command_window_hidden();
            self.ui_dirty = true;
        }
        // Reset it while nobody can see it, ready for next time.
        if let Some(view) = &self.command {
            view.post_json(r#"{"type":"closed"}"#);
        }
        if focus_page {
            self.focus_content();
        }
    }

    fn suggestions(&self, mode: CommandMode, text: &str) -> Vec<Value> {
        let found = match mode {
            CommandMode::Archive => self.library.search_archive(text, ARCHIVE_RESULTS),
            _ => self.library.search(text, now_secs(), COMMAND_SUGGESTIONS),
        };
        found
            .iter()
            .map(|s| json!({ "kind": s.kind.name(), "url": s.page.url, "title": s.page.title, "favicon": s.page.favicon, "at": s.at }))
            .collect()
    }

    /// Where the command bar goes: centered over the page card. While closed it waits out of
    /// sight at the same size, still rendering.
    fn place_command_bar(&mut self) {
        let card = self.content_rect();
        let (cw, ch) = (card.width(), card.height());
        let s = |v| self.host.scale(v);
        let w = s(COMMAND_WIDTH_DIP).min(cw - s(40)).max(s(200));
        let h = s(self.command_height_dip).min(ch - s(40)).max(s(COMMAND_MIN_HEIGHT_DIP));
        let (x, y) = (card.left + (cw - w) / 2, card.top + (ch / 6).max(s(24)));
        let bounds = Rect { left: 0, top: 0, right: w, bottom: s(COMMAND_MAX_HEIGHT_DIP) };
        self.host.place_command_bar(Rect { left: x, top: y, right: x + w, bottom: y + h }, self.command_shown);
        if bounds != self.command_bounds {
            if let Some(c) = &self.command {
                self.command_bounds = bounds;
                c.set_bounds(bounds);
            }
        }
    }

    /// Send the whole tab list to the UI, at most once per batch of events.
    fn flush_ui_state(&mut self) {
        if !self.ui_dirty || !self.ui_ready {
            return;
        }
        let Some(view) = &self.ui else { return };
        // The current space only: pinned tabs, then today tabs.
        let tabs: Vec<Value> = self
            .space_order(self.space)
            .iter()
            .filter_map(|&id| self.tab(id))
            .map(|t| {
                let side = self.split.filter(|s| s.has(t.id)).map(|s| if s.left == t.id { "left" } else { "right" });
                json!({
                    "id": t.id,
                    "title": t.title,
                    "url": if t.url == NEW_TAB_URL { "" } else { t.url.as_str() },
                    "favicon": t.favicon,
                    "loading": t.loading,
                    "canBack": t.can_back,
                    "canForward": t.can_forward,
                    "suspended": t.suspended && t.pin.is_none(),
                    "crashed": t.crashed,
                    "audible": t.audible,
                    "muted": t.muted,
                    "newTab": t.url == NEW_TAB_URL || t.url.is_empty(),
                    "pinned": t.pin.is_some(),
                    "unloaded": t.unloaded(),
                    // A pinned tab that has wandered off its own page.
                    "awayFromPin": t.pin.as_ref().is_some_and(|p| p.url != t.url),
                    "split": side,
                })
            })
            .collect();
        let spaces: Vec<Value> =
            self.spaces.iter().map(|s| json!({ "id": s.id, "name": s.name, "color": s.color })).collect();
        let bookmarked = self.active.and_then(|id| self.tab(id)).is_some_and(|t| self.library.is_bookmarked(&t.url));
        let state = json!({
            "type": "state",
            "active": self.active,
            "tabs": tabs,
            "space": self.space,
            "spaces": spaces,
            "bookmarked": bookmarked,
            "splitShown": self.split_shown().is_some(),
            "sidebar": if self.sidebar_docked { "docked" } else if self.peek { "peek" } else { "hidden" },
            "maximized": self.maximized,
            "commandBar": self.command_shown,
            "fullscreen": self.fullscreen,
        });
        view.post_json(&state.to_string());
        self.ui_dirty = false;
    }

    // ---------------------------------------------------------------- tabs

    fn add_tab(&mut self, url: String, title: String, favicon: String, asleep: bool) -> TabId {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            view: None,
            creating: false,
            pending_url: Some(url.clone()),
            bounds: Rect::default(),
            url,
            title,
            favicon,
            loading: false,
            can_back: false,
            can_forward: false,
            crashed: false,
            audible: false,
            muted: false,
            last_active: Instant::now(),
            suspended: asleep,
            suspending: false,
            background: WHITE,
            space: self.space,
            pin: None,
            last_used: now_secs(),
            pane: 0,
            recorded_url: String::new(),
        });
        self.ui_dirty = true;
        id
    }

    /// Open a today tab in the current space: right after `opener` (the tab whose link opened
    /// it) when that's a today tab too, otherwise at the top of today.
    fn open_tab(&mut self, url: String, activate: bool, opener: Option<TabId>) -> TabId {
        let space = self.space;
        let after = opener.and_then(|o| self.tabs.iter().position(|t| t.id == o && t.space == space && t.pin.is_none()));
        let pos = match after {
            Some(i) => i + 1,
            None => self.tabs.iter().position(|t| t.space == space && t.pin.is_none()).unwrap_or(self.tabs.len()),
        };
        let background = if url == NEW_TAB_URL { self.palette.page } else { WHITE };
        let id = self.add_tab(url, String::new(), String::new(), false);
        let mut tab = self.tabs.pop().expect("just added");
        tab.loading = true;
        tab.background = background;
        self.tabs.insert(pos.min(self.tabs.len()), tab);
        self.ensure_webview(id);
        if activate {
            self.activate(id);
        }
        id
    }

    /// Bring back the last session. Its tabs stay asleep (no webview) until they're first shown.
    fn restore(&mut self, s: session::Session) {
        let spaces: Vec<SpaceId> = s.spaces.iter().map(|sp| self.add_space(sp.name.clone(), sp.color.clone())).collect();
        let now = now_secs();
        let ids: Vec<TabId> = s
            .tabs
            .iter()
            .map(|t| {
                let id = self.add_tab(t.page.url.clone(), t.page.title.clone(), t.page.favicon.clone(), true);
                let tab = self.tabs.last_mut().expect("just added");
                tab.space = spaces[t.space];
                tab.pin = t.pin.as_ref().map(|p| Page::new(&p.url, &p.title, &p.favicon));
                tab.last_used = if t.last_used == 0 { now } else { t.last_used };
                // Coming back to it isn't a new visit.
                tab.recorded_url = tab.url.clone();
                id
            })
            .collect();
        for (&space, saved) in spaces.iter().zip(&s.spaces) {
            if let Some(sp) = self.space_mut(space) {
                sp.active = saved.active.map(|i| ids[i]);
            }
        }
        self.split = s.split.map(|(l, r)| Split { left: ids[l], right: ids[r] });
        self.space = spaces[s.space];
        let order = self.space_order(self.space);
        let remembered = self.current_space().and_then(|sp| sp.active).filter(|id| order.contains(id));
        match remembered.or_else(|| order.first().copied()) {
            Some(id) => self.activate(id),
            None => {
                self.open_tab(NEW_TAB_URL.to_string(), true, None);
            }
        }
    }

    /// Start creating a tab's webview, unless it has one or is getting one.
    fn ensure_webview(&mut self, id: TabId) {
        if !self.env_ready {
            return;
        }
        let pane = self.pane_index(id);
        let Some(tab) = self.tab_mut(id) else { return };
        if tab.view.is_some() || tab.creating {
            return;
        }
        tab.creating = true;
        tab.pane = pane;
        let background = tab.background;
        if let Err(e) = self.host.create_tab_view(id, pane, &self.ui_dir, background) {
            log(&format!("tab {id}: creating its webview failed: {e}"));
            if let Some(tab) = self.tab_mut(id) {
                tab.creating = false;
            }
        }
    }

    fn setup_tab(&mut self, id: TabId, view: View) -> Result<(), String> {
        let Some(tab) = self.tab(id) else {
            // Closed before its webview finished starting.
            view.close();
            return Ok(());
        };
        view.set_background(tab.background);
        // A split may have started or ended while the webview was starting.
        let pane = self.pane_index(id);
        if pane != tab.pane {
            self.host.move_to_pane(&view, pane)?;
        }
        view.configure_tab(id, &self.ui_dir)?;

        let visible = self.visible_tabs().contains(&id);
        let bounds = self.tab_bounds(id);
        view.set_bounds(bounds);
        view.set_visible(visible && !self.minimized);
        if !visible {
            view.set_memory_target(false);
        }

        let tab = self.tab_mut(id).expect("checked above");
        let url = tab.pending_url.take().unwrap_or_else(|| NEW_TAB_URL.into());
        tab.creating = false;
        tab.pane = pane;
        tab.bounds = bounds;
        tab.view = Some(view.clone());
        if let Err(e) = view.navigate(&url) {
            log(&format!("tab {id}: navigate to {url} failed: {e}"));
        }
        if self.active == Some(id) && url != NEW_TAB_URL {
            self.focus_content();
        }
        self.ui_dirty = true;
        Ok(())
    }

    /// Show a tab, switching to its space.
    fn activate(&mut self, id: TabId) {
        if self.active == Some(id) || self.tab(id).is_none() {
            return;
        }
        if self.fullscreen {
            self.set_fullscreen(false);
        }
        let before = self.visible_tabs();
        self.set_active(Some(id), &before);
    }

    /// Change the active tab, given the tabs that were on screen before.
    fn set_active(&mut self, id: Option<TabId>, before: &[TabId]) {
        if let Some(space) = id.and_then(|id| self.tab(id)).map(|t| t.space) {
            self.space = space;
        }
        self.active = id;
        self.remember_active();
        self.sync_views(before);
        self.update_window_title();
    }

    /// Coming back to a space shows the tab that was active in it.
    fn remember_active(&mut self) {
        let (space, active) = (self.space, self.active);
        if let (Some(sp), Some(_)) = (self.space_mut(space), active) {
            sp.active = active;
        }
    }

    /// Tabs on screen: the active tab, or both sides of its split (only the active one in
    /// full screen).
    fn visible_tabs(&self) -> Vec<TabId> {
        match self.split_shown() {
            Some(s) if !self.fullscreen => vec![s.left, s.right],
            _ => self.active.into_iter().collect(),
        }
    }

    fn split_shown(&self) -> Option<Split> {
        self.split.filter(|s| self.active.is_some_and(|a| s.has(a)))
    }

    /// The card pane a tab's webview belongs in.
    fn pane_index(&self, id: TabId) -> usize {
        usize::from(self.split.is_some_and(|s| s.right == id))
    }

    /// Move a tab's webview into the card pane it belongs in now.
    fn place_in_pane(&mut self, id: TabId) {
        let want = self.pane_index(id);
        let Some(tab) = self.tab_mut(id) else { return };
        if tab.pane == want {
            return;
        }
        tab.pane = want;
        let view = tab.view.clone();
        if view.is_some() {
            tab.bounds = Rect::default();
        }
        if let Some(view) = view {
            let _ = self.host.move_to_pane(&view, want);
        }
    }

    /// Lay out, show the tabs that should be on screen and hide the rest of `before`.
    /// New ones are shown before old ones are hidden, so there's never an empty frame.
    fn sync_views(&mut self, before: &[TabId]) {
        let after = self.visible_tabs();
        for &id in &after {
            self.ensure_webview(id);
        }
        self.layout();
        let now = Instant::now();
        let secs = now_secs();
        let minimized = self.minimized;
        for tab in self.tabs.iter_mut().filter(|t| after.contains(&t.id)) {
            tab.last_active = now;
            tab.last_used = secs;
            tab.suspended = false;
            if let Some(v) = &tab.view {
                v.set_visible(!minimized); // also resumes a suspended page
                v.set_memory_target(true);
            }
        }
        for tab in self.tabs.iter_mut().filter(|t| before.contains(&t.id) && !after.contains(&t.id)) {
            tab.last_active = now;
            tab.last_used = secs;
            if let Some(v) = &tab.view {
                v.set_visible(false);
                v.set_memory_target(false);
            }
        }
        self.ui_dirty = true;
    }

    /// Ctrl+W or a tab's close button. A pinned tab only unloads, back to its own page.
    fn close_tab(&mut self, id: TabId) {
        match self.tab(id) {
            Some(t) if t.pin.is_some() => self.unload_tab(id),
            Some(_) => self.remove_tab(id, true),
            None => {}
        }
    }

    /// Close a tab for good; its page goes to the archive. `by_hand`: Ctrl+Shift+T reopens it.
    fn remove_tab(&mut self, id: TabId, by_hand: bool) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let order = self.space_order(self.tabs[index].space);
        let before = self.visible_tabs();
        let tab = self.tabs.remove(index);
        self.end_split_with(id);
        if by_hand && !tab.url.is_empty() && tab.url != NEW_TAB_URL {
            self.closed.push(tab.url.clone());
            if self.closed.len() > MAX_CLOSED_TABS {
                self.closed.remove(0);
            }
        }
        self.library.archive(tab.page(), now_secs());
        if let Some(v) = &tab.view {
            v.close();
        }
        for sp in &mut self.spaces {
            if sp.active == Some(id) {
                sp.active = None;
            }
        }
        if self.active == Some(id) {
            self.replace_active(id, &order, &before, true);
        } else {
            self.sync_views(&before);
        }
        self.ui_dirty = true;
    }

    /// A pinned tab lets go of its webview and goes back to its own page.
    fn unload_tab(&mut self, id: TabId) {
        let Some(space) = self.tab(id).map(|t| t.space) else { return };
        let order = self.space_order(space);
        let before = self.visible_tabs();
        let Some(tab) = self.tab_mut(id) else { return };
        let Some(pin) = tab.pin.clone() else { return };
        if let Some(v) = tab.view.take() {
            v.close();
        }
        tab.pending_url = Some(pin.url.clone());
        tab.url = pin.url;
        tab.title = pin.title;
        tab.favicon = pin.favicon;
        tab.loading = false;
        tab.can_back = false;
        tab.can_forward = false;
        tab.crashed = false;
        tab.audible = false;
        tab.muted = false;
        tab.suspended = false;
        tab.suspending = false;
        tab.bounds = Rect::default();
        tab.background = WHITE;
        self.end_split_with(id);
        if self.active == Some(id) {
            self.replace_active(id, &order, &before, false);
        } else {
            self.sync_views(&before);
        }
        self.ui_dirty = true;
    }

    /// The active tab went away (closed, unloaded, moved to another space): show the next
    /// one in the list. If only unloaded pinned tabs are left, `load_pinned` loads the first
    /// of them; otherwise a new tab page opens.
    fn replace_active(&mut self, gone: TabId, order: &[TabId], before: &[TabId], load_pinned: bool) {
        if self.fullscreen {
            self.set_fullscreen(false);
        }
        let space = self.space;
        let here = |t: TabId| t != gone && self.tab(t).is_some_and(|t| t.space == space);
        let pos = order.iter().position(|&t| t == gone).unwrap_or(0);
        let next = order[pos..]
            .iter()
            .chain(order[..pos].iter().rev())
            .copied()
            .find(|&t| here(t) && self.tab(t).is_some_and(|t| !t.unloaded()))
            .or_else(|| order.iter().copied().find(|&t| load_pinned && here(t)));
        match next {
            Some(next) => {
                self.set_active(Some(next), before);
                self.focus_content();
            }
            None if self.tabs.is_empty() => {
                // Closing the last tab closes the window, like other browsers.
                self.active = None;
                self.host.close_window();
            }
            None => {
                self.active = None;
                self.sync_views(before);
                self.open_tab(NEW_TAB_URL.to_string(), true, None);
            }
        }
    }

    /// Move a tab to position `to` among its space's pinned or today tabs. `pinned` pins or
    /// unpins it on the way; pinning makes the page it's on its own.
    fn move_tab(&mut self, id: TabId, to: usize, pinned: Option<bool>) {
        let Some(from) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let mut tab = self.tabs.remove(from);
        let pin = pinned.unwrap_or(tab.pin.is_some()) && tab.url != NEW_TAB_URL;
        if pin && tab.pin.is_none() {
            let url = if tab.view.is_some() { tab.url.clone() } else { tab.pending_url.clone().unwrap_or(tab.url.clone()) };
            tab.pin = Some(Page::new(&url, &tab.title, &tab.favicon));
        } else if !pin && tab.pin.is_some() {
            tab.pin = None;
            tab.suspended = tab.view.is_none();
        }
        let space = tab.space;
        let section: Vec<usize> =
            self.tabs.iter().enumerate().filter(|(_, t)| t.space == space && t.pin.is_some() == pin).map(|(i, _)| i).collect();
        let pos = match section.get(to) {
            Some(&i) => i,
            None => section.last().map_or(self.tabs.len(), |&i| i + 1),
        };
        self.tabs.insert(pos, tab);
        self.ui_dirty = true;
    }

    /// Archive the current space's today tabs (the sidebar's "Clear").
    fn clear_today(&mut self) {
        let space = self.space;
        let visible = self.visible_tabs();
        let mut ids: Vec<TabId> = self.tabs.iter().filter(|t| t.space == space && t.pin.is_none()).map(|t| t.id).collect();
        // Tabs on screen go last, so the others aren't each shown on the way.
        ids.sort_by_key(|id| visible.contains(id));
        for id in ids {
            self.remove_tab(id, false);
        }
    }

    /// Today tabs nobody has looked at for a while go to the archive, like Arc. Pinned tabs,
    /// tabs on screen or playing sound, and the tab each space shows are kept.
    fn archive_idle_tabs(&mut self) {
        let now = now_secs();
        let visible = self.visible_tabs();
        let shown: Vec<TabId> = self.spaces.iter().filter_map(|s| s.active).collect();
        let idle: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| {
                t.pin.is_none()
                    && !t.audible
                    && !visible.contains(&t.id)
                    && !shown.contains(&t.id)
                    && now.saturating_sub(t.last_used) >= self.archive_after
            })
            .map(|t| t.id)
            .collect();
        for id in idle {
            self.remove_tab(id, false);
        }
    }

    // ---------------------------------------------------------------- spaces

    fn add_space(&mut self, name: String, color: String) -> SpaceId {
        let id = self.next_space_id;
        self.next_space_id += 1;
        let n = self.spaces.len();
        let name = match name.trim() {
            "" if n == 0 => "Home".to_string(),
            "" => format!("Space {}", n + 1),
            name => name.chars().take(MAX_SPACE_NAME).collect(),
        };
        let color = if SPACE_COLORS.contains(&color.as_str()) { color } else { SPACE_COLORS[n % SPACE_COLORS.len()].to_string() };
        self.spaces.push(Space { id, name, color, active: None });
        if n == 0 {
            self.space = id;
        }
        self.ui_dirty = true;
        id
    }

    /// A space's tabs in sidebar order: pinned first, then today.
    fn space_order(&self, space: SpaceId) -> Vec<TabId> {
        let section = |pinned: bool| self.tabs.iter().filter(move |t| t.space == space && t.pin.is_some() == pinned).map(|t| t.id);
        section(true).chain(section(false)).collect()
    }

    fn space_mut(&mut self, id: SpaceId) -> Option<&mut Space> {
        self.spaces.iter_mut().find(|s| s.id == id)
    }

    fn current_space(&self) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == self.space)
    }

    /// Show another space, with the tab it showed last (an empty space gets a new tab page).
    fn switch_space(&mut self, space: SpaceId) {
        if space == self.space || !self.spaces.iter().any(|s| s.id == space) {
            return;
        }
        let order = self.space_order(space);
        let remembered = self.spaces.iter().find(|s| s.id == space).and_then(|s| s.active).filter(|id| order.contains(id));
        let target = remembered
            .or_else(|| order.iter().copied().find(|&id| self.tab(id).is_some_and(|t| !t.unloaded())))
            .or_else(|| order.first().copied());
        match target {
            Some(id) => {
                self.activate(id);
                self.focus_content();
            }
            None => {
                self.space = space;
                self.open_tab(NEW_TAB_URL.to_string(), true, None);
            }
        }
        self.ui_dirty = true;
    }

    /// Delete a space. Its tabs (pinned ones too) go to the archive, so nothing is lost.
    fn delete_space(&mut self, space: SpaceId) {
        let Some(index) = self.spaces.iter().position(|s| s.id == space) else { return };
        if self.spaces.len() < 2 {
            return;
        }
        if space == self.space {
            let neighbor = self.spaces[if index == 0 { 1 } else { index - 1 }].id;
            self.switch_space(neighbor);
        }
        if self.split.is_some_and(|s| self.tab(s.left).is_some_and(|t| t.space == space)) {
            self.split = None;
        }
        let now = now_secs();
        let (gone, kept): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| t.space == space);
        self.tabs = kept;
        for tab in gone {
            self.library.archive(tab.page(), now);
            if let Some(v) = &tab.view {
                v.close();
            }
        }
        self.spaces.retain(|s| s.id != space);
        self.ui_dirty = true;
    }

    /// Move a tab to another space: to the top of its today tabs, or after its pinned tabs.
    fn move_to_space(&mut self, id: TabId, space: SpaceId) {
        let Some(from_space) = self.tab(id).map(|t| t.space) else { return };
        if from_space == space || !self.spaces.iter().any(|s| s.id == space) {
            return;
        }
        let order = self.space_order(from_space);
        let before = self.visible_tabs();
        self.end_split_with(id);
        let Some(from) = self.tabs.iter().position(|t| t.id == id) else { return };
        let mut tab = self.tabs.remove(from);
        tab.space = space;
        let pos = if tab.pin.is_some() {
            self.tabs.iter().rposition(|t| t.space == space && t.pin.is_some()).map_or(self.tabs.len(), |i| i + 1)
        } else {
            self.tabs.iter().position(|t| t.space == space && t.pin.is_none()).unwrap_or(self.tabs.len())
        };
        self.tabs.insert(pos, tab);
        for sp in &mut self.spaces {
            if sp.active == Some(id) {
                sp.active = None;
            }
        }
        if self.active == Some(id) {
            self.replace_active(id, &order, &before, true);
        } else {
            self.sync_views(&before);
        }
        self.ui_dirty = true;
    }

    // ---------------------------------------------------------------- split view

    /// Show `other` beside the active tab.
    fn split_with(&mut self, other: TabId) {
        let Some(active) = self.active else { return };
        let (Some(a), Some(o)) = (self.tab(active), self.tab(other)) else { return };
        if other == active || a.space != o.space {
            return;
        }
        let same_section = a.pin.is_some() == o.pin.is_some();
        if self.fullscreen {
            self.set_fullscreen(false);
        }
        let before = self.visible_tabs();
        if let Some(old) = self.split.take() {
            self.place_in_pane(old.right);
        }
        self.split = Some(Split { left: active, right: other });
        // Keep the pair together in the sidebar.
        if same_section && let Some(from) = self.tabs.iter().position(|t| t.id == other) {
            let tab = self.tabs.remove(from);
            let at = self.tabs.iter().position(|t| t.id == active).map_or(self.tabs.len(), |i| i + 1);
            self.tabs.insert(at, tab);
        }
        self.place_in_pane(active);
        self.place_in_pane(other);
        self.sync_views(&before);
        self.focus_content();
    }

    fn separate(&mut self) {
        let before = self.visible_tabs();
        if let Some(s) = self.split.take() {
            self.place_in_pane(s.right);
            self.sync_views(&before);
            self.focus_content();
        }
    }

    /// The split `id` is part of ends (it closed, unloaded or moved away).
    fn end_split_with(&mut self, id: TabId) {
        if let Some(s) = self.split.filter(|s| s.has(id)) {
            self.split = None;
            self.place_in_pane(s.right);
        }
    }

    /// Ctrl+\: separate the split on screen, or show the active tab beside the tab used most
    /// recently before it (a new tab page if there's none).
    fn toggle_split(&mut self) {
        if self.split_shown().is_some() {
            self.separate();
            return;
        }
        let Some(active) = self.active else { return };
        let recent = self
            .space_order(self.space)
            .into_iter()
            .filter(|&id| id != active)
            .filter_map(|id| self.tab(id))
            .filter(|t| !t.unloaded())
            .max_by_key(|t| t.last_active)
            .map(|t| t.id);
        let other = match recent {
            Some(id) => id,
            None => self.open_tab(NEW_TAB_URL.to_string(), false, Some(active)),
        };
        self.split_with(other);
    }

    fn navigate_active(&mut self, url: String) {
        let Some(id) = self.active else { return };
        let Some(tab) = self.tab_mut(id) else { return };
        match &tab.view {
            Some(view) => {
                if let Err(e) = view.navigate(&url) {
                    log(&format!("navigate to {url} failed: {e}"));
                    return;
                }
                view.focus();
            }
            None => tab.pending_url = Some(url),
        }
    }

    /// Pull the latest URL, title, history state, sound and favicon from a tab's webview.
    fn refresh_tab(&mut self, id: TabId) {
        let Some(tab) = self.tab_mut(id) else { return };
        let Some(view) = tab.view.clone() else { return };
        let s = view.state();
        if let Some(url) = s.url {
            tab.url = url;
        }
        if let Some(title) = s.title.filter(|_| !tab.crashed) {
            tab.title = title;
        }
        if let Some(b) = s.can_back {
            tab.can_back = b;
        }
        if let Some(b) = s.can_forward {
            tab.can_forward = b;
        }
        if let Some(b) = s.audible {
            tab.audible = b;
        }
        if let Some(b) = s.muted {
            tab.muted = b;
        }
        if let Some(favicon) = s.favicon {
            tab.favicon = favicon;
        }
        // A new address is a visit; otherwise the title or icon may have arrived since.
        let page = tab.page();
        if tab.url != tab.recorded_url && !tab.crashed {
            tab.recorded_url = tab.url.clone();
            self.library.record_visit(page, now_secs());
        } else {
            self.library.update_page(&page);
        }
        if self.active == Some(id) {
            self.update_window_title();
        }
        self.ui_dirty = true;
    }

    fn read_tab_background(&self, id: TabId) {
        let Some(view) = self.tab(id).and_then(|t| t.view.clone()) else { return };
        view.eval_with_result(BACKGROUND_SCRIPT, id, Event::TabBackground);
    }

    fn set_tab_background(&mut self, id: TabId, color: Rgb) {
        let visible = self.visible_tabs().contains(&id);
        let Some(tab) = self.tab_mut(id) else { return };
        tab.background = color;
        if let Some(v) = &tab.view {
            v.set_background(color);
        }
        let pane = tab.pane;
        if visible {
            self.host.set_pane_color(pane, color);
        }
    }

    fn run(&mut self, cmd: Command) {
        match cmd {
            Command::NewTab => self.open_command_bar(CommandMode::NewTab),
            Command::CloseTab => {
                if let Some(id) = self.active {
                    self.close_tab(id);
                }
            }
            Command::ReopenClosedTab => {
                if let Some(url) = self.closed.pop() {
                    self.library.unarchive(&url);
                    self.open_tab(url, true, None);
                }
            }
            Command::EditAddress => self.open_command_bar(CommandMode::Current),
            // These go through the current space's tabs, in sidebar order.
            Command::NextTab | Command::PrevTab => {
                let order = self.space_order(self.space);
                let n = order.len();
                if let (Some(i), true) = (self.active.and_then(|a| order.iter().position(|&t| t == a)), n > 1) {
                    let j = if matches!(cmd, Command::NextTab) { (i + 1) % n } else { (i + n - 1) % n };
                    self.activate(order[j]);
                    self.focus_content();
                }
            }
            Command::SelectTab(i) => {
                let order = self.space_order(self.space);
                let target = if i == usize::MAX { order.last() } else { order.get(i) };
                if let Some(&id) = target {
                    self.activate(id);
                    self.focus_content();
                }
            }
            Command::ToggleBookmark => {
                if let Some(page) = self.active.and_then(|id| self.tab(id)).map(Tab::page) {
                    self.library.toggle_bookmark(page);
                    self.ui_dirty = true;
                }
            }
            Command::ToggleSplit => self.toggle_split(),
            Command::SelectSpace(i) => {
                if let Some(space) = self.spaces.get(i).map(|s| s.id) {
                    self.switch_space(space);
                }
            }
            Command::Reload => {
                if let Some(view) = self.active_view() {
                    view.reload();
                }
            }
            Command::Back => {
                if let Some(view) = self.active_view() {
                    view.back();
                }
            }
            Command::Forward => {
                if let Some(view) = self.active_view() {
                    view.forward();
                }
            }
            Command::ToggleSidebar => {
                if self.fullscreen {
                    return;
                }
                if self.sidebar_docked {
                    if self.hide_pending.is_none() && !self.prerender_hide() {
                        self.set_sidebar(false, false);
                        self.focus_content();
                    }
                } else {
                    // Pinning a slid-out sidebar leaves it where it is.
                    self.set_sidebar(true, false);
                }
            }
            Command::ToggleFullscreen => {
                let on = !self.fullscreen;
                self.set_fullscreen(on);
            }
        }
    }

    // ---------------------------------------------------------------- focus

    /// The window itself got focus (it was activated, e.g. by a click on the page or the
    /// sidebar). If the command bar was open, the user has moved on from it.
    pub fn on_window_focus(&mut self) {
        if self.command_shown {
            self.close_command_bar(false);
            self.flush_ui_state();
        }
        self.focus_content();
    }

    pub fn focus_content(&mut self) {
        if self.command_shown {
            if let Some(c) = &self.command {
                c.focus();
            }
            return;
        }
        let Some(tab) = self.active.and_then(|id| self.tab(id)) else { return };
        if let Some(v) = &tab.view {
            v.focus();
        }
    }

    // ---------------------------------------------------------------- window

    fn sidebar_width(&self) -> i32 {
        self.host.scale(self.sidebar_width_dip)
    }

    /// Where the page card goes, in window client coordinates.
    fn content_rect(&self) -> Rect {
        let rc = self.host.client_rect();
        if self.fullscreen {
            return rc;
        }
        let m = self.host.scale(CARD_MARGIN_DIP);
        let left = if self.sidebar_docked { self.sidebar_width() } else { m };
        Rect { left, top: m, right: rc.right - m, bottom: rc.bottom - m }
    }

    /// The two card panes, in window client coordinates. The second is empty unless a split
    /// is on screen; in full screen only the active side shows, filling it.
    fn pane_rects(&self) -> [Rect; 2] {
        let full = self.content_rect();
        let Some(active) = self.active.filter(|_| self.split_shown().is_some()) else {
            return [full, Rect::default()];
        };
        if self.fullscreen {
            let mut rects = [Rect::default(); 2];
            rects[self.pane_index(active)] = full;
            return rects;
        }
        let gap = self.host.scale(CARD_MARGIN_DIP);
        let left_width = (full.right - full.left - gap) / 2;
        [
            Rect { right: full.left + left_width, ..full },
            Rect { left: full.left + left_width + gap, ..full },
        ]
    }

    /// Bounds for a tab's webview, relative to its pane. While the sidebar is about to hide,
    /// the page already has the wider size (the card clips it until then).
    fn tab_bounds(&self, id: TabId) -> Rect {
        let r = self.pane_rects()[self.pane_index(id)];
        let mut b = Rect { left: 0, top: 0, right: r.right - r.left, bottom: r.bottom - r.top };
        if self.hide_pending.is_some() && self.sidebar_docked && !self.fullscreen && self.split_shown().is_none() {
            b.right += self.sidebar_width() - self.host.scale(CARD_MARGIN_DIP);
        }
        b
    }

    /// Hiding the sidebar makes the page wider, and a webview takes a frame or two to draw at
    /// a new size; meanwhile Windows shows its plain background in the new strip on the right,
    /// a visible flash. So the page is widened first, while the card still clips it to its old
    /// size, and the card grows (and the sidebar slides away) once the page says it has drawn.
    /// Returns false if there's no page to wait for (or two, in split view: then both cards
    /// just grow), or the platform doesn't need this.
    fn prerender_hide(&mut self) -> bool {
        if !platform::PRERENDER_HIDE {
            return false;
        }
        let Some(id) = self.active.filter(|_| self.split_shown().is_none()) else { return false };
        let Some(view) = self.tab(id).and_then(|t| t.view.clone()) else { return false };
        self.frame_nonce += 1;
        let nonce = self.frame_nonce;
        self.hide_pending = Some(nonce);
        let bounds = self.tab_bounds(id);
        if let Some(tab) = self.tab_mut(id) {
            if let Some(v) = &tab.view {
                v.set_bounds(bounds);
                tab.bounds = bounds;
            }
        }
        // Three animation frames: the page lays out at the new size, draws, and that frame
        // reaches the screen.
        let script = format!(
            "requestAnimationFrame(() => requestAnimationFrame(() => requestAnimationFrame(() => \
             window.chrome?.webview?.postMessage('{FRAME_MESSAGE}{nonce}'))))"
        );
        view.eval(&script);
        self.host.set_timer(TIMER_HIDE, HIDE_TIMEOUT_MS);
        // Refresh the fill color too, in case the page changed since it loaded.
        self.read_tab_background(id);
        true
    }

    fn finish_hide(&mut self) {
        self.host.kill_timer(TIMER_HIDE);
        if self.hide_pending.take().is_some() && self.sidebar_docked && !self.fullscreen {
            self.set_sidebar(false, false);
            self.focus_content();
        }
    }

    /// The sidebar's place. A slid-out sidebar uses exactly the same spot as the docked one,
    /// so nothing in it moves when you pin it with the toggle button. A hidden one waits
    /// just off the left edge, still rendering, so it can slide back without a blank frame.
    fn sidebar_rect(&self) -> Rect {
        let rc = self.host.client_rect();
        let w = self.sidebar_width();
        // Leaves a thin strip at the top so the window can be resized from there.
        let top = if self.maximized || self.fullscreen { 0 } else { self.host.scale(platform::TOP_RESIZE_DIP) };
        let x = if self.fullscreen { -w } else { self.sidebar_x() };
        Rect { left: x, top, right: x + w, bottom: rc.bottom }
    }

    /// Horizontal offset of the sidebar: 0 when out, -width when hidden, and in between while sliding.
    fn sidebar_x(&self) -> i32 {
        let w = self.sidebar_width() as f32;
        let Some(slide) = self.slide else {
            return if self.sidebar_docked || self.peek { 0 } else { -(w as i32) };
        };
        let t = (slide.start.elapsed().as_secs_f32() / slide.duration().as_secs_f32()).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(3); // ease-out cubic
        let hidden_fraction = if slide.showing { 1.0 - eased } else { eased };
        -(w * hidden_fraction).round() as i32
    }

    /// Change where the sidebar is, sliding it if it has to come in or go out.
    fn set_sidebar(&mut self, docked: bool, peek: bool) {
        let was_out = match self.slide {
            Some(s) => s.showing,
            None => self.sidebar_docked || self.peek,
        };
        let out = docked || peek;
        self.sidebar_docked = docked;
        self.peek = peek;
        self.peek_left_at = None;
        if out != was_out {
            // Reversing mid-slide continues from where it is.
            let start = match self.slide {
                Some(s) => {
                    let done = (s.start.elapsed().as_secs_f32() / s.duration().as_secs_f32()).min(1.0);
                    let next = Slide { start: Instant::now(), showing: out };
                    Instant::now() - next.duration().mul_f32(1.0 - done)
                }
                None => Instant::now(),
            };
            self.slide = Some(Slide { start, showing: out });
            self.host.request_animate();
        }
        self.layout();
        self.ui_dirty = true;
    }

    /// One frame of the slide, paced by the screen so it moves on every refresh.
    pub fn animate(&mut self) {
        let Some(slide) = self.slide else { return };
        if slide.start.elapsed() >= slide.duration() || self.minimized {
            self.slide = None;
            self.layout();
            self.ui_dirty = true;
            self.flush_ui_state();
            return;
        }
        let r = self.sidebar_rect();
        self.host.slide_sidebar(r);
        self.host.request_animate();
    }

    fn layout(&mut self) {
        if self.minimized {
            return;
        }
        let rects = self.pane_rects();
        let radius = if self.fullscreen { 0 } else { self.host.scale(CARD_RADIUS_DIP) };
        let sidebar = self.sidebar_rect();
        let visible = self.visible_tabs();
        let bounds: Vec<Rect> = visible.iter().map(|&id| self.tab_bounds(id)).collect();
        let show_split_pane = rects[1].right > rects[1].left;
        let split_pane_change = (show_split_pane != self.split_pane_shown).then_some(show_split_pane);
        self.split_pane_shown = show_split_pane;
        // The pages first, so they start laying out at their new size while the panes move.
        for (&id, &b) in visible.iter().zip(&bounds) {
            let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else { continue };
            if let Some(v) = &tab.view {
                if tab.bounds != b {
                    v.set_bounds(b);
                    tab.bounds = b;
                }
            }
            self.host.set_pane_color(tab.pane, tab.background);
        }
        for (i, r) in rects.iter().enumerate() {
            if r.right > r.left {
                self.host.shape_pane(i, r.width(), r.height(), radius);
            }
        }
        self.host.move_panes(rects, split_pane_change, sidebar);
        let size = (sidebar.width(), sidebar.height());
        if size != self.sidebar_size {
            if let Some(ui) = &self.ui {
                self.sidebar_size = size;
                ui.set_bounds(Rect { left: 0, top: 0, right: size.0, bottom: size.1 });
            }
        }
        self.place_command_bar();
    }

    pub fn on_resize(&mut self) {
        let minimized = self.host.is_minimized();
        let maximized = self.host.is_maximized();
        if maximized != self.maximized {
            self.maximized = maximized;
            self.ui_dirty = true;
        }
        if minimized != self.minimized {
            self.minimized = minimized;
            if minimized {
                self.close_command_bar(false);
            }
            // Hidden webviews stop rendering, which saves CPU and battery while minimized.
            for id in self.visible_tabs() {
                if let Some(v) = self.tab(id).and_then(|t| t.view.as_ref()) {
                    v.set_visible(!minimized);
                }
            }
        }
        self.layout();
        // Called straight from the window's resize event, outside the event queue, so push
        // state here.
        self.flush_ui_state();
    }

    /// Mouse over the window's own area (the margins around the page card). Touching the
    /// left edge while the sidebar is hidden slides it out.
    pub fn on_mouse_move(&mut self, x: i32) {
        let hidden = !self.sidebar_docked && !self.peek && !self.fullscreen;
        if hidden && self.peek_armed && x < self.host.scale(CARD_MARGIN_DIP) {
            self.set_peek(true);
            self.flush_ui_state();
        }
    }

    fn set_peek(&mut self, on: bool) {
        if self.sidebar_docked || self.peek == on || self.fullscreen {
            return;
        }
        if !on {
            self.peek_armed = false;
        }
        self.set_sidebar(false, on);
        self.host.set_timer(TIMER_PEEK, PEEK_TICK_MS);
    }

    /// Slides the peeking sidebar back once the cursor has left it.
    fn peek_tick(&mut self) {
        let (x, y) = self.host.cursor_pos();
        let rc = self.host.client_rect();
        let slack = self.host.scale(6);
        if self.peek {
            let over_sidebar = x >= -slack && x < self.sidebar_width() + slack && y >= -slack && y < rc.bottom + slack;
            // Keep it out while a button is held (dragging a tab, resizing the sidebar).
            let button_down = self.host.left_button_down();
            if over_sidebar || button_down || self.command_open {
                self.peek_left_at = None;
            } else if self.peek_left_at.get_or_insert_with(Instant::now).elapsed() >= PEEK_LINGER {
                self.set_peek(false);
            }
        } else if !self.peek_armed {
            // Re-arm once the cursor is away from the left edge.
            let near_edge = x < 2 * self.host.scale(CARD_MARGIN_DIP) && y >= 0 && y < rc.bottom && x >= 0;
            self.peek_armed = !near_edge;
        }

        if !self.peek && self.peek_armed {
            self.host.kill_timer(TIMER_PEEK);
        }
    }

    fn window_action(&self, action: WindowAction) {
        WINDOW_ACTIONS.with(|a| a.borrow_mut().push(action));
    }

    pub fn on_move(&mut self) {
        // Keeps dropdowns, tooltips and IME windows positioned correctly.
        for v in [&self.ui, &self.command].into_iter().flatten() {
            v.notify_parent_moved();
        }
        for id in self.visible_tabs() {
            if let Some(v) = self.tab(id).and_then(|t| t.view.as_ref()) {
                v.notify_parent_moved();
            }
        }
        if self.command_shown {
            self.place_command_bar();
        }
    }

    fn update_window_title(&self) {
        let title = match self.active.and_then(|id| self.tab(id)) {
            Some(t) if !t.title.is_empty() && t.url != NEW_TAB_URL => t.title.clone(),
            _ => "New Tab".to_string(),
        };
        self.host.set_title(&title);
    }

    fn on_fullscreen_changed(&mut self, id: TabId) {
        if self.active != Some(id) {
            // In split view, the other side can go full screen too.
            if !self.split_shown().is_some_and(|s| s.has(id)) {
                return;
            }
            self.active = Some(id);
            self.remember_active();
        }
        let Some(view) = self.active_view() else { return };
        if let Some(on) = view.contains_fullscreen() {
            self.set_fullscreen(on);
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        if on == self.fullscreen {
            return;
        }
        if on {
            self.close_command_bar(false);
        }
        let before = self.visible_tabs();
        self.hide_pending = None;
        self.fullscreen = on;
        if on {
            self.peek = false;
            self.slide = None;
        }
        self.host.set_fullscreen(on);
        // In split view only one side goes full screen.
        self.sync_views(&before);
    }

    // ---------------------------------------------------------------- memory

    /// Freeze background tabs that have been idle for a while. A suspended tab keeps its
    /// page in memory but stops running script and timers, and the engine can trim its
    /// memory further; it resumes instantly when shown again.
    pub fn on_timer(&mut self, timer_id: TimerId) {
        match timer_id {
            TIMER_SUSPEND => {
                self.suspend_idle_tabs();
                self.archive_idle_tabs();
            }
            TIMER_PEEK => self.peek_tick(),
            TIMER_SESSION => {
                self.save_session();
                if self.library.dirty && self.library_saved_at.elapsed() >= LIBRARY_SAVE_EVERY {
                    session::save_library(&mut self.library);
                    self.library_saved_at = Instant::now();
                }
            }
            TIMER_PRESENT => self.show_command_bar(),
            TIMER_HIDE => self.finish_hide(),
            _ => {}
        }
        // Called straight from the timer, outside the event queue.
        self.flush_ui_state();
    }

    fn suspend_idle_tabs(&mut self) {
        let now = Instant::now();
        let visible = self.visible_tabs();
        let suspend_after = self.suspend_after;
        for tab in &mut self.tabs {
            let idle = now.duration_since(tab.last_active) >= suspend_after;
            if visible.contains(&tab.id) || tab.suspended || tab.suspending || !idle {
                continue;
            }
            let Some(v) = &tab.view else { continue };
            tab.suspending = v.try_suspend(tab.id);
        }
    }

    // ---------------------------------------------------------------- session

    fn session_json(&self) -> String {
        let tab_index = |id: TabId| self.tabs.iter().position(|t| t.id == id);
        let space_index = |id: SpaceId| self.spaces.iter().position(|s| s.id == id).unwrap_or(0);
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .map(|t| {
                let url = t.pending_url.as_deref().filter(|_| t.view.is_none()).unwrap_or(&t.url);
                json!({
                    "url": url,
                    "title": if t.crashed { "" } else { t.title.as_str() },
                    "favicon": t.favicon,
                    "space": space_index(t.space),
                    "pin": t.pin.as_ref().map(|p| json!({ "url": p.url, "title": p.title, "favicon": p.favicon })),
                    "lastUsed": t.last_used,
                })
            })
            .collect();
        let spaces: Vec<Value> = self
            .spaces
            .iter()
            .map(|s| json!({ "name": s.name, "color": s.color, "active": s.active.and_then(tab_index) }))
            .collect();
        let split_pair = self.split.and_then(|s| Some([tab_index(s.left)?, tab_index(s.right)?]));
        // The window's normal (not maximized or full screen) place.
        let (r, maximized) = self.host.placement();
        json!({
            "version": 2,
            "tabs": tabs,
            "spaces": spaces,
            "space": space_index(self.space),
            "split": split_pair,
            "sidebarWidth": self.sidebar_width_dip,
            "sidebarDocked": self.sidebar_docked,
            "window": {
                "rect": [r.left, r.top, r.right, r.bottom],
                "maximized": maximized || self.maximized,
            },
        })
        .to_string()
    }

    fn save_session(&mut self) {
        let json = self.session_json();
        if json != self.saved_session {
            session::save(&json);
            self.saved_session = json;
        }
    }

    fn close_all(self) {
        for tab in &self.tabs {
            if let Some(v) = &tab.view {
                v.close();
            }
        }
        for v in [&self.ui, &self.command].into_iter().flatten() {
            v.close();
        }
    }

    // ---------------------------------------------------------------- helpers

    fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_view(&self) -> Option<View> {
        self.active.and_then(|id| self.tab(id)).and_then(|t| t.view.clone())
    }
}

pub fn log(msg: &str) {
    if cfg!(debug_assertions) {
        eprintln!("[browser] {msg}");
    }
}

fn fatal(msg: &str) {
    log(msg);
    platform::fatal(msg);
}

#[cfg(test)]
mod tests {
    use super::{Rect, parse_rgb};

    #[test]
    fn reads_page_background_colors() {
        // Script results arrive as JSON, so the string arrives quoted.
        assert_eq!(parse_rgb(r#""rgb(255, 255, 255)""#), Some((255, 255, 255)));
        assert_eq!(parse_rgb("rgb(18, 18, 18)"), Some((18, 18, 18)));
        assert_eq!(parse_rgb("rgba(13, 17, 23, 1)"), Some((13, 17, 23)));
        assert_eq!(parse_rgb("null"), None);
        assert_eq!(parse_rgb(r#""garbage(1, x)""#), None);
        assert_eq!(parse_rgb(""), None);
    }

    #[test]
    fn rect_size() {
        let r = Rect { left: 10, top: 20, right: 110, bottom: 70 };
        assert_eq!((r.width(), r.height()), (100, 50));
    }
}
