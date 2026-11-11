//! macOS: an AppKit window (tao) around WebKit webviews (wry), with the menu bar from muda.
//!
//! The window keeps the native traffic lights over a transparent title bar, and its content
//! reaches under them, so the sidebar's top row is the title bar. Inside the window a flipped
//! root view holds the panes, like the child windows on Windows: the page card, the split's
//! second card, the sidebar, and the command bar on top. Each pane is a layer-backed view (the
//! cards clip to rounded corners), and each webview is a subview of its pane.
//!
//! The UI pages talk to the host through `window.chrome.webview` like on Windows; a small
//! script (`HOST_SHIM`) maps it onto wry's IPC. They're served from `browser://ui/`.
//!
//! Webview callbacks queue a `browser::Event` and wake the event loop with a user event.
//! Keyboard shortcuts are menu items (⌘T, ⌘L, ⌘W…), which also gives pages copy and paste.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    ffi::c_void,
    path::{Path, PathBuf},
    ptr::{NonNull, null_mut},
    rc::Rc,
    sync::Mutex,
    time::{Duration, Instant},
};

use block2::RcBlock;
use muda::{
    Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    accelerator::{Accelerator, Code, Modifiers},
};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject},
};
use objc2_app_kit::{
    NSAlert, NSAutoresizingMaskOptions, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSResponder,
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowButton,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    NSDictionary, NSKeyValueChangeKey, NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration,
    NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event as LoopEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::macos::{ActivationPolicy, EventLoopExtMacOS, WindowBuilderExtMacOS, WindowExtMacOS},
    window::{Theme, Window, WindowBuilder},
};
use wry::{
    NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder, WebViewExtMacOS, WryWebView,
    http::{Request, Response},
    raw_window_handle::{AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle},
};

use crate::{
    browser::{self, Command, Event, PageState, Rect, Surface, TabId, TimerId, WindowAction},
    theme::{Palette, Rgb},
};

type Res<T> = Result<T, String>;

/// The UI is served from this scheme (`browser://ui/…`), straight from the `ui/` folder.
const SCHEME: &str = "browser";
pub const UI_ORIGIN: &str = "browser://ui/";
pub const UI_URL: &str = "browser://ui/chrome.html";
pub const COMMAND_URL: &str = "browser://ui/command.html";
pub const NEW_TAB_URL: &str = "browser://ui/newtab.html";
/// macOS resizes the window from its edges itself, over the webviews.
pub const TOP_RESIZE_DIP: i32 = 0;
/// Not used here: it waits for a message from the page, and tab pages get no message channel
/// on macOS (`window.ipc` only exists in the UI webviews).
pub const PRERENDER_HIDE: bool = false;
/// WebKit has no favicon API, so after each load the page is asked for its icon link.
pub const FAVICON_SCRIPT: Option<&str> = Some(
    r#"(() => {
  const link = [...document.querySelectorAll("link[rel~='icon' i], link[rel='apple-touch-icon' i]")].find((l) => l.href);
  if (link) return link.href;
  return /^https?:$/.test(location.protocol) ? location.origin + "/favicon.ico" : "";
})()"#,
);

/// WebKit needs no shared environment; webviews can be made right away.
pub type Env = ();

const MIN_WIDTH: f64 = 480.0;
const MIN_HEIGHT: f64 = 320.0;
/// The traffic lights sit in the sidebar's top row (42 points tall, see `#topbar` in
/// ui/chrome.css), which leaves room for them on macOS.
const TRAFFIC_LIGHTS: (f64, f64) = (14.0, 21.0);
/// The sidebar must be out at least this far for the traffic lights to show over it.
const TRAFFIC_LIGHTS_WIDTH: i32 = 80;
const COMMAND_RADIUS: f64 = 12.0;
/// The sidebar slide moves at most this often.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// Gives the UI pages the same `chrome.webview` WebView2 does: `postMessage(string)` to the
/// host, and `message` events whose `data` is what the host sent (`View::post_json`).
/// Tested in tests/ui.mjs.
const HOST_SHIM: &str = include_str!("../../ui/mac-host.js");
/// The command bar closes when it loses focus (a click elsewhere, switching apps), like the
/// popup window on Windows.
const COMMAND_BLUR: &str = r#"window.addEventListener("blur", () => window.ipc.postMessage("__deactivated"));"#;
const DEACTIVATED: &str = "__deactivated";

enum UserEvent {
    /// Drain the browser's event queue.
    Wake,
    Menu(muda::MenuId),
}

/// A repeating timer, like `SetTimer` on Windows.
struct Timer {
    id: TimerId,
    every: Duration,
    next: Instant,
}

thread_local! {
    static PROXY: RefCell<Option<EventLoopProxy<UserEvent>>> = const { RefCell::new(None) };
    static WINDOW: RefCell<Option<Rc<Window>>> = const { RefCell::new(None) };
    static TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    /// When the sidebar slide's next frame is due.
    static ANIMATE_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static WAKE_PENDING: Cell<bool> = const { Cell::new(false) };
    static QUIT: Cell<bool> = const { Cell::new(false) };
    /// Tab webviews (by address) and their tabs, to tell which tab a click lands in.
    static TAB_VIEWS: RefCell<Vec<(usize, TabId)>> = const { RefCell::new(Vec::new()) };
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Regular);
    let proxy = event_loop.create_proxy();
    PROXY.with(|p| *p.borrow_mut() = Some(proxy.clone()));

    let menu = build_menu()?;
    menu.init_for_nsapp();
    let menu_proxy = Mutex::new(proxy);
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Ok(p) = menu_proxy.lock() {
            let _ = p.send_event(UserEvent::Menu(e.id));
        }
    }));

    let window = WindowBuilder::new()
        .with_title("New Tab")
        .with_inner_size(LogicalSize::new(1280.0, 820.0))
        .with_min_inner_size(LogicalSize::new(MIN_WIDTH, MIN_HEIGHT))
        .with_visible(false)
        .with_titlebar_transparent(true)
        .with_title_hidden(true)
        .with_fullsize_content_view(true)
        .with_traffic_light_inset(LogicalPosition::new(TRAFFIC_LIGHTS.0, TRAFFIC_LIGHTS.1))
        .with_automatic_window_tabbing(false)
        .build(&event_loop)?;
    let window = Rc::new(window);
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));

    let palette = Palette::new(window.theme() == Theme::Dark);
    let (r, g, b) = palette.strip;
    // Paint empty areas in the UI's own color so there is no white flash on start.
    window.set_background_color(Some((r, g, b, 255)));
    let host = Host::new(window.clone(), palette)?;
    install_click_monitor();

    let urls: Vec<String> = std::env::args().skip(1).collect();
    // Puts the window back where it was last time (if the session is restored).
    let maximized = browser::start(host, urls, palette)?;
    if maximized {
        window.set_maximized(true);
    }
    window.set_visible(true);
    window.set_focus();

    event_loop.run(move |event, _, control_flow| {
        // The menu bar lives as long as the loop.
        let _ = &menu;
        match event {
            LoopEvent::UserEvent(UserEvent::Wake) => {
                WAKE_PENDING.set(false);
                browser::drain_events();
            }
            LoopEvent::UserEvent(UserEvent::Menu(id)) => {
                if let Some(cmd) = menu_command(id.as_ref()) {
                    browser::post(Event::Command(cmd));
                }
            }
            LoopEvent::WindowEvent { event, .. } => match event {
                WindowEvent::Resized(_) => browser::with_browser_or_defer(|b| b.on_resize()),
                WindowEvent::Moved(_) => {
                    browser::with_browser(|b| b.on_move());
                }
                WindowEvent::Focused(true) => {
                    browser::with_browser(|b| b.on_window_focus());
                }
                WindowEvent::CloseRequested => request_quit(),
                _ => {}
            },
            LoopEvent::LoopDestroyed => browser::shutdown(),
            _ => {}
        }
        run_due_timers();
        if QUIT.get() {
            browser::shutdown();
            *control_flow = ControlFlow::Exit;
            return;
        }
        *control_flow = match next_deadline() {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        };
    });
}

/// Wake the event loop to drain the event queue.
pub fn wake() {
    if WAKE_PENDING.replace(true) {
        return;
    }
    PROXY.with(|p| {
        if let Some(p) = &*p.borrow() {
            let _ = p.send_event(UserEvent::Wake);
        }
    });
}

fn request_quit() {
    QUIT.set(true);
    wake();
}

pub fn run_window_actions(actions: Vec<WindowAction>) {
    let Some(window) = WINDOW.with(|w| w.borrow().clone()) else { return };
    for action in actions {
        match action {
            WindowAction::Minimize => window.set_minimized(true),
            WindowAction::Maximize => window.set_maximized(true),
            WindowAction::Restore => window.set_maximized(false),
            WindowAction::Close => request_quit(),
            WindowAction::Drag => drag_window(&ns_window_of(&window)),
        }
    }
}

/// Move the window with the mouse, from a press the sidebar's top row reported. That
/// press has been handled by the time the message arrives, so the drag starts from a
/// fresh one at the cursor (only while the button is still down).
fn drag_window(window: &NSWindow) {
    if NSEvent::pressedMouseButtons() & 1 == 0 {
        return;
    }
    let location = window.mouseLocationOutsideOfEventStream();
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::LeftMouseDown,
        location,
        NSEventModifierFlags::empty(),
        NSProcessInfo::processInfo().systemUptime(),
        window.windowNumber(),
        None,
        0,
        1,
        1.0,
    );
    if let Some(event) = event {
        window.performWindowDragWithEvent(&event);
    }
}

pub fn fatal(msg: &str) {
    if let Some(mtm) = MainThreadMarker::new() {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Browser"));
        alert.setInformativeText(&NSString::from_str(msg));
        alert.runModal();
    }
    std::process::exit(1);
}

/// WebKit needs nothing started first.
pub fn start_environment() -> Res<()> {
    browser::post(Event::EnvironmentReady(Ok(())));
    Ok(())
}

// ------------------------------------------------------------------ timers

fn run_due_timers() {
    let now = Instant::now();
    let due: Vec<TimerId> = TIMERS.with(|t| {
        let mut timers = t.borrow_mut();
        timers
            .iter_mut()
            .filter(|timer| timer.next <= now)
            .map(|timer| {
                timer.next = now + timer.every;
                timer.id
            })
            .collect()
    });
    for id in due {
        // An earlier timer may have killed this one.
        if TIMERS.with(|t| t.borrow().iter().any(|timer| timer.id == id)) {
            browser::with_browser(|b| b.on_timer(id));
        }
    }
    if ANIMATE_AT.get().is_some_and(|at| at <= now) {
        ANIMATE_AT.set(None);
        browser::with_browser(|b| b.animate());
    }
}

fn next_deadline() -> Option<Instant> {
    let timers = TIMERS.with(|t| t.borrow().iter().map(|timer| timer.next).min());
    match (timers, ANIMATE_AT.get()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

// ------------------------------------------------------------------ menu bar

fn build_menu() -> muda::Result<Menu> {
    let cmd = Modifiers::META;
    let item = |id: &str, text: &str, mods: Modifiers, key: Code| {
        MenuItem::with_id(id, text, true, Some(Accelerator::new(mods, key)))
    };
    let sep = PredefinedMenuItem::separator;

    let app = Submenu::with_items(
        "Browser",
        true,
        &[
            &PredefinedMenuItem::about(None, None),
            &sep(),
            &PredefinedMenuItem::services(None),
            &sep(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &sep(),
            &PredefinedMenuItem::quit(None),
        ],
    )?;
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("newTab", "New Tab", cmd, Code::KeyT),
            &item("reopenTab", "Reopen Closed Tab", cmd | Modifiers::SHIFT, Code::KeyT),
            &item("editAddress", "Open Location…", cmd, Code::KeyL),
            &sep(),
            &item("closeTab", "Close Tab", cmd, Code::KeyW),
        ],
    )?;
    // Without these, ⌘C/⌘V/⌘A/⌘Z do nothing in web pages on macOS.
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &sep(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item("toggleSidebar", "Show or Hide Sidebar", cmd, Code::KeyS),
            &item("toggleSplit", "Split View", cmd, Code::Backslash),
            &sep(),
            &item("reload", "Reload Page", cmd, Code::KeyR),
            &sep(),
            &item("toggleFullscreen", "Full Screen", cmd | Modifiers::CONTROL, Code::KeyF),
        ],
    )?;
    let history = Submenu::with_items(
        "History",
        true,
        &[&item("back", "Back", cmd, Code::BracketLeft), &item("forward", "Forward", cmd, Code::BracketRight)],
    )?;
    let bookmarks =
        Submenu::with_items("Bookmarks", true, &[&item("toggleBookmark", "Bookmark This Page", cmd, Code::KeyD)])?;

    let tabs = Submenu::with_items(
        "Tabs",
        true,
        &[
            &item("nextTab", "Next Tab", cmd | Modifiers::ALT, Code::ArrowRight),
            &item("prevTab", "Previous Tab", cmd | Modifiers::ALT, Code::ArrowLeft),
            &sep(),
        ],
    )?;
    const DIGITS: [Code; 9] = [
        Code::Digit1,
        Code::Digit2,
        Code::Digit3,
        Code::Digit4,
        Code::Digit5,
        Code::Digit6,
        Code::Digit7,
        Code::Digit8,
        Code::Digit9,
    ];
    for (i, &digit) in DIGITS.iter().enumerate().take(8) {
        tabs.append(&item(&format!("tab:{i}"), &format!("Tab {}", i + 1), cmd, digit))?;
    }
    tabs.append(&item("tab:last", "Last Tab", cmd, Code::Digit9))?;
    tabs.append(&sep())?;
    for (i, &digit) in DIGITS.iter().enumerate() {
        tabs.append(&item(&format!("space:{i}"), &format!("Space {}", i + 1), Modifiers::CONTROL, digit))?;
    }

    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &sep(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ],
    )?;
    window.set_as_windows_menu_for_nsapp();

    Menu::with_items(&[&app, &file, &edit, &view, &history, &bookmarks, &tabs, &window])
}

/// The browser command for a menu item's id.
fn menu_command(id: &str) -> Option<Command> {
    use Command::*;
    Some(match id {
        "newTab" => NewTab,
        "reopenTab" => ReopenClosedTab,
        "editAddress" => EditAddress,
        "closeTab" => CloseTab,
        "toggleSidebar" => ToggleSidebar,
        "toggleSplit" => ToggleSplit,
        "reload" => Reload,
        "toggleFullscreen" => ToggleFullscreen,
        "back" => Back,
        "forward" => Forward,
        "toggleBookmark" => ToggleBookmark,
        "nextTab" => NextTab,
        "prevTab" => PrevTab,
        "tab:last" => SelectTab(usize::MAX),
        _ => {
            if let Some(n) = id.strip_prefix("tab:") {
                SelectTab(n.parse().ok()?)
            } else {
                SelectSpace(id.strip_prefix("space:")?.parse().ok()?)
            }
        }
    })
}

// ------------------------------------------------------------------ views

define_class!(
    /// The window's content: everything else sits in it. Flipped, so frames are measured
    /// from the top left like on Windows; dragging its bare margins moves the window.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BrowserRootView"]
    struct RootView;

    impl RootView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            true
        }

        /// From its tracking area, wherever the cursor is in the window (over webviews too):
        /// touching the left edge slides a hidden sidebar out.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            browser::with_browser(|b| b.on_mouse_move(p.x as i32));
        }
    }
);

define_class!(
    /// Holds webviews: the page cards, the sidebar, the command bar.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BrowserPaneView"]
    struct PaneView;

    impl PaneView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl RootView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // AppKit sends mouse moves to the webview under the cursor; a tracking area also
        // reports them here.
        let options =
            NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::ActiveInKeyWindow | NSTrackingAreaOptions::InVisibleRect;
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, options, Some(&this), None)
        };
        this.addTrackingArea(&area);
        this
    }
}

impl PaneView {
    /// A layer-backed pane in `parent` (which is already in the window, so the layer exists).
    /// `clip`: its webview is clipped to the pane's rounded corners.
    fn new(mtm: MainThreadMarker, parent: &NSView, clip: bool) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        parent.addSubview(&this);
        this.setWantsLayer(true);
        if let Some(layer) = this.layer() {
            layer.setMasksToBounds(clip);
        }
        this
    }

    fn set_color(&self, (r, g, b): Rgb) {
        if let Some(layer) = self.layer() {
            let color = CGColor::new_srgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0);
            layer.setBackgroundColor(Some(&color));
        }
    }

    /// Round the corners, clipping what's inside to them.
    fn set_radius(&self, radius: f64) {
        if let Some(layer) = self.layer() {
            layer.setCornerRadius(radius);
            layer.setMasksToBounds(true);
        }
    }
}

fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(NSPoint::new(r.left as f64, r.top as f64), NSSize::new(r.width().max(0) as f64, r.height().max(0) as f64))
}

fn wry_rect(r: Rect) -> wry::Rect {
    wry::Rect {
        position: LogicalPosition::new(r.left as f64, r.top as f64).into(),
        size: LogicalSize::new(r.width().max(0) as f64, r.height().max(0) as f64).into(),
    }
}

fn ns_window_of(window: &Window) -> Retained<NSWindow> {
    unsafe { Retained::retain(window.ns_window() as *mut NSWindow) }.expect("a tao window has an NSWindow")
}

/// Lets wry put a webview inside one of our panes (it adds webviews to the view it's given).
struct Parent(NonNull<c_void>);

impl Parent {
    fn of(view: &NSView) -> Parent {
        Parent(NonNull::from(view).cast())
    }
}

impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

/// In split view a click into the other side makes that side the active tab (WebView2's
/// `GotFocus` on Windows). AppKit has no such event for webviews, so every click is looked at.
fn install_click_monitor() {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        if let Some(id) = tab_under(unsafe { event.as_ref() }) {
            browser::post(Event::TabFocused(id));
        }
        event.as_ptr()
    });
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::LeftMouseDown, &block) };
    // Watches for as long as the app runs.
    std::mem::forget(monitor);
}

fn tab_under(event: &NSEvent) -> Option<TabId> {
    let mtm = MainThreadMarker::new()?;
    let window = event.window(mtm)?;
    let hit = window.contentView()?.hitTest(event.locationInWindow())?;
    let mut view = Some(hit);
    while let Some(v) = view {
        let address = Retained::as_ptr(&v) as usize;
        if let Some(id) = TAB_VIEWS.with(|t| t.borrow().iter().find(|(a, _)| *a == address).map(|(_, id)| *id)) {
            return Some(id);
        }
        view = unsafe { v.superview() };
    }
    None
}

// ------------------------------------------------------------------ the window

/// The main window and its panes.
pub struct Host {
    window: Rc<Window>,
    ns_window: Retained<NSWindow>,
    root: Retained<RootView>,
    /// The page cards; the second is the right side of a split.
    panes: [Retained<PaneView>; 2],
    sidebar: Retained<PaneView>,
    /// Draws the command bar's shadow, around `command`, which clips it to rounded corners.
    command_frame: Retained<PaneView>,
    command: Retained<PaneView>,
    /// The window's place before full screen.
    normal_placement: Cell<Option<(Rect, bool)>>,
    traffic_lights_shown: Cell<bool>,
}

impl Host {
    fn new(window: Rc<Window>, palette: Palette) -> Res<Host> {
        let mtm = MainThreadMarker::new().ok_or("the browser must run on the main thread")?;
        let ns_window = ns_window_of(&window);
        let content = ns_window.contentView().ok_or("the window has no content view")?;
        let root = RootView::new(mtm, content.bounds());
        root.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        content.addSubview(&root);
        root.setWantsLayer(true);

        // Added bottom to top: the sidebar stacks above the page so it can slide over it.
        let panes = [PaneView::new(mtm, &root, true), PaneView::new(mtm, &root, true)];
        panes[1].setHidden(true);
        let sidebar = PaneView::new(mtm, &root, true);
        let command_frame = PaneView::new(mtm, &root, false);
        if let Some(layer) = command_frame.layer() {
            layer.setShadowOpacity(if palette.dark { 0.5 } else { 0.22 });
            layer.setShadowRadius(18.0);
            layer.setShadowOffset(NSSize::new(0.0, -6.0));
        }
        let command = PaneView::new(mtm, &command_frame, true);
        command.set_radius(COMMAND_RADIUS);
        command.set_color(palette.panel);
        sidebar.set_color(palette.strip);

        let host = Host {
            window,
            ns_window,
            root,
            panes,
            sidebar,
            command_frame,
            command,
            normal_placement: Cell::new(None),
            traffic_lights_shown: Cell::new(true),
        };
        host.place_command_bar(Rect { left: 0, top: 0, right: 680, bottom: 58 }, false);
        Ok(host)
    }

    pub fn set_env(&mut self, _env: Env) {}

    pub fn create_ui_view(&self, surface: Surface, ui_dir: &str, background: Rgb) -> Res<()> {
        let (parent, script) = match surface {
            Surface::Sidebar => (&self.sidebar, HOST_SHIM.to_string()),
            Surface::CommandBar => (&self.command, format!("{HOST_SHIM}\n{COMMAND_BLUR}")),
        };
        let dir = ui_dir.to_string();
        let (r, g, b) = background;
        let webview = WebViewBuilder::new()
            .with_bounds(wry_rect(Rect { left: 0, top: 0, right: 200, bottom: 200 }))
            .with_background_color((r, g, b, 255))
            .with_initialization_script_for_main_only(script, true)
            .with_custom_protocol(SCHEME.into(), move |_, request| serve_ui(&dir, &request))
            .with_ipc_handler(move |request: Request<String>| {
                if !request.uri().to_string().starts_with(surface.url()) {
                    return;
                }
                let msg = request.into_body();
                if surface == Surface::CommandBar && msg == DEACTIVATED {
                    browser::post(Event::CommandBarDeactivated);
                } else {
                    browser::post(Event::UiMessage(surface, msg));
                }
            })
            // The UI webview must only ever show our own pages; anything else opens as a tab.
            .with_navigation_handler(|url| {
                if url.starts_with(UI_ORIGIN) {
                    return true;
                }
                browser::post(Event::OpenInNewTab(url));
                false
            })
            .with_new_window_req_handler(|url, _| {
                browser::post(Event::OpenInNewTab(url));
                NewWindowResponse::Deny
            })
            .with_devtools(cfg!(debug_assertions))
            .with_accept_first_mouse(true)
            // Making a webview must not bring the app to the front (focus is handled here).
            .with_focused(false)
            .build_as_child(&Parent::of(parent))
            .map_err(|e| e.to_string())?;
        browser::post(Event::UiViewReady(surface, Ok(View::new(webview, None))));
        Ok(())
    }

    pub fn create_tab_view(&self, id: TabId, pane: usize, ui_dir: &str, background: Rgb) -> Res<()> {
        let dir = ui_dir.to_string();
        let (r, g, b) = background;
        let webview = WebViewBuilder::new()
            .with_bounds(wry_rect(Rect { left: 0, top: 0, right: 200, bottom: 200 }))
            .with_background_color((r, g, b, 255))
            // The built-in new tab page.
            .with_custom_protocol(SCHEME.into(), move |_, request| serve_ui(&dir, &request))
            .with_on_page_load_handler(move |event, url| match event {
                PageLoadEvent::Started => browser::post(Event::TabNavigating(id, url)),
                PageLoadEvent::Finished => browser::post(Event::TabLoaded(id)),
            })
            .with_document_title_changed_handler(move |_| browser::post(Event::TabUpdated(id)))
            // Popups and target=_blank links open as tabs.
            .with_new_window_req_handler(|url, _| {
                browser::post(Event::OpenInNewTab(url));
                NewWindowResponse::Deny
            })
            // Into ~/Downloads (without a handler WebKit downloads are cancelled).
            .with_download_started_handler(|_, _| true)
            .with_devtools(true)
            .with_back_forward_navigation_gestures(true)
            .with_accept_first_mouse(true)
            .with_focused(false)
            .build_as_child(&Parent::of(&self.panes[pane]))
            .map_err(|e| e.to_string())?;
        let view = View::new(webview, Some(id));
        browser::post(Event::TabViewReady(id, Ok(view)));
        Ok(())
    }

    /// Move a tab's webview into card pane `pane`.
    pub fn move_to_pane(&self, view: &View, pane: usize) -> Res<()> {
        let wk = &view.0.wk;
        wk.removeFromSuperview();
        self.panes[pane].addSubview(wk);
        Ok(())
    }

    /// Layout is in points already.
    pub fn scale(&self, dip: i32) -> i32 {
        dip
    }

    pub fn client_rect(&self) -> Rect {
        let size = self.root.bounds().size;
        Rect { left: 0, top: 0, right: size.width as i32, bottom: size.height as i32 }
    }

    /// A repeating timer; `Browser::on_timer(id)` runs each time until it's killed.
    pub fn set_timer(&self, id: TimerId, ms: u32) {
        let every = Duration::from_millis(ms as u64);
        TIMERS.with(|t| {
            let mut timers = t.borrow_mut();
            timers.retain(|timer| timer.id != id);
            timers.push(Timer { id, every, next: Instant::now() + every });
        });
    }

    pub fn kill_timer(&self, id: TimerId) {
        TIMERS.with(|t| t.borrow_mut().retain(|timer| timer.id != id));
    }

    pub fn close_window(&self) {
        request_quit();
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    pub fn is_minimized(&self) -> bool {
        self.window.is_minimized()
    }

    pub fn is_maximized(&self) -> bool {
        self.window.is_maximized()
    }

    /// The cursor, in points from the window's top left.
    pub fn cursor_pos(&self) -> (i32, i32) {
        let p = self.ns_window.mouseLocationOutsideOfEventStream();
        let height = self.root.bounds().size.height;
        (p.x as i32, (height - p.y) as i32)
    }

    pub fn left_button_down(&self) -> bool {
        NSEvent::pressedMouseButtons() & 1 != 0
    }

    /// Fill the screen in place (no new Space, no animation), like full screen on Windows.
    pub fn set_fullscreen(&mut self, on: bool) {
        if on {
            self.normal_placement.set(Some(self.current_placement()));
        }
        self.window.set_simple_fullscreen(on);
        if !on {
            self.normal_placement.set(None);
        }
        self.update_traffic_lights(!on);
    }

    fn current_placement(&self) -> (Rect, bool) {
        let scale = self.window.scale_factor();
        let position = self.window.outer_position().map(|p| p.to_logical::<f64>(scale)).unwrap_or(LogicalPosition::new(0.0, 0.0));
        let size = self.window.outer_size().to_logical::<f64>(scale);
        let (x, y) = (position.x as i32, position.y as i32);
        let rect = Rect { left: x, top: y, right: x + size.width as i32, bottom: y + size.height as i32 };
        (rect, self.window.is_maximized())
    }

    /// The window's place (in points, from the top left of the main screen), and whether
    /// it's zoomed.
    pub fn placement(&self) -> (Rect, bool) {
        self.normal_placement.get().unwrap_or_else(|| self.current_placement())
    }

    /// Put the (still hidden) window where it was last time, if that's on a screen now.
    pub fn restore_placement(&self, r: Rect) -> bool {
        let on_screen = self.window.available_monitors().any(|m| {
            let scale = m.scale_factor();
            let (p, s) = (m.position().to_logical::<f64>(scale), m.size().to_logical::<f64>(scale));
            let (x, y) = (r.left as f64, r.top as f64);
            x < p.x + s.width && x + r.width() as f64 > p.x && y < p.y + s.height && y + r.height() as f64 > p.y
        });
        if !on_screen {
            return false;
        }
        self.window.set_outer_position(LogicalPosition::new(r.left as f64, r.top as f64));
        self.window.set_inner_size(LogicalSize::new(r.width() as f64, r.height() as f64));
        true
    }

    /// The color card pane `pane` shows where its webview hasn't drawn yet.
    pub fn set_pane_color(&self, pane: usize, rgb: Rgb) {
        self.panes[pane].set_color(rgb);
    }

    /// Round card pane `pane`'s corners.
    pub fn shape_pane(&mut self, pane: usize, _width: i32, _height: i32, radius: i32) {
        self.panes[pane].set_radius(radius as f64);
    }

    /// Move the card panes and the sidebar together; `split_change` shows or hides the
    /// second card.
    pub fn move_panes(&self, cards: [Rect; 2], split_change: Option<bool>, sidebar: Rect) {
        for (pane, r) in self.panes.iter().zip(cards) {
            pane.setFrame(ns_rect(r));
        }
        if let Some(show) = split_change {
            self.panes[1].setHidden(!show);
        }
        self.sidebar.setFrame(ns_rect(sidebar));
        self.update_traffic_lights(sidebar.right >= TRAFFIC_LIGHTS_WIDTH);
    }

    /// One frame of the sidebar's slide.
    pub fn slide_sidebar(&self, r: Rect) {
        self.sidebar.setFrameOrigin(NSPoint::new(r.left as f64, r.top as f64));
        self.update_traffic_lights(r.right >= TRAFFIC_LIGHTS_WIDTH);
    }

    /// Run `Browser::animate` again at the next frame.
    pub fn request_animate(&self) {
        if ANIMATE_AT.get().is_none() {
            ANIMATE_AT.set(Some(Instant::now() + FRAME_INTERVAL));
        }
    }

    /// The traffic lights show over the sidebar, and hide with it (they'd sit over the page).
    fn update_traffic_lights(&self, show: bool) {
        if self.traffic_lights_shown.replace(show) == show {
            return;
        }
        for kind in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
            if let Some(button) = self.ns_window.standardWindowButton(kind) {
                button.setHidden(!show);
            }
        }
    }

    /// Put the command bar at `r`, or out of sight while it's closed (still rendering).
    pub fn place_command_bar(&self, r: Rect, shown: bool) {
        let at = if shown { r } else { Rect { left: -20000, top: -20000, right: -20000 + r.width(), bottom: -20000 + r.height() } };
        self.command_frame.setFrame(ns_rect(at));
        self.command.setFrame(ns_rect(Rect { left: 0, top: 0, right: r.width(), bottom: r.height() }));
    }

    pub fn show_command_window(&self) {}

    pub fn hide_command_window(&self) {}

    /// After the command bar is hidden: don't leave keyboard focus in it.
    pub fn command_window_hidden(&self) {
        if self.focus_in(&self.command) {
            self.ns_window.makeFirstResponder(Some(&self.root));
        }
    }

    /// The command bar has keyboard focus in the active window.
    pub fn command_bar_active(&self) -> bool {
        self.ns_window.isKeyWindow() && self.focus_in(&self.command)
    }

    fn focus_in(&self, view: &NSView) -> bool {
        let Some(responder) = self.ns_window.firstResponder() else { return false };
        match responder.downcast::<NSView>() {
            Ok(v) => v.isDescendantOf(view),
            Err(_) => false,
        }
    }
}

/// A file from the UI folder for `browser://ui/<path>`.
fn serve_ui(ui_dir: &str, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let file = ui_file(Path::new(ui_dir), request.uri().path());
    match file.as_ref().and_then(|f| std::fs::read(f).ok()) {
        Some(bytes) => Response::builder()
            .header("Content-Type", mime_type(file.as_deref().unwrap_or(Path::new(""))))
            .body(Cow::Owned(bytes))
            .unwrap_or_default(),
        None => Response::builder().status(404).body(Cow::Borrowed(&[][..])).unwrap_or_default(),
    }
}

/// The file a URL path names inside the UI folder. Nothing outside it can be named.
fn ui_file(ui_dir: &Path, path: &str) -> Option<PathBuf> {
    let path = path.trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    if path.is_empty() || parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\\')) {
        return None;
    }
    Some(parts.iter().fold(ui_dir.to_path_buf(), |dir, p| dir.join(p)))
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

// ------------------------------------------------------------------ webviews

struct ObserverIvars {
    webview: Retained<WryWebView>,
    tab: TabId,
}

define_class!(
    /// Tells the browser when a tab's address or back/forward state changes (WebView2's
    /// `SourceChanged` and `HistoryChanged`). Titles come from wry.
    #[unsafe(super(NSObject))]
    #[ivars = ObserverIvars]
    #[name = "BrowserPageObserver"]
    struct PageObserver;

    impl PageObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_key_path(
            &self,
            _key_path: Option<&NSString>,
            _of_object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut c_void,
        ) {
            browser::post(Event::TabUpdated(self.ivars().tab));
        }
    }

    unsafe impl NSObjectProtocol for PageObserver {}
);

const OBSERVED_KEYS: [&str; 3] = ["URL", "canGoBack", "canGoForward"];

impl PageObserver {
    fn new(webview: Retained<WryWebView>, tab: TabId) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ObserverIvars { webview, tab });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        for key in OBSERVED_KEYS {
            unsafe {
                this.ivars().webview.addObserver_forKeyPath_options_context(
                    &this,
                    &NSString::from_str(key),
                    NSKeyValueObservingOptions::New,
                    null_mut(),
                );
            }
        }
        this
    }
}

impl Drop for PageObserver {
    fn drop(&mut self) {
        for key in OBSERVED_KEYS {
            unsafe { self.ivars().webview.removeObserver_forKeyPath(self, &NSString::from_str(key)) };
        }
    }
}

struct ViewInner {
    webview: WebView,
    wk: Retained<WryWebView>,
    _observer: Option<Retained<PageObserver>>,
}

impl Drop for ViewInner {
    fn drop(&mut self) {
        // Its address may be reused, so it must not be mistaken for the tab any more.
        let address = Retained::as_ptr(&self.wk) as usize;
        TAB_VIEWS.with(|t| t.borrow_mut().retain(|(a, _)| *a != address));
    }
}

/// One WebKit webview.
#[derive(Clone)]
pub struct View(Rc<ViewInner>);

impl View {
    fn new(webview: WebView, tab: Option<TabId>) -> View {
        let wk = webview.webview();
        let observer = tab.map(|id| PageObserver::new(wk.clone(), id));
        if let Some(id) = tab {
            TAB_VIEWS.with(|t| t.borrow_mut().push((Retained::as_ptr(&wk) as usize, id)));
        }
        View(Rc::new(ViewInner { webview, wk, _observer: observer }))
    }

    /// Set up when it was made.
    pub fn configure_ui(&self, _surface: Surface, _ui_dir: &str, _background: Rgb) -> Res<()> {
        Ok(())
    }

    /// Set up when it was made.
    pub fn configure_tab(&self, _id: TabId, _ui_dir: &str) -> Res<()> {
        Ok(())
    }

    pub fn navigate(&self, url: &str) -> Res<()> {
        self.0.webview.load_url(url).map_err(|e| e.to_string())
    }

    pub fn reload(&self) {
        let _ = self.0.webview.reload();
    }

    pub fn stop(&self) {
        unsafe { self.0.wk.stopLoading() };
    }

    pub fn back(&self) {
        let _ = unsafe { self.0.wk.goBack() };
    }

    pub fn forward(&self) {
        let _ = unsafe { self.0.wk.goForward() };
    }

    /// Bounds relative to the webview's pane.
    pub fn set_bounds(&self, r: Rect) {
        let _ = self.0.webview.set_bounds(wry_rect(r));
    }

    pub fn set_visible(&self, visible: bool) {
        let _ = self.0.webview.set_visible(visible);
    }

    pub fn set_background(&self, (r, g, b): Rgb) {
        let _ = self.0.webview.set_background_color((r, g, b, 255));
    }

    pub fn focus(&self) {
        let _ = self.0.webview.focus();
    }

    pub fn close(&self) {
        let _ = self.0.webview.set_visible(false);
        self.0.wk.removeFromSuperview();
    }

    /// The page receives it as a `message` event whose `data` is the parsed JSON (see
    /// `HOST_SHIM`).
    pub fn post_json(&self, json: &str) {
        let _ = self.0.webview.evaluate_script(&format!("window.__browserReceive && window.__browserReceive({json})"));
    }

    pub fn eval(&self, js: &str) {
        let _ = self.0.webview.evaluate_script(js);
    }

    /// Run a script and post `event(id, result)`, the result as JSON.
    pub fn eval_with_result(&self, js: &str, id: TabId, event: fn(TabId, String) -> Event) {
        let _ = self.0.webview.evaluate_script_with_callback(js, move |result| browser::post(event(id, result)));
    }

    pub fn state(&self) -> PageState {
        let wk = &self.0.wk;
        unsafe {
            PageState {
                url: self.0.webview.url().ok(),
                title: wk.title().map(|t| t.to_string()),
                can_back: Some(wk.canGoBack()),
                can_forward: Some(wk.canGoForward()),
                // WebKit doesn't say whether a page plays sound, or let it be muted.
                audible: None,
                muted: None,
                // Asked from the page instead (FAVICON_SCRIPT).
                favicon: None,
            }
        }
    }

    pub fn toggle_mute(&self) {}

    /// WebKit manages background pages' memory itself.
    pub fn set_memory_target(&self, _visible: bool) {}

    /// WebKit has no way to freeze a page; it throttles hidden ones itself.
    pub fn try_suspend(&self, _id: TabId) -> bool {
        false
    }

    /// WebKit shows a page's full screen element in its own window.
    pub fn contains_fullscreen(&self) -> Option<bool> {
        None
    }

    pub fn notify_parent_moved(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_items_map_to_commands() {
        assert!(matches!(menu_command("newTab"), Some(Command::NewTab)));
        assert!(matches!(menu_command("tab:0"), Some(Command::SelectTab(0))));
        assert!(matches!(menu_command("tab:last"), Some(Command::SelectTab(usize::MAX))));
        assert!(matches!(menu_command("space:8"), Some(Command::SelectSpace(8))));
        assert!(menu_command("space:x").is_none());
        assert!(menu_command("about").is_none());
    }

    #[test]
    fn ui_files_stay_inside_the_ui_folder() {
        let dir = Path::new("/ui");
        assert_eq!(ui_file(dir, "/chrome.html"), Some(PathBuf::from("/ui/chrome.html")));
        assert_eq!(ui_file(dir, "/a/b.css"), Some(PathBuf::from("/ui/a/b.css")));
        assert_eq!(ui_file(dir, "/../secret"), None);
        assert_eq!(ui_file(dir, "/a/../../secret"), None);
        assert_eq!(ui_file(dir, "/"), None);
        assert_eq!(ui_file(dir, "//etc/passwd"), None);
        assert_eq!(mime_type(Path::new("x/chrome.js")), "text/javascript; charset=utf-8");
    }
}
