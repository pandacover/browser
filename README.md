# Singularity

A lightweight Windows browser: a small native Rust shell around the Microsoft Edge
WebView2 engine, with its tab strip and toolbar written in HTML/CSS.

## Run

```powershell
cargo run                                # opens a new tab
cargo run -- example.com github.com      # opens these URLs as tabs
cargo build --release                    # ~310 KB exe
```

Needs the Rust MSVC toolchain and the WebView2 runtime (ships with Windows 11).
Build output goes to `C:/Users/luvma/.cargo-target/singularity-browser` (see
`.cargo/config.toml`) so OneDrive doesn't sync build artifacts.

## Layout

| Path | What it is |
|---|---|
| `src/main.rs` | Win32 window, message loop, window procedure |
| `src/browser.rs` | Tabs, WebView2 setup and events, UI messaging, tab sleeping, fullscreen, shortcuts |
| `src/theme.rs` | Light/dark detection and title bar colors |
| `ui/chrome.html` `.css` `.js` | Tab strip + toolbar (runs in its own WebView2 at `https://singularity.ui/`) |
| `ui/address.js` | Turns address bar text into a URL or search |
| `ui/newtab.html` | New tab page |
| `tests/` | `address.test.js` (unit), `e2e.mjs` (drives the real browser) |

### How it fits together

- **One shared WebView2 environment.** All tabs share one browser process, one GPU process
  and one network process. Each tab has its own renderer process.
- **The Rust side owns all state.** WebView2 callbacks only queue an `Event`; the window procedure
  handles events one at a time (no re-entrancy). After each batch, the full tab list is sent to
  the UI as one JSON message, and the UI just renders it.
- **UI → Rust:** `chrome.webview.postMessage(JSON.stringify({cmd, ...}))`. Commands:
  `navigate, back, forward, reload, stop, newTab, closeTab, activate, moveTab, reopenTab, focusContent, reloadUi`.
- **UI edits don't need a rebuild:** `ui/` is served from the source folder in development. Press
  **Ctrl+Shift+F5** in the browser to reload the UI.
  - Two values must stay in sync between Rust and CSS: `UI_HEIGHT_DIP` (`browser.rs`) with
    `--ui-height` (`chrome.css`), and the strip color in `theme.rs` with `--strip`.

### Memory

- **Background tabs:** set to `MemoryUsageTargetLevel = Low` as soon as they're hidden.
- **Idle background tabs:** after 5 minutes they're frozen with `TrySuspend` and shown dimmed.
  Set the `SINGULARITY_SUSPEND_SECS` environment variable to change the delay. They wake
  instantly when selected.
- **Minimizing:** hides the active webview, so nothing renders.

Measured with the release build: the Rust process uses about 3 MB. The rest is WebView2 itself.

## Shortcuts

| Keys | Action |
|---|---|
| Ctrl+T / Ctrl+Shift+T | New tab / reopen closed tab |
| Ctrl+W, Ctrl+F4 | Close tab |
| Ctrl+L, Alt+D, F6 | Focus address bar |
| Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PgDn / Ctrl+PgUp | Next / previous tab |
| Ctrl+1…8, Ctrl+9 | Go to tab N / last tab |
| F5, Ctrl+R | Reload |
| Alt+Left / Alt+Right | Back / forward |

You can also middle-click a tab to close it, drag tabs to reorder them, and double-click
empty strip space to open a new tab.

## Tests

```powershell
node tests/address.test.js

# end-to-end: start the browser with a debug port, then run the suite
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
cargo run -- https://example.com
node tests/e2e.mjs        # in a second terminal
```

## Not built yet

- Tabs in the title bar (custom frame); the window currently uses the standard Windows title bar.
- Session restore, history, bookmarks, settings page, downloads panel (WebView2's built-in
  download popup works).
- Packaging: a release build reads `ui/` from the source folder unless a `ui/` folder sits next to
  the exe, so copy it there (or embed it) before distributing.
