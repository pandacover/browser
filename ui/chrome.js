// Browser chrome logic. The Rust host owns all state; it sends {type:"state"} snapshots
// and this file renders them. User actions are sent back as {cmd: ...} messages.

const host = window.chrome.webview;
const send = (cmd, extra = {}) => host.postMessage(JSON.stringify({ cmd, ...extra }));

const $ = (sel) => document.querySelector(sel);
const tabsEl = $("#tabs");
const template = $("#tab-template");
const address = $("#address");
const siteIcon = $("#site-icon");
const backBtn = $("#back");
const forwardBtn = $("#forward");
const reloadBtn = $("#reload");

let state = { active: null, tabs: [] };
const tabEls = new Map(); // tab id -> element
let editingAddress = false;
let drag = null;

const ICONS = {
  lock: '<svg viewBox="0 0 16 16"><rect x="3.5" y="7" width="9" height="6.5" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/></svg>',
  info: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6"/><path d="M8 7.5v3.5M8 5h0"/></svg>',
  search: '<svg viewBox="0 0 16 16"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5L14 14"/></svg>',
};

host.addEventListener("message", (event) => {
  const msg = event.data;
  if (msg.type === "state") {
    state = msg;
    render();
  } else if (msg.type === "focusAddress") {
    address.focus();
    address.select();
  }
});

// ------------------------------------------------------------------ rendering

function activeTab() {
  return state.tabs.find((t) => t.id === state.active) || null;
}

function tabTitle(tab) {
  if (tab.newTab) return "New Tab";
  if (tab.title) return tab.title;
  try {
    return new URL(tab.url).host || tab.url;
  } catch {
    return tab.url || "Loading…";
  }
}

function render() {
  renderTabs();
  renderToolbar();
}

function renderTabs() {
  const seen = new Set();
  state.tabs.forEach((tab, index) => {
    seen.add(tab.id);
    let el = tabEls.get(tab.id);
    if (!el) {
      el = createTabElement(tab.id);
      tabEls.set(tab.id, el);
    }
    const title = tabTitle(tab);
    el.querySelector(".tab-title").textContent = title;
    el.title = tab.suspended ? `${title}\n(Sleeping to save memory)` : title;
    el.classList.toggle("active", tab.id === state.active);
    el.classList.toggle("loading", tab.loading && !tab.newTab);
    el.classList.toggle("suspended", tab.suspended);
    el.setAttribute("aria-selected", tab.id === state.active);

    const img = el.querySelector(".favicon");
    const favicon = tab.favicon || "";
    if (img.dataset.src !== favicon) {
      img.dataset.src = favicon;
      img.classList.toggle("missing", !favicon);
      if (favicon) img.src = favicon;
      else img.removeAttribute("src");
    }

    // Keep DOM order in sync with the host's order (skip while dragging).
    if (!drag && tabsEl.children[index] !== el) tabsEl.insertBefore(el, tabsEl.children[index] || null);
  });
  for (const [id, el] of tabEls) {
    if (!seen.has(id)) {
      el.remove();
      tabEls.delete(id);
    }
  }
  updateNarrowTabs();
}

function createTabElement(id) {
  const el = template.content.firstElementChild.cloneNode(true);
  el.dataset.id = id;
  const img = el.querySelector(".favicon");
  img.addEventListener("error", () => img.classList.add("missing"));
  img.addEventListener("load", () => img.classList.remove("missing"));

  el.querySelector(".tab-close").addEventListener("click", (e) => {
    e.stopPropagation();
    send("closeTab", { id });
  });
  el.addEventListener("auxclick", (e) => {
    if (e.button === 1) send("closeTab", { id }); // middle click
  });
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest(".tab-close")) return;
    send("activate", { id });
    startDrag(e, el, id);
  });
  return el;
}

function updateNarrowTabs() {
  for (const el of tabEls.values()) el.classList.toggle("narrow", el.getBoundingClientRect().width < 72);
}
new ResizeObserver(updateNarrowTabs).observe(tabsEl);

function renderToolbar() {
  const tab = activeTab();
  backBtn.disabled = !tab?.canBack;
  forwardBtn.disabled = !tab?.canForward;
  const loading = !!tab?.loading && !tab?.newTab;
  reloadBtn.classList.toggle("loading", loading);
  reloadBtn.title = loading ? "Stop" : "Reload (F5)";

  if (!editingAddress) address.value = tab ? tab.url : "";

  const url = tab?.url || "";
  let icon = "search";
  siteIcon.className = "";
  if (url.startsWith("https:")) icon = "lock";
  else if (url.startsWith("http:")) {
    icon = "info";
    siteIcon.className = "insecure";
  } else if (url) icon = "info";
  siteIcon.innerHTML = ICONS[icon];
  siteIcon.title = icon === "lock" ? "Connection is secure" : icon === "info" && url.startsWith("http:") ? "Not secure" : "";
}

// ------------------------------------------------------------------ tab dragging

function startDrag(e, el, id) {
  drag = { el, id, startX: e.clientX, offset: 0, moved: false };
  el.setPointerCapture(e.pointerId);
  el.addEventListener("pointermove", onDragMove);
  el.addEventListener("pointerup", endDrag, { once: true });
  el.addEventListener("pointercancel", endDrag, { once: true });
}

function onDragMove(e) {
  if (!drag) return;
  const dx = e.clientX - drag.startX;
  if (!drag.moved && Math.abs(dx) < 5) return;
  drag.moved = true;
  drag.el.classList.add("dragging");

  // Swap with a neighbour once the pointer passes its midpoint.
  const siblings = [...tabsEl.children];
  const index = siblings.indexOf(drag.el);
  const rect = drag.el.getBoundingClientRect();
  const next = siblings[index + 1];
  const prev = siblings[index - 1];
  if (next && e.clientX > next.getBoundingClientRect().left + next.offsetWidth / 2) {
    tabsEl.insertBefore(next, drag.el);
    drag.startX += next.offsetWidth + 2;
  } else if (prev && e.clientX < prev.getBoundingClientRect().left + prev.offsetWidth / 2) {
    tabsEl.insertBefore(drag.el, prev);
    drag.startX -= prev.offsetWidth + 2;
  }
  const clamped = Math.max(-rect.left + 6, e.clientX - drag.startX);
  drag.el.style.transform = `translateX(${clamped}px)`;
}

function endDrag() {
  if (!drag) return;
  const { el, id, moved } = drag;
  el.removeEventListener("pointermove", onDragMove);
  el.classList.remove("dragging");
  el.style.transform = "";
  drag = null;
  if (moved) send("moveTab", { id, to: [...tabsEl.children].indexOf(el) });
}

// ------------------------------------------------------------------ toolbar

backBtn.addEventListener("click", () => send("back"));
forwardBtn.addEventListener("click", () => send("forward"));
reloadBtn.addEventListener("click", () => send(reloadBtn.classList.contains("loading") ? "stop" : "reload"));
$("#new-tab").addEventListener("click", () => send("newTab"));
$("#strip-fill").addEventListener("dblclick", () => send("newTab"));

address.addEventListener("focus", () => {
  editingAddress = true;
  // Select everything on the first click, like other browsers.
  requestAnimationFrame(() => address.select());
});
address.addEventListener("blur", () => {
  editingAddress = false;
  renderToolbar();
});
$("#address-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const url = addressToUrl(address.value);
  if (!url) return;
  address.value = url;
  editingAddress = false;
  send("navigate", { url });
});
address.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    editingAddress = false;
    renderToolbar();
    send("focusContent");
  }
});

document.addEventListener("keydown", (e) => {
  // Ctrl+Shift+F5 reloads this UI, for quick iteration on ui/ files.
  if (e.key === "F5" && e.ctrlKey && e.shiftKey) {
    e.preventDefault();
    send("reloadUi");
  }
});
document.addEventListener("contextmenu", (e) => {
  if (e.target !== address) e.preventDefault();
});

send("ready");
