// End-to-end smoke test. Drives the real browser UI over the Chrome DevTools Protocol.
//
// 1. Start the browser with a debug port (and a throwaway profile, so it doesn't join a
//    browser window you already have open):
//      $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
//      $env:BROWSER_USER_DATA="$env:TEMP\browser-test"
//      cargo run -- https://example.com
// 2. node tests/e2e.mjs
//
// Optional: BROWSER_PID=<pid> enables the window-edge hover check;
// SKIP_FOCUS_STEALING=1 skips the maximize and fullscreen checks.
//
// Needs network access (loads example.com/.org/.net).

const PORT = process.env.CDP_PORT || 9222;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function targets() {
  const res = await fetch(`http://127.0.0.1:${PORT}/json`);
  return (await res.json()).filter((t) => t.type === "page");
}

class Cdp {
  static async connect(wsUrl) {
    const cdp = new Cdp();
    cdp.ws = new WebSocket(wsUrl);
    cdp.nextId = 1;
    cdp.pending = new Map();
    cdp.ws.onmessage = (e) => {
      const msg = JSON.parse(e.data);
      const p = cdp.pending.get(msg.id);
      if (p) {
        cdp.pending.delete(msg.id);
        msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
      }
    };
    await new Promise((res, rej) => {
      cdp.ws.onopen = res;
      cdp.ws.onerror = rej;
    });
    return cdp;
  }
  send(method, params = {}) {
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  async eval(expression, userGesture = false) {
    const r = await this.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true, userGesture });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  }
  close() {
    this.ws.close();
  }
}

let passed = 0;
let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.message}`);
  }
}

async function waitFor(what, fn, timeout = 15000) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeout) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) {
      last = e.message;
    }
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${what} (last: ${JSON.stringify(last)})`);
}

const uiTarget = (await targets()).find((t) => t.url.startsWith("https://browser.ui/chrome.html"));
const commandTarget = (await targets()).find((t) => t.url.startsWith("https://browser.ui/command.html"));
if (!uiTarget || !commandTarget) throw new Error("UI pages not found; is the browser running with --remote-debugging-port?");
const ui = await Cdp.connect(uiTarget.webSocketDebuggerUrl);
const bar = await Cdp.connect(commandTarget.webSocketDebuggerUrl);
const state = () => ui.eval("JSON.parse(JSON.stringify(state))");
const active = async () => {
  const s = await state();
  return s.tabs.find((t) => t.id === s.active);
};
// The host reports whether the command bar is on screen in the sidebar's state.
const barOpen = async () => (await bar.eval("open")) && (await state()).commandBar;
const barClosed = async () => !(await bar.eval("open")) && !(await state()).commandBar;
/** Type into the command bar and press Enter. */
const submitCommand = (text) =>
  bar.eval(`query.value = ${JSON.stringify(text)}; query.dispatchEvent(new Event('input'));
            document.querySelector('#form').requestSubmit(); true`);
/** Ctrl+L: open the command bar on the current page's address. */
const editAddress = async () => {
  await ui.eval("address.click(); true");
  await waitFor("command bar open", barOpen);
};

const initial = await state();
const startCount = initial.tabs.length;
// Window-wide checks (maximize, fullscreen) take focus from whatever you're doing.
const skipFocusStealing = !!process.env.SKIP_FOCUS_STEALING;
// The hover check posts a mouse message to this process's window.
const pid = process.env.BROWSER_PID;

await check("UI received initial state", async () => {
  if (startCount < 1 || initial.active == null) throw new Error(JSON.stringify(initial));
});

await check("first tab loads and reports title + url", async () => {
  await waitFor("example.com title", async () => {
    const t = await active();
    return t.title === "Example Domain" && t.url === "https://example.com/" && !t.loading;
  });
});

await check("window title follows active tab", async () => {
  // The UI page can't see the native title, so check what we can: tab title rendered in DOM.
  const text = await ui.eval("document.querySelector('.tab.active .tab-title').textContent");
  if (text !== "Example Domain") throw new Error(text);
});

await check("new tab button opens an empty command bar; Escape closes it", async () => {
  await ui.eval("document.querySelector('#new-tab').click(); true");
  await waitFor("command bar open", barOpen);
  const info = await bar.eval("({ mode, value: query.value, focused: document.activeElement === query })");
  if (info.mode !== "newTab" || info.value !== "" || !info.focused) throw new Error(JSON.stringify(info));
  await bar.eval("query.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'})); true");
  await waitFor("closed", barClosed);
  if ((await state()).tabs.length !== startCount) throw new Error("Escape still created a tab");
});

await check("Ctrl+L edits the current address and navigates the same tab", async () => {
  await editAddress();
  const info = await bar.eval("({ mode, value: query.value })");
  if (info.mode !== "current" || info.value !== "https://example.com/") throw new Error(JSON.stringify(info));
  await submitCommand("example.org");
  await waitFor("example.org", async () => {
    const t = await active();
    return t.url === "https://example.org/" && !t.loading && t.title ? t : null;
  });
  await waitFor("closed", barClosed);
  if ((await state()).tabs.length !== startCount) throw new Error("opened a new tab");
});

await check("sidebar shows just the site", async () => {
  const v = await ui.eval("addressText.textContent");
  if (v !== "example.org") throw new Error(v);
});

await check("back and forward", async () => {
  await waitFor("canBack", async () => (await active()).canBack);
  await ui.eval("document.querySelector('#back').click(); true");
  await waitFor("back to example.com", async () => (await active()).url === "https://example.com/");
  await waitFor("canForward", async () => (await active()).canForward);
  await ui.eval("document.querySelector('#forward').click(); true");
  await waitFor("forward to example.org", async () => (await active()).url === "https://example.org/");
});

await check("search terms go to the search engine", async () => {
  await editAddress();
  await submitCommand("browser e2e test");
  // Decode repeatedly: after many automated searches Google may redirect to its captcha page,
  // which carries the original query double-encoded in `continue=`.
  const decoded = (url) => {
    for (let i = 0; i < 3; i++) url = decodeURIComponent(url.replace(/\+/g, " "));
    return url;
  };
  await waitFor("search url", async () => {
    const url = (await active()).url;
    return url.includes("google.") && decoded(url).includes("browser e2e test");
  });
  await ui.eval("document.querySelector('#back').click(); true");
  await waitFor("back to example.org", async () => (await active()).url === "https://example.org/");
});

await check("Ctrl+T opens what you type in a new tab", async () => {
  await ui.eval("send('newTab'); true");
  await waitFor("command bar open", barOpen);
  await submitCommand("example.net");
  await waitFor("new tab with example.net", async () => {
    const s = await state();
    const t = s.tabs.find((x) => x.id === s.active);
    return s.tabs.length === startCount + 1 && t.url === "https://example.net/" && !t.loading;
  });
});

await check("command bar finds open tabs and switches to them", async () => {
  await editAddress();
  await bar.eval("query.value = 'example.org'; query.dispatchEvent(new Event('input')); true");
  const items = await bar.eval("items.map(i => i.kind)");
  if (JSON.stringify(items) !== '["go","tab"]') throw new Error(JSON.stringify(items));
  const tabCount = (await state()).tabs.length;
  await bar.eval("query.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown'})); document.querySelector('#form').requestSubmit(); true");
  await waitFor("switched", async () => (await active()).url === "https://example.org/");
  if ((await state()).tabs.length !== tabCount) throw new Error("tab count changed");
  await waitFor("closed", barClosed);
});

await check("clicking a tab activates it", async () => {
  const s = await state();
  const other = s.tabs.find((t) => t.id !== s.active);
  await ui.eval(`document.querySelector('.tab[data-id="${other.id}"]')
    .dispatchEvent(new PointerEvent('pointerdown', {button: 0, bubbles: true, pointerId: 1})); true`);
  await ui.eval(`document.querySelector('.tab[data-id="${other.id}"]')
    .dispatchEvent(new PointerEvent('pointerup', {button: 0, bubbles: true, pointerId: 1})); true`);
  await waitFor("tab active", async () => (await state()).active === other.id);
  // Back to the first tab for the next check.
  await ui.eval(`send('activate', {id: state.tabs[0].id}); true`);
  await waitFor("first tab active", async () => (await state()).active === (await state()).tabs[0].id);
});

await check("window.open from a page opens a new tab next to it", async () => {
  const before = await state();
  const page = (await targets()).find((t) => t.url === before.tabs[0].url);
  const cdp = await Cdp.connect(page.webSocketDebuggerUrl);
  await cdp.eval("window.open('https://example.com/?popup'); true", true);
  cdp.close();
  const after = await waitFor("popup tab", async () => {
    const s = await state();
    return s.tabs.length === before.tabs.length + 1 ? s : null;
  });
  const idx = after.tabs.findIndex((t) => t.id === after.active);
  if (idx !== 1) throw new Error(`popup inserted at ${idx}, expected 1`);
  await waitFor("popup loads", async () => (await active()).url === "https://example.com/?popup");
});

await check("close tab via its close button, then reopen it", async () => {
  const before = await state();
  const victim = before.tabs.find((t) => t.id === before.active);
  await ui.eval(`document.querySelector('.tab[data-id="${victim.id}"] .tab-close').click(); true`);
  await waitFor("tab closed", async () => (await state()).tabs.length === before.tabs.length - 1);
  const s = await state();
  if (s.tabs.some((t) => t.id === victim.id)) throw new Error("still present");
  if (s.active == null) throw new Error("no active tab after close");
  await ui.eval("send('reopenTab'); true");
  await waitFor("reopened", async () => {
    const t = await active();
    return (await state()).tabs.length === before.tabs.length && t.url === victim.url;
  });
});

await check("drag-reorder sends moveTab and host reorders", async () => {
  const s = await state();
  const last = s.tabs[s.tabs.length - 1];
  await ui.eval(`send('moveTab', {id: ${last.id}, to: 0}); true`);
  await waitFor("moved", async () => (await state()).tabs[0].id === last.id);
});

await check("a pinned tab sits above the line, and closing it only unloads it", async () => {
  const s = await state();
  const id = s.tabs[s.tabs.length - 1].id;
  await ui.eval(`send('pin', {id: ${id}}); true`);
  await waitFor("pinned first", async () => {
    const t = (await state()).tabs[0];
    return t.id === id && t.pinned;
  });
  const order = await ui.eval("[...document.querySelector('#tabs').children].map(e => e.dataset.id || e.id)");
  if (order[0] !== String(id) || order[1] !== "today") throw new Error(JSON.stringify(order));
  await ui.eval(`send('activate', {id: ${id}}); send('closeTab', {id: ${id}}); true`);
  await waitFor("unloaded, still listed", async () => {
    const t = (await state()).tabs.find((t) => t.id === id);
    return t && t.unloaded && (await state()).active !== id;
  });
  await ui.eval(`send('activate', {id: ${id}}); true`);
  await waitFor("loads again", async () => {
    const t = (await state()).tabs.find((t) => t.id === id);
    return !t.unloaded && !t.loading && t.title;
  });
  await ui.eval(`send('unpin', {id: ${id}}); true`);
  await waitFor("unpinned", async () => !(await state()).tabs.some((t) => t.pinned));
});

await check("Ctrl+D bookmarks the page and the star shows it", async () => {
  if ((await state()).bookmarked) throw new Error("already bookmarked");
  await ui.eval("send('toggleBookmark'); true");
  await waitFor("bookmarked", async () => (await state()).bookmarked);
  if (!(await ui.eval("bookmarkBtn.classList.contains('on')"))) throw new Error("star not shown");
  await ui.eval("send('toggleBookmark'); true");
  await waitFor("not bookmarked", async () => !(await state()).bookmarked);
});

await check("the command bar finds pages from history", async () => {
  await ui.eval("send('newTab'); true");
  await waitFor("command bar open", barOpen);
  // The search from earlier is in history but not open in any tab.
  await bar.eval("query.value = 'browser e2e test'; query.dispatchEvent(new Event('input')); true");
  await waitFor("history result", async () => (await bar.eval("items.map(i => i.kind)")).includes("history"));
  await bar.eval("query.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'})); true");
  await waitFor("closed", barClosed);
});

await check("closed tabs go to the archive and come back from it", async () => {
  const before = await state();
  const victim = before.tabs.find((t) => t.id === before.active);
  await ui.eval(`send('closeTab', {id: ${victim.id}}); true`);
  await waitFor("closed", async () => (await state()).tabs.length === before.tabs.length - 1);
  await ui.eval("document.querySelector('#archive').click(); true");
  await waitFor("archive open", barOpen);
  const info = await bar.eval("({ mode, first: items[0] && items[0].url })");
  if (info.mode !== "archive" || info.first !== victim.url) throw new Error(JSON.stringify(info));
  await bar.eval("document.querySelector('#form').requestSubmit(); true");
  await waitFor("restored", async () => {
    const t = await active();
    return (await state()).tabs.length === before.tabs.length && t.url === victim.url;
  });
});

await check("split view shows two tabs side by side, Ctrl+\\ again separates them", async () => {
  const s = await state();
  const pageOf = async (tab) => {
    const target = (await targets()).find((t) => t.url === tab.url);
    return Cdp.connect(target.webSocketDebuggerUrl);
  };
  const first = await pageOf(s.tabs.find((t) => t.id === s.active));
  const fullWidth = await first.eval("innerWidth");
  await ui.eval("send('toggleSplit'); true");
  await waitFor("split shown", async () => (await state()).splitShown);
  const pair = (await state()).tabs.filter((t) => t.split);
  if (pair.length !== 2) throw new Error(JSON.stringify(pair));
  const other = await pageOf(pair.find((t) => t.id !== s.active));
  await waitFor("both narrower", async () => (await first.eval("innerWidth")) < fullWidth * 0.6 && (await other.eval("innerWidth")) < fullWidth * 0.6);
  await ui.eval("send('separate'); true");
  await waitFor("separated", async () => !(await state()).splitShown && !(await state()).tabs.some((t) => t.split));
  await waitFor("full width again", async () => (await first.eval("innerWidth")) === fullWidth);
  first.close();
  other.close();
});

await check("spaces keep their own tabs", async () => {
  const home = await state();
  await ui.eval("send('newSpace'); true");
  await waitFor("new space", async () => {
    const s = await state();
    return s.spaces.length === home.spaces.length + 1 && s.space !== home.space && s.tabs.length === 1 && s.tabs[0].newTab;
  });
  const created = (await state()).space;
  await ui.eval(`send('renameSpace', {space: ${created}, name: 'E2E'}); true`);
  await waitFor("renamed", async () => (await state()).spaces.some((sp) => sp.name === "E2E"));
  await ui.eval(`send('switchSpace', {space: ${home.space}}); true`);
  await waitFor("back home", async () => {
    const s = await state();
    return s.space === home.space && s.active === home.active && s.tabs.length === home.tabs.length;
  });
  await ui.eval(`send('deleteSpace', {space: ${created}}); true`);
  await waitFor("deleted", async () => (await state()).spaces.length === home.spaces.length);
});

if (pid) await check("touching the left edge with the sidebar hidden slides it out, and it slides back", async () => {
  await ui.eval("send('toggleSidebar'); true");
  await waitFor("hidden", async () => (await state()).sidebar === "hidden");
  // Simulate the mouse in the window's left margin without moving the real cursor.
  const { execFileSync } = await import("node:child_process");
  execFileSync("powershell", ["-NoProfile", "-Command", `
    Add-Type 'using System; using System.Runtime.InteropServices; public class M { [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l); }';
    $h = (Get-Process -Id ${Number(pid)}).MainWindowHandle;
    [M]::PostMessage($h, 0x200, 0, (300 -shl 16) -bor 2) | Out-Null`]);
  await waitFor("peek", async () => (await state()).sidebar === "peek");
  // The real cursor isn't over the sidebar, so it slides back by itself, once.
  await waitFor("slides back", async () => (await state()).sidebar === "hidden", 4000);
  await sleep(800);
  if ((await state()).sidebar !== "hidden") throw new Error("slid out again on its own");
  await ui.eval("send('toggleSidebar'); true");
  await waitFor("docked", async () => (await state()).sidebar === "docked");
});

await check("with the sidebar hidden, Ctrl+T opens the command bar without sliding it out", async () => {
  await ui.eval("send('toggleSidebar'); true");
  await waitFor("hidden", async () => (await state()).sidebar === "hidden");
  await ui.eval("send('newTab'); true");
  await waitFor("command bar open", barOpen);
  if ((await state()).sidebar !== "hidden") throw new Error((await state()).sidebar);
  await bar.eval("query.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'})); true");
  await waitFor("closed", barClosed);
  await ui.eval("send('toggleSidebar'); true");
  await waitFor("docked again", async () => (await state()).sidebar === "docked");
});

if (!skipFocusStealing) await check("maximize and restore update the window button", async () => {
  await ui.eval("send('maximize'); true");
  await waitFor("maximized", async () => (await state()).maximized);
  if (!(await ui.eval("document.body.classList.contains('maximized')"))) throw new Error("icon not updated");
  // Regression: moving focus into a page (as closing the active tab does) used to swallow
  // the restore command.
  await ui.eval("send('focusContent'); send('maximize'); true");
  await waitFor("restored", async () => !(await state()).maximized);
});

// Briefly covers the whole screen.
if (!skipFocusStealing) await check("page fullscreen fills the screen and hides the sidebar", async () => {
  const page = (await targets()).find((t) => t.url === "https://example.org/" || t.url === "https://example.com/");
  const cdp = await Cdp.connect(page.webSocketDebuggerUrl);
  await ui.eval(`send('activate', {id: state.tabs.find(t => t.url === ${JSON.stringify(page.url)}).id}); true`);
  // Fullscreen needs a focused page (the test may run with the window in the background).
  await ui.eval("send('focusContent'); true");
  await waitFor("page focus", () => cdp.eval("document.hasFocus()"));
  await cdp.eval("document.documentElement.requestFullscreen().then(() => true)", true);
  await waitFor("full screen size", () => cdp.eval("innerWidth === screen.width && innerHeight === screen.height"));
  await waitFor("fullscreen state", async () => (await state()).fullscreen);
  await cdp.eval("document.exitFullscreen().then(() => true)");
  await waitFor("card again", () => cdp.eval("innerWidth < screen.width"));
  await waitFor("windowed state", async () => !(await state()).fullscreen);
  cdp.close();
});

await check("sidebar width can be dragged within limits", async () => {
  await ui.eval("send('sidebarWidth', {width: 1000}); true");
  await waitFor("clamped to max", () => ui.eval("innerWidth === 420"));
  await ui.eval("send('sidebarWidth', {width: 10}); true");
  await waitFor("clamped to min", () => ui.eval("innerWidth === 200"));
  await ui.eval("send('sidebarWidth', {width: 252}); true");
  await waitFor("back to default", () => ui.eval("innerWidth === 252"));
});

// Leaves the page thinking the pointer is over the tab list, so it runs after the hover tests.
await check("dragging a tab with the mouse reorders it", async () => {
  // Synthetic input goes straight to the page; the real mouse isn't touched.
  const before = (await state()).tabs.map((t) => t.id);
  const rects = await ui.eval("[...document.querySelectorAll('.tab')].map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })");
  const [x, y] = rects[rects.length - 1];
  const targetY = rects[0][1] - 5;
  const mouse = (type, y, buttons) =>
    ui.send("Input.dispatchMouseEvent", { type, x, y, button: "left", buttons, clickCount: 1 });
  await mouse("mousePressed", y, 1);
  for (let i = 1; i <= 10; i++) await mouse("mouseMoved", y + ((targetY - y) * i) / 10, 1);
  await mouse("mouseReleased", targetY, 0);
  const moved = before[before.length - 1];
  await waitFor("moved to the top", async () => (await state()).tabs[0].id === moved);
});

await check("closed webviews are gone from the process (targets)", async () => {
  const s = await state();
  const pages = (await targets()).filter((t) => !t.url.startsWith("https://browser.ui/chrome.html") && !t.url.startsWith("https://browser.ui/command.html"));
  if (pages.length !== s.tabs.length) throw new Error(`${pages.length} page targets for ${s.tabs.length} tabs`);
});

ui.close();
bar.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
