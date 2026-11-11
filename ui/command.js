// Command bar: type an address or a search, jump to an open tab, a bookmark or a page from
// history, or (in archive mode) reopen an archived tab.
// The host sends {type:"open", mode, text, tabs, active, suggestions} and later {type:"closed"}.
// While you type we ask it for matching bookmarks and history ({cmd:"query", text, seq}) and it
// answers {type:"suggestions", seq, items}.
// We answer "present" once the opened state is on screen (the host reveals the window then),
// and submit {url, newTab} | switchTab {id} | restore {url} | close. "height" keeps the window
// fitted to us.

const host = window.chrome.webview;
const send = (cmd, extra = {}) => host.postMessage(JSON.stringify({ cmd, ...extra }));

const $ = (sel) => document.querySelector(sel);
const box = $("#box");
const query = $("#query");
const results = $("#results");

const MAX_ITEMS = 10; // what fits in the bar's tallest window
const MAX_TAB_RESULTS = 7;
const MAX_TAB_RESULTS_WITH_PAGES = 4;
const ICONS = {
  search: '<svg viewBox="0 0 16 16"><circle cx="7.25" cy="7.25" r="4.5"/><path d="M10.5 10.5L13.5 13.5"/></svg>',
  globe: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6.2"/><path d="M1.8 8h12.4M8 1.8c-2.2 2.5-2.2 9.9 0 12.4M8 1.8c2.2 2.5 2.2 9.9 0 12.4"/></svg>',
  tab: '<svg viewBox="0 0 16 16"><rect x="2" y="3" width="12" height="10" rx="2.25"/><path d="M2 6.25h12"/></svg>',
  archive: '<svg viewBox="0 0 16 16"><rect x="2" y="3" width="12" height="3.25" rx="1"/><path d="M3 6.25v5.5a1.25 1.25 0 0 0 1.25 1.25h7.5A1.25 1.25 0 0 0 13 11.75v-5.5M6.5 8.75h3"/></svg>',
};
const PLACEHOLDERS = { archive: "Search archived tabs" };

let open = false;
let mode = "current"; // "newTab": results open in a new tab; "current": in the active tab; "archive"
let tabs = [];
let activeId = null;
let initialText = "";
let suggestions = []; // from the host: bookmarks and history, or archived tabs
let seq = 0; // the latest query sent; older answers are ignored
let items = [];
let selected = 0;

host.addEventListener("message", (event) => {
  const msg = event.data;
  if (msg.type === "open") {
    open = true;
    mode = msg.mode;
    tabs = msg.tabs || [];
    activeId = msg.active;
    initialText = msg.text || "";
    suggestions = msg.suggestions || [];
    seq++;
    query.value = initialText;
    query.placeholder = PLACEHOLDERS[mode] || "Search or enter address";
    update(true); // the host sent the suggestions for this text along with it
    query.focus();
    query.select();
    // Two frames: the first draws the new content, the second runs once it's on screen.
    requestAnimationFrame(() => requestAnimationFrame(() => {
      if (open) send("present", { height: box.offsetHeight });
    }));
  } else if (msg.type === "suggestions") {
    if (!open || msg.seq !== seq) return;
    suggestions = msg.items || [];
    update(true);
  } else if (msg.type === "closed") {
    // Hidden by now: reset for next time.
    open = false;
    query.value = "";
    items = [];
    suggestions = [];
    render();
  }
});

// ------------------------------------------------------------------ results

function host_(url) {
  try {
    return new URL(url).host.replace(/^www\./, "");
  } catch {
    return url;
  }
}

function ago(secs) {
  const d = Date.now() / 1000 - secs;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  return `${Math.floor(d / 86400)} d ago`;
}

/** Every word of what was typed appears in the title or address. */
function matches(words, ...fields) {
  const hay = fields.join(" ").toLowerCase();
  return words.every((w) => hay.includes(w));
}

/** `fromHost`: new suggestions arrived; keep the selected row if it's still there. */
function update(fromHost = false) {
  const text = query.value.trim();
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  const keep = fromHost ? items[selected]?.key : null;
  items = [];

  if (mode === "archive") {
    for (const s of suggestions.filter((s) => matches(words, s.title, s.url)).slice(0, MAX_ITEMS)) {
      items.push({
        kind: "archived",
        key: `a:${s.url}`,
        url: s.url,
        label: s.title || host_(s.url),
        detail: host_(s.url),
        hint: `Archived ${ago(s.at)}`,
        favicon: s.favicon,
        icon: ICONS.archive,
      });
    }
  } else {
    let goUrl = null;
    if (text) {
      goUrl = addressToUrl(text);
      const isSearch = goUrl.startsWith(SEARCH_URL);
      items.push({
        kind: "go",
        key: "go",
        url: goUrl,
        label: isSearch ? text : goUrl,
        detail: isSearch ? "Google Search" : "",
        hint: isSearch ? "Search" : mode === "newTab" ? "Open in new tab" : "Open",
        icon: isSearch ? ICONS.search : ICONS.globe,
      });
    }
    // Skip the lists while the box still holds the current page's address, since
    // everything would "match" it.
    if (text !== initialText.trim() || !text) {
      const openUrls = new Set(tabs.map((t) => t.url));
      // Answers to an older query still match as long as every word does.
      const pages = text
        ? suggestions.filter((s) => !openUrls.has(s.url) && s.url !== goUrl && matches(words, s.title, s.url))
        : [];
      const tabLimit = pages.length ? MAX_TAB_RESULTS_WITH_PAGES : MAX_TAB_RESULTS;
      const tabMatches = tabs.filter((t) => t.id !== activeId && matches(words, t.title, t.url));
      for (const t of tabMatches.slice(0, tabLimit)) {
        items.push({
          kind: "tab",
          key: `t:${t.id}`,
          id: t.id,
          label: t.title || host_(t.url),
          detail: t.space ? `${host_(t.url)} · ${t.space}` : host_(t.url),
          hint: "Switch to tab",
          favicon: t.favicon,
        });
      }
      for (const s of pages.slice(0, MAX_ITEMS - items.length)) {
        items.push({
          kind: s.kind,
          key: `p:${s.url}`,
          url: s.url,
          label: s.title || host_(s.url),
          detail: host_(s.url),
          hint: s.kind === "bookmark" ? "Bookmark" : ago(s.at),
          favicon: s.favicon,
          icon: ICONS.globe,
        });
      }
    }
  }
  // Ask the host for what matches (in the archive, an empty box lists the newest).
  if (!fromHost && open && (text || mode === "archive")) {
    seq++;
    send("query", { text, seq });
  }
  const kept = keep ? items.findIndex((i) => i.key === keep) : -1;
  selected = kept >= 0 ? kept : 0;
  render();
}

function render() {
  results.replaceChildren(
    ...items.map((item, i) => {
      const li = document.createElement("li");
      li.className = "result" + (i === selected ? " selected" : "");
      li.setAttribute("role", "option");
      const icon = document.createElement("span");
      icon.className = "icon";
      if (item.favicon) {
        const img = document.createElement("img");
        img.src = item.favicon;
        img.onerror = () => (icon.innerHTML = item.icon || ICONS.tab);
        icon.append(img);
      } else {
        icon.innerHTML = item.icon || ICONS.tab;
      }
      const text = document.createElement("span");
      text.className = "text";
      const label = document.createElement("span");
      label.className = "label";
      label.textContent = item.label;
      text.append(label);
      if (item.detail && item.detail !== item.label) {
        const detail = document.createElement("span");
        detail.className = "detail";
        detail.textContent = item.detail;
        text.append(detail);
      }
      const hint = document.createElement("span");
      hint.className = "hint";
      hint.textContent = item.hint;
      const kbd = document.createElement("kbd");
      kbd.textContent = "↵";
      hint.append(kbd);
      li.append(icon, text, hint);
      li.addEventListener("mousemove", () => select(i));
      li.addEventListener("mousedown", (e) => {
        e.preventDefault(); // keep focus in the input
        choose(item);
      });
      return li;
    }),
  );
  if (open && mode === "archive" && !items.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = query.value.trim() ? "No archived tabs match" : "Tabs you close, and today tabs left unused for 12 hours, show up here";
    results.append(li);
  }
  reportHeight();
}

function select(i) {
  if (i === selected || !items[i]) return;
  results.children[selected]?.classList.remove("selected");
  selected = i;
  results.children[selected].classList.add("selected");
  results.children[selected].scrollIntoView({ block: "nearest" });
}

let lastHeight = 0;
function reportHeight() {
  const height = box.offsetHeight;
  if (height !== lastHeight) {
    lastHeight = height;
    send("height", { height });
  }
}

// ------------------------------------------------------------------ actions

function choose(item) {
  if (!item || !open) return;
  open = false;
  if (item.kind === "tab") send("switchTab", { id: item.id });
  else if (item.kind === "archived") send("restore", { url: item.url });
  else send("submit", { url: item.url, newTab: mode === "newTab" });
}

query.addEventListener("input", () => update());
query.addEventListener("keydown", (e) => {
  if ((e.key === "ArrowDown" || e.key === "ArrowUp") && !items.length) return;
  if (e.key === "ArrowDown") select((selected + 1) % items.length);
  else if (e.key === "ArrowUp") select((selected - 1 + items.length) % items.length);
  else if (e.key === "Escape") {
    open = false;
    send("close");
  } else return;
  e.preventDefault();
});
$("#form").addEventListener("submit", (e) => {
  e.preventDefault();
  choose(items[selected]);
});
document.addEventListener("contextmenu", (e) => {
  if (e.target !== query) e.preventDefault();
});

send("ready");
