# Browser

A lightweight browser for Windows and macOS: a small native Rust shell around the system's
web engine, Microsoft Edge WebView2 on Windows and WebKit (Safari's engine) on macOS. Tabs live
in a vertical sidebar written in HTML/CSS, and the page sits as a rounded card beside it, in a
window with no standard title bar. (It has no product name yet; the UI deliberately shows none.)

Arc-style tab handling: **spaces** (separate sets of tabs, each with a name and color),
**pinned tabs** above a line and **today tabs** below it, today tabs left unseen for 12 hours
**archived** automatically, **split view** (two tabs side by side), and **history and bookmarks**
searched from the command bar.

## Run

```powershell
cargo run                                # restores your last session (or opens a new tab)
cargo run -- example.com github.com      # opens these URLs as tabs instead
cargo build --release                    # small exe, see target dir below
```

Windows needs the Rust MSVC toolchain and the WebView2 runtime (ships with Windows 11).
Build output goes to `C:/Users/luvma/.cargo-target/singularity-browser` (see
`.cargo/config.toml`) so OneDrive doesn't sync build artifacts. The exe is `browser.exe`.

macOS needs Rust and the Xcode command line tools (`xcode-select --install`); WebKit ships
with the system. The same commands work. `.cargo/config.toml` holds a Windows path, so on a Mac
set your own build folder first, or cargo makes a folder named `C:` inside the project:

```sh
export CARGO_TARGET_DIR=~/.cargo-target/browser
cargo run
```

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | Picks the platform and starts it |
| `src/browser.rs` | The browser itself, the same on both systems: tabs, spaces, pinning, archiving, split view, layout and the sidebar slide, UI messaging, tab sleeping, fullscreen. Everything native goes through `platform::Host` (the window, its panes, timers) and `platform::View` (one webview) |
| `src/library.rs` | History, bookmarks and archived tabs, and the search over them (unit tested) |
| `src/session.rs` | Profile folder; saving and restoring spaces, tabs, the split, sidebar and window placement; where the library is kept |
| `src/theme.rs` | Light/dark palette shared with the CSS |
| `src/win/mod.rs` | Windows: Win32 window and message loop, WebView2 setup and events, keyboard shortcuts |
| `src/win/frame.rs` | Windows: custom window frame (no title bar), hit-testing, the child "panes" that hold webviews, the command bar's popup window |
| `src/mac/mod.rs` | macOS: the window (tao), WebKit webviews (wry), pane views, the menu bar and its shortcuts (muda), timers |
| `build.rs`, `app.manifest` | Embeds the Windows app manifest (OS compatibility, DPI awareness, modern dialogs) |
| `ui/chrome.html` `.css` `.js` | Sidebar: window buttons, current site, bookmark star, space name, pinned and today tabs, tab and space menus, space switcher, archive button (its own webview, at `https://browser.ui/` on Windows and `browser://ui/` on macOS) |
| `ui/command.html` `.css` `.js` | Command bar (Ctrl+T / Ctrl+L): address or search, open tabs, bookmarks and history; also the archive search |
| `ui/address.js` | Turns typed text into a URL or search |
| `ui/newtab.html` | New tab page (a quiet clock and date) |
| `ui/mac-host.js` | macOS: gives the UI pages the `chrome.webview` object they use on Windows, over wry's IPC |
| `tests/` | `address.test.js` (unit), `ui.mjs` (UI pages, headless, fake host), `e2e.mjs` (drives the real browser, Windows) |

### How it fits together

- **One browser, two shells.** `browser.rs` holds all the state and logic and calls only
  `platform::Host` and `platform::View`. `src/win` implements them with Win32 and WebView2,
  `src/mac` with AppKit and WebKit. A feature written in `browser.rs` and `ui/` works on both;
  only something the engine itself must do needs a method in each platform.
- **One shared WebView2 environment** (Windows). All tabs share one browser process, one GPU
  process and one network process. Each tab has its own renderer process.
- **The Rust side owns all state.** Webview callbacks only queue an `Event`; the event loop
  handles events one at a time (no re-entrancy). After each batch, the full tab list is sent to
  the UI as one JSON message, and the UI just renders it.
- **UI → Rust:** `chrome.webview.postMessage(JSON.stringify({cmd, ...}))`. Commands:
  `navigate, back, forward, reload, stop, newTab, closeTab, activate, moveTab {id, to, pinned?}, reopenTab, toggleMute, focusContent, reloadUi`,
  `pin, unpin, resetPin, setPinHere, clearToday, openArchive, toggleBookmark, splitWith {id}, separate, toggleSplit`,
  `switchSpace, newSpace, renameSpace, spaceColor, deleteSpace, moveToSpace {id, space}`,
  plus `openCommandBar, toggleSidebar, sidebarWidth, minimize, maximize, close, dragWindow` for the window.
  The command bar sends `present {height}`, `height`, `query {text, seq}` (answered with
  `{type: "suggestions", seq, items}`), `submit {url, newTab}`, `switchTab {id}`, `restore {url}` and `close`.
- **Split view:** a second card pane sits beside the first. The right-hand tab's webview is moved
  into it with `SetParentWindow`, and back when the split ends. A split shows whenever one of its
  tabs is the active tab; a click into the other side makes that side active (`GotFocus`).
- **Window frame:** `WM_NCCALCSIZE` removes the title bar but keeps the native resize borders,
  snapping, shadow and rounded corners. The sidebar's top row is the title bar (`app-region: drag`).
  The page card is a child window clipped to rounded corners; the 8 px margin around it belongs
  to the main window, which uses it for resizing, dragging, and (left edge) revealing a hidden sidebar.
- **No flicker:**
  - The UI webviews never stop rendering. A hidden webview drops its picture and shows an empty
    frame when it comes back, so a hidden sidebar is just moved off the left edge.
  - The command bar is a separate popup window. When closed it is cloaked and parked off screen,
    still rendering. Its webview is always its full maximum height, so a longer result list never
    needs a new layout.
  - When the command bar opens, the host only reveals it after the page answers `present` (two
    animation frames after drawing what it was opened with). It never shows stale text.
  - Panes move together in one `DeferWindowPos` batch, and window regions and webview bounds are
    only re-set when they change.
  - Hiding the sidebar widens the page first, while the card still clips it to its old size. A
    script asks the page to post `__browser_frame_<n>` three animation frames later, and only
    then does the card grow and the sidebar slide away. Without this, the strip that opens on the
    right flashed the webview's plain background for a frame or two. If the page doesn't answer
    within 150 ms, it goes ahead anyway.
  - Each tab's default background follows the color behind the page's right edge (read after
    load and before hiding the sidebar). So when the window is resized, the frame or two before
    the page redraws isn't a contrasting strip.
- **Sidebar toggle (Ctrl+S)** slides, paced by `DwmFlush` so it moves on every screen refresh.
  Hiding: the page grows underneath at once and the sidebar slides away over it. Showing: the page
  makes room and the sidebar slides in. The hover-to-peek slide uses the same animation.
- **Command bar** is a top-level popup owned by the main window, so Windows gives it real rounded
  corners and a shadow. Losing activation (clicking the page, Alt+Tab) closes it. It only takes the
  foreground if the browser already has it.
- **UI edits don't need a rebuild:** `ui/` is served from the source folder in development. Press
  **Ctrl+Shift+F5** in the browser to reload the UI.
  - The colors in `theme.rs` must match `--strip` (chrome.css), `--panel` (command.css) and `--bg` (newtab.html).

### Spaces, pinned tabs and the archive

- Each **space** has its own pinned and today tabs and remembers which tab it showed. Switch with
  the dots at the bottom of the sidebar or Alt+1…9; **+** makes a new one (then type its name).
  Right-click a dot (or the space name) to rename it, change its color or delete it. Deleting a
  space moves its tabs, pinned ones too, to the archive.
- **Pinned tabs** sit above the line. Drag a tab across the line, or use its right-click menu, to
  pin or unpin it. Pinning keeps the page it's on as the tab's own: closing a pinned tab (Ctrl+W)
  only unloads it, back to that page, and if you wander off, a button on the tab (and the menu)
  takes you back. "Pin this page instead" makes the current page its own.
- **Today tabs** sit below the line, newest at the top. Ones not on screen for 12 hours are
  archived (override with `BROWSER_ARCHIVE_SECS`), except tabs playing sound and the tab each
  space shows. **Clear** (hover the line) archives them all now.
- The **archive** holds closed and archived tabs (the newest 500). The box button at the bottom of the
  sidebar searches it; Enter reopens one.

### History and bookmarks

Every new address a tab shows is a visit. Ctrl+D (or the star in the address) bookmarks the
page. As you type, the command bar lists matching open tabs, then bookmarks and history: every
word must appear in the title or address; bookmarks, sites whose name starts with what you typed,
and pages you visit often and recently rank higher. History keeps 90 days (at most 10,000 pages).
All of it is kept in `library.json` in the profile folder, written at most every 10 seconds and on exit.

### Session

Spaces, tabs (URL, title, favicon, space, pinned page, when last seen), each space's shown tab, the
split, the sidebar width and shown/hidden state, and the window's place are saved every 2 seconds
when they change (and on exit) to `session.json` in the profile folder. Sessions saved by earlier
builds (one list of tabs) still load. On the next start they come back. Restored tabs stay asleep, with no webview,
until you first open them. Passing URLs on the command line skips the restore.

The profile lives in `%LOCALAPPDATA%\Browser\User Data` on Windows and
`~/Library/Application Support/Browser/User Data` on macOS (override with `BROWSER_USER_DATA`).
A profile from the earlier `%LOCALAPPDATA%\Singularity` folder is moved there once, keeping logins.
On macOS that folder holds the session and the library; WebKit keeps cookies and site data in
its own store for the app (`~/Library/WebKit`), which `BROWSER_USER_DATA` doesn't move.

### Memory

- **Background tabs:** set to `MemoryUsageTargetLevel = Low` as soon as they're hidden.
- **Idle background tabs:** after 5 minutes they're frozen with `TrySuspend` and shown dimmed.
  Set the `BROWSER_SUSPEND_SECS` environment variable to change the delay. They wake
  instantly when selected.
- **Restored tabs** don't load at all until opened.
- **Minimizing:** hides the active webview, so nothing renders.

### Also

- Thin overlay scrollbars (Fluent style), password saving and form autofill.
- A speaker icon on tabs playing sound; click it to mute the tab.
- Hover the address to copy the page's link.

### On macOS

The same sidebar, spaces, pinned and today tabs, archive, split view, command bar, history,
bookmarks and session. What's different:

- **Window:** the native traffic lights sit at the top left of the sidebar (the sidebar's own
  window buttons are hidden) and hide with it. Drag the sidebar's top row to move the window.
- **Shortcuts are menu items** with ⌘ (see the table below), which also makes copy, paste,
  undo and select all work in pages. The sidebar's hints show ⌘ too.
- **Full screen** (⌃⌘F) fills the screen in place. The green button still gives macOS's own
  full screen, as a bigger window.
- **Engine:** pages render with WebKit, so sites behave as in Safari. Two-finger swipes go
  back and forward. Downloads go straight to ~/Downloads.
- **Not on macOS (WebKit doesn't offer them here):** freezing idle background tabs and memory
  targets (WebKit throttles hidden pages itself), the speaker icon and muting, noticing a
  crashed page, saving passwords, closing a tab from `window.close()`, and Ctrl+F / print /
  zoom shortcuts. A page's full screen video opens in WebKit's own full screen window.
- Favicons come from the page's `<link rel="icon">` (or `/favicon.ico`), read after each load.

## Shortcuts

| Windows | macOS | Action |
|---|---|---|
| Ctrl+T | ⌘T | Command bar: open what you type in a new tab |
| Ctrl+L, Alt+D, F6 | ⌘L | Command bar: edit the current page's address |
| Ctrl+Shift+T | ⇧⌘T | Reopen closed tab |
| Ctrl+W, Ctrl+F4 | ⌘W | Close tab (a pinned tab unloads) |
| Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PgDn / Ctrl+PgUp | ⌥⌘→ / ⌥⌘← | Next / previous tab in this space |
| Ctrl+1…8, Ctrl+9 | ⌘1…8, ⌘9 | Go to tab N / last tab in this space |
| Alt+1…9 | ⌃1…9 | Go to space N |
| Ctrl+D | ⌘D | Bookmark the page, or remove its bookmark |
| Ctrl+\ | ⌘\ | Split view with the tab used last before this one; again to separate |
| F5, Ctrl+R | ⌘R | Reload |
| Alt+Left / Alt+Right | ⌘[ / ⌘] | Back / forward |
| Ctrl+S | ⌘S | Hide / show the sidebar |
| F11 | ⌃⌘F | Full screen |
| Ctrl+F, Ctrl+P, Ctrl+plus/minus | | Find, print, zoom (built into WebView2; not on macOS yet) |
|  | ⌘Q, ⌘M, ⌘H | Quit, minimize, hide (macOS menus) |

In the command bar, the arrow keys pick a result (open tabs, bookmarks and history that match are
listed under what you typed), Enter opens it and Escape closes the bar. Clicking the site name in the sidebar opens it too.

Right-click a tab to pin or unpin it, open it in split view beside the current tab, move it to
another space, copy its link or close it. You can also middle-click a tab to close it, drag tabs up and down to reorder them,
double-click empty sidebar space to open a new tab, and drag the sidebar's right edge to
resize it. With the sidebar hidden, touch the window's left edge to slide it out.

## Tests

```powershell
node tests/address.test.js
cargo test
node tests/ui.mjs          # sidebar and command bar in a headless Chromium, with a fake host
$env:BROWSER_ENGINE="webkit"; node tests/ui.mjs   # the same in WebKit, the macOS engine
                                                  # (npx playwright install webkit first)

# end-to-end (Windows): start the browser with a debug port and a throwaway profile, then run the suite
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
$env:BROWSER_USER_DATA="$env:TEMP\browser-test"
cargo run -- https://example.com
node tests/e2e.mjs        # in a second terminal
```

`ui.mjs` also checks the sidebar as it looks on macOS, and the macOS bridge (`ui/mac-host.js`)
driving the real sidebar and command bar pages. On a Mac, `cargo test` runs the macOS module's
tests too (menu shortcuts, serving `ui/`).

To check that the macOS code compiles from Windows (`rustup target add aarch64-apple-darwin`):
`cargo check --target aarch64-apple-darwin`. One dependency compiles a small Objective-C file
and needs a C compiler for the Mac target; for a check only, point `CC_aarch64_apple_darwin`
and `AR_aarch64_apple_darwin` at stand-ins that write empty files (nothing is linked).

`BROWSER_USER_DATA` keeps the test browser separate from a window you already have open.
Set `CDP_PORT` if you used another port, `BROWSER_PID` to the test browser's process id to
include the window-edge hover check, and `SKIP_FOCUS_STEALING=1` to skip the maximize and
fullscreen checks (they take focus and briefly cover the screen).

## Not built yet

- Settings page, downloads panel (WebView2's built-in download popup works), a list for managing
  bookmarks (for now: search for one, open it, press Ctrl+D), importing from other browsers.
- Arc extras not done: folders, favorites (tiles shared by all spaces), a separate profile
  (logins) per space, more than two tabs in a split, dragging the split's divider, tinting the
  window in the space's color.
- Anti-aliased card corners. The card is clipped with a window region, which gives aliased
  corners. Layered child windows over the corners could fix this; not yet verified.
- An app icon, waiting for a name.
- Windows 11 snap layouts when hovering the maximize button (needs native caption buttons).
- Packaging: a release build reads `ui/` from the source folder unless a `ui/` folder sits next to
  the exe (or, on macOS, in the app bundle's `Contents/Resources`), so copy it there (or embed it)
  before distributing. There's no `.app` bundle yet; `cargo run` starts the bare binary.
- macOS: the missing pieces listed under "On macOS" above, and a live test pass on a Mac (the
  macOS code is compiled and its UI tested in WebKit, but hasn't been run on a Mac yet).
