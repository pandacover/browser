// macOS only: gives the UI pages the `chrome.webview` object WebView2 provides on Windows,
// on top of wry's `window.ipc`. Pages call `postMessage(string)` to reach the host, and get
// `message` events whose `data` is what the host sent (it calls `__browserReceive(json)`).
// Built into the app (`include_str!` in src/mac/mod.rs) and run before the page's own scripts.
(() => {
  const listeners = [];
  window.chrome = window.chrome || {};
  window.chrome.webview = {
    postMessage: (m) => window.ipc.postMessage(typeof m === "string" ? m : JSON.stringify(m)),
    addEventListener: (type, fn) => {
      if (type === "message") listeners.push(fn);
    },
    removeEventListener: (type, fn) => {
      const i = listeners.indexOf(fn);
      if (i >= 0) listeners.splice(i, 1);
    },
  };
  window.__browserReceive = (data) => {
    for (const fn of listeners.slice()) fn({ data });
  };
})();
