// Sidebar logic. The Rust host owns all state; it sends {type:"state"} snapshots and this
// file renders them. User actions are sent back as {cmd: ...} messages.

const host = window.chrome.webview;
const send = (cmd, extra = {}) => host.postMessage(JSON.stringify({ cmd, ...extra }));

const $ = (sel) => document.querySelector(sel);
const tabsEl = $("#tabs");
const todayEl = $("#today");
const template = $("#tab-template");
const address = $("#address");
const addressText = $("#address-text");
const siteIcon = $("#site-icon");
const backBtn = $("#back");
const forwardBtn = $("#forward");
const reloadBtn = $("#reload");
const toggleBtn = $("#toggle-sidebar");
const copyBtn = $("#copy-link");
const bookmarkBtn = $("#bookmark");
const spaceName = $("#space-name");
const spaceRename = $("#space-rename");
const spaceList = $("#space-list");
const menu = $("#menu");

// Must match SPACE_COLORS in src/browser.rs.
const SPACE_COLORS = ["#5b6cf0", "#e5484d", "#30a46c", "#f08c2e", "#8e4ec6", "#0d9fb8", "#d6409f", "#71717a"];

// On macOS the shortcuts use ⌘ (⌃ for spaces), and the window has its own traffic lights
// at the top left (body.mac hides ours). Hints are written the Windows way and translated here.
const MAC = /^Mac/.test(navigator.platform);
const MAC_KEYS = [[/Alt\+Left/g, "⌘["], [/Alt\+Right/g, "⌘]"], [/Alt\+/g, "⌃"], [/Ctrl[+ ]/g, "⌘"], [/\bF5\b/g, "⌘R"]];
const keys = (hint) => (MAC ? MAC_KEYS.reduce((text, [from, to]) => text.replace(from, to), hint) : hint);

let state = { active: null, tabs: [], space: null, spaces: [], sidebar: "docked", maximized: false, commandBar: false };
const tabEls = new Map(); // tab id -> element
let drag = null;
let rendered = false; // tabs that appear after the first render animate in
let renameNewSpace = false; // the "+" button was clicked: rename the space it makes

const ICONS = {
  lock: '<svg viewBox="0 0 16 16"><rect x="3.5" y="7" width="9" height="6.5" rx="1.75"/><path d="M5.5 7V5.25a2.5 2.5 0 0 1 5 0V7"/></svg>',
  info: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6"/><path d="M8 7.5v3.5M8 5h0"/></svg>',
  search: '<svg viewBox="0 0 16 16"><circle cx="7.25" cy="7.25" r="4.5"/><path d="M10.5 10.5L13.5 13.5"/></svg>',
};

host.addEventListener("message", (event) => {
  const msg = event.data;
  if (msg.type === "state") {
    const previousSpace = state.space;
    state = msg;
    render();
    if (renameNewSpace && state.space !== previousSpace) {
      renameNewSpace = false;
      startRename();
    }
  }
});

// ------------------------------------------------------------------ rendering

function activeTab() {
  return state.tabs.find((t) => t.id === state.active) || null;
}

function currentSpace() {
  return state.spaces.find((s) => s.id === state.space) || null;
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

/** The address shows just the site, like "example.com"; the command bar edits the full URL. */
function displayUrl(url) {
  try {
    const u = new URL(url);
    if (u.protocol === "https:" || u.protocol === "http:") return u.host.replace(/^www\./, "");
    if (u.protocol === "file:") return decodeURIComponent(u.pathname.split("/").pop() || url);
  } catch {}
  return url;
}

function render() {
  document.body.classList.toggle("peek", state.sidebar === "peek");
  document.body.classList.toggle("maximized", !!state.maximized);
  document.body.classList.toggle("has-today", state.tabs.some((t) => !t.pinned));
  $("#win-max").title = state.maximized ? "Restore" : "Maximize";
  toggleBtn.title = keys(state.sidebar === "docked" ? "Hide sidebar (Ctrl+S)" : "Keep sidebar open (Ctrl+S)");
  renderSpaces();
  renderTabs();
  renderToolbar();
}

function renderSpaces() {
  const space = currentSpace();
  document.documentElement.style.setProperty("--space", space?.color || "");
  if (space && spaceName.textContent !== space.name) spaceName.textContent = space.name;

  // Rebuild only when something changed; it's a handful of buttons.
  const key = JSON.stringify([state.space, state.spaces]);
  if (spaceList.dataset.key === key) return;
  spaceList.dataset.key = key;
  spaceList.replaceChildren(
    ...state.spaces.map((s, i) => {
      const button = document.createElement("button");
      button.className = "space-button" + (s.id === state.space ? " current" : "");
      button.title = i < 9 ? `${s.name} (${keys(`Alt+${i + 1}`)})` : s.name;
      button.setAttribute("role", "tab");
      button.setAttribute("aria-selected", s.id === state.space);
      button.setAttribute("aria-label", s.name);
      button.style.setProperty("--color", s.color);
      const dot = document.createElement("span");
      dot.className = "dot";
      button.append(dot);
      button.addEventListener("click", () => send("switchSpace", { space: s.id }));
      button.addEventListener("dblclick", () => s.id === state.space && startRename());
      button.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        openSpaceMenu(s, e.clientX, e.clientY);
      });
      return button;
    }),
  );
}

function renderTabs() {
  const seen = new Set();
  const shownPair = state.splitShown ? state.tabs.filter((t) => t.split).map((t) => t.id) : [];
  const ordered = [];
  for (const tab of state.tabs) {
    seen.add(tab.id);
    let el = tabEls.get(tab.id);
    if (!el) {
      el = createTabElement(tab.id);
      tabEls.set(tab.id, el);
      if (rendered) {
        el.classList.add("entering");
        el.addEventListener("animationend", () => el.classList.remove("entering"), { once: true });
      }
    }
    const title = tabTitle(tab);
    const titleEl = el.querySelector(".tab-title");
    if (titleEl.textContent !== title) titleEl.textContent = title;
    el.title = tab.suspended ? `${title}\n(Sleeping to save memory)` : title;
    el.classList.toggle("active", tab.id === state.active);
    el.classList.toggle("split-partner", tab.id !== state.active && shownPair.includes(tab.id));
    el.classList.toggle("in-split", !!tab.split);
    el.classList.toggle("loading", tab.loading && !tab.newTab);
    el.classList.toggle("suspended", tab.suspended);
    el.classList.toggle("pinned", !!tab.pinned);
    el.classList.toggle("unloaded", !!tab.unloaded);
    el.classList.toggle("away", !!tab.awayFromPin && !tab.unloaded);
    el.classList.toggle("audible", !!tab.audible);
    el.classList.toggle("muted", !!tab.muted);
    el.querySelector(".tab-audio").title = tab.muted ? "Unmute tab" : "Mute tab";
    el.querySelector(".tab-close").title = keys(tab.pinned ? "Unload tab (Ctrl+W)" : "Close tab (Ctrl+W)");
    el.setAttribute("aria-selected", tab.id === state.active);

    const img = el.querySelector(".favicon");
    const favicon = tab.favicon || "";
    if (img.dataset.src !== favicon) {
      img.dataset.src = favicon;
      img.classList.toggle("missing", !favicon);
      if (favicon) img.src = favicon;
      else img.removeAttribute("src");
    }
    ordered.push(el);
  }
  // Pinned tabs above the today line, the rest below it.
  const pinnedCount = state.tabs.filter((t) => t.pinned).length;
  ordered.splice(pinnedCount, 0, todayEl);

  // Keep DOM order in sync with the host's order (skip while dragging).
  if (!drag) {
    ordered.forEach((el, index) => {
      if (tabsEl.children[index] !== el) tabsEl.insertBefore(el, tabsEl.children[index] || null);
    });
  }
  for (const [id, el] of tabEls) {
    if (!seen.has(id)) {
      el.remove();
      tabEls.delete(id);
    }
  }
  tabEls.get(state.active)?.scrollIntoView({ block: "nearest" });
  rendered = true;
}

function createTabElement(id) {
  const el = template.content.firstElementChild.cloneNode(true);
  el.dataset.id = id;
  const img = el.querySelector(".favicon");
  img.addEventListener("error", () => img.classList.add("missing"));
  img.addEventListener("load", () => img.classList.remove("missing"));

  el.querySelector(".tab-audio").addEventListener("click", (e) => {
    e.stopPropagation();
    send("toggleMute", { id });
  });
  el.querySelector(".tab-home").addEventListener("click", (e) => {
    e.stopPropagation();
    send("resetPin", { id });
  });
  el.querySelector(".tab-close").addEventListener("click", (e) => {
    e.stopPropagation();
    send("closeTab", { id });
  });
  el.addEventListener("auxclick", (e) => {
    if (e.button === 1) send("closeTab", { id }); // middle click
  });
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest(".tab-close, .tab-audio, .tab-home")) return;
    send("activate", { id });
    startDrag(e, el, id);
  });
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const tab = state.tabs.find((t) => t.id === id);
    if (tab) openTabMenu(tab, e.clientX, e.clientY);
  });
  return el;
}

function renderToolbar() {
  const tab = activeTab();
  backBtn.disabled = !tab?.canBack;
  forwardBtn.disabled = !tab?.canForward;
  const loading = !!tab?.loading && !tab?.newTab;
  reloadBtn.classList.toggle("loading", loading);
  reloadBtn.title = loading ? "Stop" : keys("Reload (F5)");

  const text = tab ? displayUrl(tab.url) : "";
  if (addressText.textContent !== text) addressText.textContent = text;
  address.title = `${tab?.url || "Search or enter address"} (${keys("Ctrl+L")})`;
  copyBtn.disabled = !tab?.url;
  bookmarkBtn.disabled = !tab?.url;
  bookmarkBtn.classList.toggle("on", !!state.bookmarked);
  bookmarkBtn.title = keys(state.bookmarked ? "Remove bookmark (Ctrl+D)" : "Bookmark this page (Ctrl+D)");

  const url = tab?.url || "";
  let icon = "search";
  let cls = "";
  if (url.startsWith("https:")) icon = "lock";
  else if (url.startsWith("http:")) {
    icon = "info";
    cls = "insecure";
  } else if (url) icon = "info";
  // Only touch the DOM when the icon changes, so it never redraws for nothing.
  if (siteIcon.dataset.icon !== icon + cls) {
    siteIcon.dataset.icon = icon + cls;
    siteIcon.className = cls;
    siteIcon.innerHTML = ICONS[icon];
  }
  siteIcon.title = icon === "lock" ? "Connection is secure" : url.startsWith("http:") ? "Not secure" : "";
}

// ------------------------------------------------------------------ tab dragging

function startDrag(e, el, id) {
  drag = { el, id, startY: e.clientY, moved: false };
  el.setPointerCapture(e.pointerId);
  el.addEventListener("pointermove", onDragMove);
  el.addEventListener("pointerup", endDrag, { once: true });
  el.addEventListener("pointercancel", endDrag, { once: true });
}

function onDragMove(e) {
  if (!drag) return;
  const dy = e.clientY - drag.startY;
  if (!drag.moved && Math.abs(dy) < 5) return;
  drag.moved = true;
  drag.el.classList.add("dragging");

  // Swap with a neighbour (a tab, or the today line) once the pointer passes its midpoint.
  const siblings = [...tabsEl.children];
  const index = siblings.indexOf(drag.el);
  const next = siblings[index + 1];
  const prev = siblings[index - 1];
  const step = (other) => other.offsetHeight + 2; // + row gap
  if (next && e.clientY > next.getBoundingClientRect().top + next.offsetHeight / 2) {
    tabsEl.insertBefore(next, drag.el);
    drag.startY += step(next);
  } else if (prev && e.clientY < prev.getBoundingClientRect().top + prev.offsetHeight / 2) {
    tabsEl.insertBefore(drag.el, prev);
    drag.startY -= step(prev);
  }
  drag.el.style.transform = `translateY(${e.clientY - drag.startY}px)`;
}

function endDrag() {
  if (!drag) return;
  const { el, id, moved } = drag;
  el.removeEventListener("pointermove", onDragMove);
  el.classList.remove("dragging");
  el.style.transform = "";
  drag = null;
  if (!moved) return;
  // Above the today line is pinned; positions count within each section.
  const children = [...tabsEl.children];
  const index = children.indexOf(el);
  const line = children.indexOf(todayEl);
  const pinned = index < line;
  send("moveTab", { id, to: pinned ? index : index - line - 1, pinned });
}

// ------------------------------------------------------------------ menus

/**
 * Show a menu at (x, y). Items: {label, run, disabled?, danger?}, "-" for a line, or
 * {swatches: [colors], selected, pick(color)}.
 */
function openMenu(x, y, items) {
  menu.replaceChildren(
    ...items.map((item) => {
      if (item === "-") {
        const sep = document.createElement("div");
        sep.className = "menu-sep";
        return sep;
      }
      if (item.swatches) {
        const row = document.createElement("div");
        row.className = "menu-swatches";
        for (const color of item.swatches) {
          const swatch = document.createElement("button");
          swatch.className = "swatch" + (color === item.selected ? " selected" : "");
          swatch.style.setProperty("--color", color);
          swatch.setAttribute("aria-label", color);
          swatch.addEventListener("click", () => {
            closeMenu();
            item.pick(color);
          });
          row.append(swatch);
        }
        return row;
      }
      const button = document.createElement("button");
      button.className = "menu-item" + (item.danger ? " danger" : "");
      button.setAttribute("role", "menuitem");
      button.textContent = item.label;
      button.disabled = !!item.disabled;
      button.addEventListener("click", () => {
        closeMenu();
        item.run();
      });
      return button;
    }),
  );
  menu.hidden = false;
  // Keep it inside the sidebar.
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(6, Math.min(x, innerWidth - r.width - 6))}px`;
  menu.style.top = `${Math.max(6, Math.min(y, innerHeight - r.height - 6))}px`;
}

function closeMenu() {
  menu.hidden = true;
}

document.addEventListener("pointerdown", (e) => {
  if (!menu.hidden && !menu.contains(e.target)) closeMenu();
}, true);
window.addEventListener("blur", closeMenu);
$("#tab-list").addEventListener("scroll", closeMenu);

function openTabMenu(tab, x, y) {
  const items = [];
  items.push(tab.pinned
    ? { label: "Unpin", run: () => send("unpin", { id: tab.id }) }
    : { label: "Pin", run: () => send("pin", { id: tab.id }), disabled: tab.newTab });
  if (tab.pinned && tab.awayFromPin && !tab.unloaded) {
    items.push({ label: "Back to the pinned page", run: () => send("resetPin", { id: tab.id }) });
    items.push({ label: "Pin this page instead", run: () => send("setPinHere", { id: tab.id }) });
  }
  items.push("-");
  if (tab.split) items.push({ label: "Separate split view", run: () => send("separate") });
  else if (tab.id !== state.active) items.push({ label: "Open in split view", run: () => send("splitWith", { id: tab.id }) });
  else items.push({ label: keys("Split view (Ctrl+\\)"), run: () => send("toggleSplit") });
  for (const space of state.spaces) {
    if (space.id !== state.space) items.push({ label: `Move to ${space.name}`, run: () => send("moveToSpace", { id: tab.id, space: space.id }) });
  }
  if (tab.url) items.push({ label: "Copy link", run: () => navigator.clipboard.writeText(tab.url).catch(() => {}) });
  items.push("-");
  if (tab.pinned) {
    if (!tab.unloaded) items.push({ label: "Unload", run: () => send("closeTab", { id: tab.id }) });
    items.push({ label: "Remove pinned tab", danger: true, run: () => {
      send("unpin", { id: tab.id });
      send("closeTab", { id: tab.id });
    } });
  } else {
    items.push({ label: "Close tab", run: () => send("closeTab", { id: tab.id }) });
  }
  openMenu(x, y, items.filter((item, i, all) => item !== "-" || (i > 0 && all[i - 1] !== "-")));
}

function openSpaceMenu(space, x, y) {
  openMenu(x, y, [
    { label: "Rename", run: () => {
      if (space.id !== state.space) send("switchSpace", { space: space.id });
      renameNewSpace = space.id !== state.space;
      if (space.id === state.space) startRename();
    } },
    { swatches: SPACE_COLORS, selected: space.color, pick: (color) => send("spaceColor", { space: space.id, color }) },
    "-",
    { label: "New space", run: newSpace },
    { label: "Delete space", danger: true, disabled: state.spaces.length < 2, run: () => {
      // Nothing is lost (its tabs go to the archive), but it's a big change.
      if (confirm(`Delete the space "${space.name}"? Its tabs move to the archive.`)) send("deleteSpace", { space: space.id });
    } },
  ]);
}

function newSpace() {
  renameNewSpace = true;
  send("newSpace");
}

// ------------------------------------------------------------------ renaming a space

function startRename() {
  const space = currentSpace();
  if (!space) return;
  spaceRename.value = space.name;
  spaceName.hidden = true;
  spaceRename.hidden = false;
  spaceRename.focus();
  spaceRename.select();
}

function endRename(save) {
  if (spaceRename.hidden) return;
  const name = spaceRename.value.trim();
  spaceRename.hidden = true;
  spaceName.hidden = false;
  if (save && name && name !== currentSpace()?.name) {
    spaceName.textContent = name;
    send("renameSpace", { space: state.space, name });
  }
}

spaceName.addEventListener("dblclick", startRename);
$("#space-header").addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const space = currentSpace();
  if (space) openSpaceMenu(space, e.clientX, e.clientY);
});
spaceRename.addEventListener("keydown", (e) => {
  if (e.key === "Enter") endRename(true);
  else if (e.key === "Escape") endRename(false);
  else return;
  e.preventDefault();
  send("focusContent");
});
spaceRename.addEventListener("blur", () => endRename(true));

// ------------------------------------------------------------------ sidebar resizing

const handle = $("#resize-handle");
handle.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  handle.setPointerCapture(e.pointerId);
  let pending = false;
  const move = (ev) => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => {
      pending = false;
      send("sidebarWidth", { width: ev.clientX });
    });
  };
  handle.addEventListener("pointermove", move);
  handle.addEventListener("pointerup", () => handle.removeEventListener("pointermove", move), { once: true });
});

// ------------------------------------------------------------------ buttons

backBtn.addEventListener("click", () => send("back"));
forwardBtn.addEventListener("click", () => send("forward"));
reloadBtn.addEventListener("click", () => send(reloadBtn.classList.contains("loading") ? "stop" : "reload"));
toggleBtn.addEventListener("click", () => send("toggleSidebar"));
$("#win-min").addEventListener("click", () => send("minimize"));
$("#win-max").addEventListener("click", () => send("maximize"));
$("#win-close").addEventListener("click", () => send("close"));
$("#new-tab").addEventListener("click", () => send("newTab"));
$("#list-fill").addEventListener("dblclick", () => send("newTab"));
$("#clear-today").addEventListener("click", () => send("clearToday"));
$("#archive").addEventListener("click", () => send("openArchive"));
$("#new-space").addEventListener("click", newSpace);
bookmarkBtn.addEventListener("click", () => send("toggleBookmark"));

// The top row is the window's title bar. WebView2 normally handles `app-region: drag`
// itself and this page never sees the click; if it does, ask the host to move the window.
$("#topbar").addEventListener("mousedown", (e) => {
  if (e.button !== 0 || e.target.closest("button")) return;
  send(e.detail === 2 ? "maximize" : "dragWindow");
});

address.addEventListener("click", () => send("openCommandBar"));

copyBtn.addEventListener("click", async () => {
  const url = activeTab()?.url;
  if (!url) return;
  try {
    await navigator.clipboard.writeText(url);
  } catch {
    return;
  }
  copyBtn.classList.add("done");
  copyBtn.title = "Copied";
  clearTimeout(copyBtn.timer);
  copyBtn.timer = setTimeout(() => {
    copyBtn.classList.remove("done");
    copyBtn.title = "Copy link";
  }, 1200);
});

document.addEventListener("keydown", (e) => {
  // Ctrl+Shift+F5 reloads this UI, for quick iteration on ui/ files.
  if (e.key === "F5" && e.ctrlKey && e.shiftKey) {
    e.preventDefault();
    send("reloadUi");
  } else if (e.key === "Escape" && !menu.hidden) {
    closeMenu();
  }
});
document.addEventListener("contextmenu", (e) => {
  if (e.target !== spaceRename) e.preventDefault();
});

if (MAC) {
  document.body.classList.add("mac");
  // The hints written into the page (and the tab template) at load.
  for (const root of [document, template.content]) {
    for (const el of root.querySelectorAll("[title]")) el.title = keys(el.title);
    for (const el of root.querySelectorAll("kbd")) el.textContent = keys(el.textContent);
  }
}

send("ready");
