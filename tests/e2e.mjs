// End-to-end smoke test. Drives the real browser UI over the Chrome DevTools Protocol.
//
// 1. Start the browser with a debug port:
//      $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
//      cargo run -- https://example.com
// 2. node tests/e2e.mjs
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

const uiTarget = (await targets()).find((t) => t.url.startsWith("https://singularity.ui/chrome.html"));
if (!uiTarget) throw new Error("UI page not found; is the browser running with --remote-debugging-port?");
const ui = await Cdp.connect(uiTarget.webSocketDebuggerUrl);
const state = () => ui.eval("JSON.parse(JSON.stringify(state))");
const active = async () => {
  const s = await state();
  return s.tabs.find((t) => t.id === s.active);
};
const submitAddress = (text) =>
  ui.eval(`address.focus(); address.value = ${JSON.stringify(text)};
           document.querySelector('#address-form').requestSubmit(); true`);

const initial = await state();
const startCount = initial.tabs.length;

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

await check("new tab button opens a focused blank tab", async () => {
  await ui.eval("document.querySelector('#new-tab').click(); true");
  await waitFor("new tab", async () => {
    const s = await state();
    const t = s.tabs.find((x) => x.id === s.active);
    return s.tabs.length === startCount + 1 && t.newTab;
  });
  await waitFor("address focus", () => ui.eval("document.activeElement === address"));
  const tabEls = await ui.eval("document.querySelectorAll('.tab').length");
  if (tabEls !== startCount + 1) throw new Error(`${tabEls} tab elements`);
});

await check("typing a host navigates the active tab", async () => {
  await submitAddress("example.org");
  await waitFor("example.org", async () => {
    const t = await active();
    return t.url === "https://example.org/" && !t.loading && t.title ? t : null;
  });
});

await check("address bar shows the page url when not editing", async () => {
  await ui.eval("address.blur(); true");
  const v = await ui.eval("address.value");
  if (v !== "https://example.org/") throw new Error(v);
});

await check("back and forward", async () => {
  await waitFor("canBack", async () => (await active()).canBack);
  await ui.eval("document.querySelector('#back').click(); true");
  await waitFor("back to new tab page", async () => (await active()).newTab);
  await waitFor("canForward", async () => (await active()).canForward);
  await ui.eval("document.querySelector('#forward').click(); true");
  await waitFor("forward to example.org", async () => (await active()).url === "https://example.org/");
});

await check("search terms go to the search engine", async () => {
  await submitAddress("singularity browser test");
  // Decode repeatedly: after many automated searches Google may redirect to its captcha page,
  // which carries the original query double-encoded in `continue=`.
  const decoded = (url) => {
    for (let i = 0; i < 3; i++) url = decodeURIComponent(url.replace(/\+/g, " "));
    return url;
  };
  await waitFor("search url", async () => {
    const url = (await active()).url;
    return url.includes("google.") && decoded(url).includes("singularity browser test");
  });
  await ui.eval("document.querySelector('#back').click(); true");
  await waitFor("back to example.org", async () => (await active()).url === "https://example.org/");
});

await check("clicking a tab activates it", async () => {
  const s = await state();
  const first = s.tabs[0];
  await ui.eval(`document.querySelector('.tab[data-id="${first.id}"]')
    .dispatchEvent(new PointerEvent('pointerdown', {button: 0, bubbles: true, pointerId: 1})); true`);
  await ui.eval(`document.querySelector('.tab[data-id="${first.id}"]')
    .dispatchEvent(new PointerEvent('pointerup', {button: 0, bubbles: true, pointerId: 1})); true`);
  await waitFor("first tab active", async () => (await state()).active === first.id);
});

await check("window.open from a page opens a new tab next to it", async () => {
  const before = await state();
  const page = (await targets()).find((t) => t.url === "https://example.com/");
  const cdp = await Cdp.connect(page.webSocketDebuggerUrl);
  await cdp.eval("window.open('https://example.net/'); true", true);
  cdp.close();
  const after = await waitFor("popup tab", async () => {
    const s = await state();
    return s.tabs.length === before.tabs.length + 1 ? s : null;
  });
  const idx = after.tabs.findIndex((t) => t.id === after.active);
  if (idx !== 1) throw new Error(`popup inserted at ${idx}, expected 1`);
  await waitFor("example.net loads", async () => (await active()).url === "https://example.net/");
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

await check("closed webviews are gone from the process (targets)", async () => {
  const s = await state();
  const pages = (await targets()).filter((t) => !t.url.startsWith("https://singularity.ui/chrome.html"));
  if (pages.length !== s.tabs.length) throw new Error(`${pages.length} page targets for ${s.tabs.length} tabs`);
});

ui.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
