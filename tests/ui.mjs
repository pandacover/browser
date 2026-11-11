// Checks the sidebar and command bar pages in a headless Chromium (no window), with a fake
// host: `chrome.webview` is replaced by a stub that records what the page sends and lets the
// test deliver the host's messages. Covers rendering, menus, dragging and the command bar's
// query protocol; the Rust side is covered by `cargo test` and the live browser by e2e.mjs.
//
//   node tests/ui.mjs
//
// Needs playwright-core and its headless shell (`npx playwright install chromium-headless-shell`).
// Set PLAYWRIGHT_CORE to the module folder and CHROME_PATH to the executable if they aren't found.

import { createRequire } from "node:module";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
const require = createRequire(import.meta.url);

function loadPlaywright() {
  const npm = join(process.env.APPDATA || "", "npm", "node_modules");
  const candidates = [process.env.PLAYWRIGHT_CORE, "playwright-core", join(npm, "playwright-core"),
    join(npm, "playwright", "node_modules", "playwright-core"), join(npm, "@playwright", "mcp", "node_modules", "playwright-core")];
  for (const c of candidates.filter(Boolean)) {
    try {
      return require(c);
    } catch {}
  }
  throw new Error("playwright-core not found; set PLAYWRIGHT_CORE");
}

/** The newest headless shell Playwright has downloaded. */
function headlessShell() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  const root = join(process.env.LOCALAPPDATA || "", "ms-playwright");
  const dirs = existsSync(root) ? readdirSync(root).filter((d) => d.startsWith("chromium_headless_shell-")) : [];
  dirs.sort((a, b) => Number(b.split("-")[1]) - Number(a.split("-")[1]));
  for (const d of dirs) {
    const exe = join(root, d, "chrome-headless-shell-win64", "chrome-headless-shell.exe");
    if (existsSync(exe)) return exe;
  }
  return undefined; // let Playwright look for its own
}

// BROWSER_ENGINE=webkit runs the same checks in WebKit, the engine the macOS build uses
// (`npx playwright install webkit` first).
const { chromium, webkit } = loadPlaywright();
const WEBKIT = process.env.BROWSER_ENGINE === "webkit";
const UI = new URL("../ui/", import.meta.url).href;
let passed = 0, failed = 0;
const check = async (name, fn) => {
  try { await fn(); passed++; console.log("ok  ", name); }
  catch (e) { failed++; console.log("FAIL", name, "-", e.message); }
};
const eq = (a, b, what = "") => {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`${what} expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
};

const fakeHost = () => {
  window.__sent = [];
  const listeners = [];
  window.chrome = { webview: {
    postMessage: (m) => window.__sent.push(JSON.parse(m)),
    addEventListener: (_, fn) => listeners.push(fn),
  } };
  window.__deliver = (data) => listeners.forEach((fn) => fn({ data }));
};

const browser = WEBKIT
  ? await webkit.launch({ headless: true })
  : await chromium.launch({ headless: true, executablePath: headlessShell() });
console.log(`engine: ${WEBKIT ? "webkit" : "chromium"} ${browser.version()}`);
const errors = [];
const newPage = async (file, width, height) => {
  const page = await browser.newPage({ viewport: { width, height } });
  page.on("pageerror", (e) => errors.push(`${file}: ${e.message}`));
  await page.addInitScript(fakeHost);
  await page.goto(UI + file);
  return page;
};
const sent = (page) => page.evaluate(() => window.__sent);
const deliver = (page, data) => page.evaluate((d) => window.__deliver(d), data);
const lastSent = async (page, cmd) => (await sent(page)).filter((m) => m.cmd === cmd).pop();

// ------------------------------------------------------------------ command bar
const bar = await newPage("command.html", 680, 520);
const kinds = () => bar.evaluate(() => items.map((i) => i.kind));
const type = (text) => bar.evaluate((t) => { query.value = t; query.dispatchEvent(new Event("input")); }, text);
const tabs = [
  { id: 1, title: "Example Domain", url: "https://example.com/", favicon: "" },
  { id: 2, title: "Example Org", url: "https://example.org/", favicon: "" },
  { id: 3, title: "Work mail", url: "https://mail.example/", favicon: "", space: "Work" },
];

await check("bar says ready", async () => eq((await sent(bar))[0].cmd, "ready"));

await check("Ctrl+L on the current page lists nothing but the address", async () => {
  await deliver(bar, { type: "open", mode: "current", text: "https://example.com/", active: 1, tabs, suggestions: [] });
  eq(await kinds(), ["go"]);
  eq((await sent(bar)).filter((m) => m.cmd === "query").length, 0, "no query on open");
  await bar.waitForFunction(() => window.__sent.some((m) => m.cmd === "present"));
});

await check("typing lists the go item and matching tabs, and asks the host (e2e expects go,tab)", async () => {
  await type("example.org");
  eq(await kinds(), ["go", "tab"]);
  const q = await lastSent(bar, "query");
  eq(q.text, "example.org");
});

await check("host suggestions are added; ones already open as tabs are dropped", async () => {
  const q = await lastSent(bar, "query");
  await deliver(bar, { type: "suggestions", seq: q.seq, items: [
    { kind: "history", url: "https://example.org/", title: "Example Org", favicon: "", at: Date.now() / 1000 },
    { kind: "bookmark", url: "https://example.org/docs", title: "Org docs", favicon: "", at: 0 },
    { kind: "history", url: "https://other.example/", title: "Unrelated", favicon: "", at: 0 },
  ] });
  eq(await kinds(), ["go", "tab", "bookmark"]);
});

await check("an answer to an older query is ignored", async () => {
  const q = await lastSent(bar, "query");
  await deliver(bar, { type: "suggestions", seq: q.seq - 1, items: [] });
  eq(await kinds(), ["go", "tab", "bookmark"]);
});

await check("stale suggestions are filtered by the new text right away", async () => {
  await type("example.org docs");
  eq(await kinds(), ["go", "bookmark"]);
});

await check("picking a history/bookmark row navigates (submit)", async () => {
  await bar.evaluate(() => { query.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" })); document.querySelector("#form").requestSubmit(); });
  const s = await lastSent(bar, "submit");
  eq([s.url, s.newTab], ["https://example.org/docs", false]);
  await deliver(bar, { type: "closed" });
});

await check("tabs in other spaces show the space name", async () => {
  await deliver(bar, { type: "open", mode: "newTab", text: "", active: 1, tabs, suggestions: [] });
  const detail = await bar.evaluate(() => items.find((i) => i.id === 3).detail);
  eq(detail, "mail.example · Work");
  await deliver(bar, { type: "closed" });
});

await check("archive mode lists archived tabs, filters, and restores", async () => {
  const now = Date.now() / 1000;
  await deliver(bar, { type: "open", mode: "archive", text: "", active: 1, tabs, suggestions: [
    { kind: "archived", url: "https://a.example/", title: "Alpha", favicon: "", at: now - 7200 },
    { kind: "archived", url: "https://b.example/", title: "Beta", favicon: "", at: now - 60 },
  ] });
  eq(await kinds(), ["archived", "archived"]);
  eq(await bar.evaluate(() => query.placeholder), "Search archived tabs");
  eq(await bar.evaluate(() => items[0].hint), "Archived 2 h ago");
  await type("beta");
  eq(await kinds(), ["archived"]);
  eq((await lastSent(bar, "query")).text, "beta");
  await bar.evaluate(() => document.querySelector("#form").requestSubmit());
  eq((await lastSent(bar, "restore")).url, "https://b.example/");
  await deliver(bar, { type: "closed" });
});

await check("an empty archive says so", async () => {
  await deliver(bar, { type: "open", mode: "archive", text: "", active: 1, tabs, suggestions: [] });
  eq(await bar.evaluate(() => document.querySelectorAll("#results .empty").length), 1);
  await deliver(bar, { type: "closed" });
});

// ------------------------------------------------------------------ sidebar
const ui = await newPage("chrome.html", 252, 700);
const tab = (id, extra = {}) => ({ id, title: `Tab ${id}`, url: `https://t${id}.example/`, favicon: "", loading: false,
  canBack: false, canForward: false, suspended: false, crashed: false, audible: false, muted: false, newTab: false,
  pinned: false, unloaded: false, awayFromPin: false, split: null, ...extra });
const baseState = {
  type: "state", active: 3, space: 1, bookmarked: true, splitShown: true,
  spaces: [{ id: 1, name: "Home", color: "#5b6cf0" }, { id: 2, name: "Work", color: "#e5484d" }],
  tabs: [tab(1, { pinned: true, unloaded: true }), tab(2, { pinned: true, awayFromPin: true }),
         tab(3, { split: "left" }), tab(4, { split: "right" }), tab(5)],
  sidebar: "docked", maximized: false, commandBar: false, fullscreen: false,
};
const order = () => ui.evaluate(() => [...document.querySelector("#tabs").children].map((e) => e.dataset.id || e.id));

await check("sidebar says ready", async () => eq((await sent(ui))[0].cmd, "ready"));

await check("pinned tabs sit above the today line, today tabs below", async () => {
  await deliver(ui, baseState);
  eq(await order(), ["1", "2", "today", "3", "4", "5"]);
});

await check("tab classes: unloaded, away from pin, split partner, active", async () => {
  const cls = (id) => ui.evaluate((id) => [...document.querySelector(`.tab[data-id="${id}"]`).classList], id);
  const c1 = await cls(1), c2 = await cls(2), c3 = await cls(3), c4 = await cls(4);
  if (!c1.includes("unloaded") || !c1.includes("pinned")) throw new Error("tab 1: " + c1);
  if (!c2.includes("away")) throw new Error("tab 2: " + c2);
  if (!c3.includes("active") || !c3.includes("in-split")) throw new Error("tab 3: " + c3);
  if (!c4.includes("split-partner") || c4.includes("active")) throw new Error("tab 4: " + c4);
});

await check("space header, color and switcher", async () => {
  eq(await ui.evaluate(() => document.querySelector("#space-name").textContent), "Home");
  eq(await ui.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--space").trim()), "#5b6cf0");
  eq(await ui.evaluate(() => [...document.querySelectorAll(".space-button")].map((b) => b.classList.contains("current"))), [true, false]);
  await ui.click(".space-button:nth-child(2)");
  eq((await lastSent(ui, "switchSpace")).space, 2);
});

await check("bookmark star reflects state and toggles", async () => {
  eq(await ui.evaluate(() => document.querySelector("#bookmark").classList.contains("on")), true);
  await ui.evaluate(() => document.querySelector("#bookmark").click());
  if (!(await lastSent(ui, "toggleBookmark"))) throw new Error("not sent");
});

await check("tab menu: pin a today tab, move it to another space, split with it", async () => {
  await ui.click('.tab[data-id="5"]', { button: "right" });
  const labels = await ui.evaluate(() => [...document.querySelectorAll("#menu .menu-item")].map((b) => b.textContent));
  for (const l of ["Pin", "Open in split view", "Move to Work", "Copy link", "Close tab"]) {
    if (!labels.includes(l)) throw new Error(`missing "${l}" in ${JSON.stringify(labels)}`);
  }
  const box = await ui.evaluate(() => { const r = document.querySelector("#menu").getBoundingClientRect(); return [r.left, r.right, r.bottom]; });
  if (box[0] < 0 || box[1] > 252 || box[2] > 700) throw new Error("menu outside the sidebar: " + box);
  await ui.evaluate(() => [...document.querySelectorAll("#menu .menu-item")].find((b) => b.textContent === "Pin").click());
  eq((await lastSent(ui, "pin")).id, 5);
  eq(await ui.evaluate(() => document.querySelector("#menu").hidden), true, "menu closes");
  await ui.click('.tab[data-id="5"]', { button: "right" });
  await ui.evaluate(() => [...document.querySelectorAll("#menu .menu-item")].find((b) => b.textContent === "Open in split view").click());
  eq((await lastSent(ui, "splitWith")).id, 5);
});

await check("pinned tab menu offers going back to its page and removing it", async () => {
  await ui.click('.tab[data-id="2"]', { button: "right" });
  const labels = await ui.evaluate(() => [...document.querySelectorAll("#menu .menu-item")].map((b) => b.textContent));
  for (const l of ["Unpin", "Back to the pinned page", "Pin this page instead", "Unload", "Remove pinned tab"]) {
    if (!labels.includes(l)) throw new Error(`missing "${l}" in ${JSON.stringify(labels)}`);
  }
  await ui.keyboard.press("Escape");
  eq(await ui.evaluate(() => document.querySelector("#menu").hidden), true, "Escape closes it");
});

await check("dragging a today tab above the line pins it", async () => {
  await deliver(ui, { ...baseState, tabs: [tab(1, { pinned: true }), tab(3), tab(5)], splitShown: false });
  eq(await order(), ["1", "today", "3", "5"]);
  const r = await ui.evaluate(() => [...document.querySelectorAll(".tab")].map((e) => { const b = e.getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; }));
  const [x, y] = r[2]; // tab 5
  await ui.mouse.move(x, y);
  await ui.mouse.down();
  for (let i = 1; i <= 15; i++) await ui.mouse.move(x, y + ((r[0][1] + 4 - y) * i) / 15);
  await ui.mouse.up();
  const m = await lastSent(ui, "moveTab");
  eq([m.id, m.pinned, m.to], [5, true, 1]);
});

await check("dragging within today keeps it unpinned (the e2e drag)", async () => {
  await deliver(ui, { ...baseState, tabs: [tab(3), tab(4), tab(5)], splitShown: false, active: 3 });
  const r = await ui.evaluate(() => [...document.querySelectorAll(".tab")].map((e) => { const b = e.getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; }));
  const [x, y] = r[2];
  const targetY = r[0][1] - 5;
  await ui.mouse.move(x, y);
  await ui.mouse.down();
  for (let i = 1; i <= 10; i++) await ui.mouse.move(x, y + ((targetY - y) * i) / 10);
  await ui.mouse.up();
  const m = await lastSent(ui, "moveTab");
  eq([m.id, m.pinned, m.to], [5, false, 0]);
});

await check("new space: the + button asks for one, then its name is edited", async () => {
  await ui.click("#new-space");
  if (!(await lastSent(ui, "newSpace"))) throw new Error("not sent");
  await deliver(ui, { ...baseState, space: 3, spaces: [...baseState.spaces, { id: 3, name: "Space 3", color: "#30a46c" }] });
  eq(await ui.evaluate(() => !document.querySelector("#space-rename").hidden && document.activeElement.id), "space-rename");
  await ui.keyboard.type("Reading");
  await ui.keyboard.press("Enter");
  const m = await lastSent(ui, "renameSpace");
  eq([m.space, m.name], [3, "Reading"]);
  eq(await ui.evaluate(() => document.querySelector("#space-name").textContent), "Reading");
});

await check("space menu: colors and delete (with confirm)", async () => {
  ui.once("dialog", (d) => d.accept());
  await ui.click(".space-button:nth-child(1)", { button: "right" });
  eq(await ui.evaluate(() => document.querySelectorAll("#menu .swatch").length), 8);
  await ui.click("#menu .swatch:nth-child(2)");
  eq((await lastSent(ui, "spaceColor")).color, "#e5484d");
  await ui.click(".space-button:nth-child(1)", { button: "right" });
  await ui.evaluate(() => [...document.querySelectorAll("#menu .menu-item")].find((b) => b.textContent === "Delete space").click());
  eq((await lastSent(ui, "deleteSpace")).space, 1);
});

await check("Clear and Archive buttons", async () => {
  await ui.evaluate(() => { document.querySelector("#clear-today").click(); document.querySelector("#archive").click(); });
  if (!(await lastSent(ui, "clearToday")) || !(await lastSent(ui, "openArchive"))) throw new Error("not sent");
});

// ------------------------------------------------------------------ platforms
await check("Windows: our own window buttons and Ctrl shortcut hints", async () => {
  eq(await ui.evaluate(() => document.body.classList.contains("mac")), false);
  eq(await ui.evaluate(() => getComputedStyle(document.querySelector("#window-controls")).display), "flex");
  eq(await ui.evaluate(() => document.querySelector("#back").title), "Back (Alt+Left)");
  eq(await ui.evaluate(() => document.querySelector("#new-tab kbd").textContent), "Ctrl T");
});

await check("macOS: the traffic lights instead of our window buttons, ⌘ shortcut hints", async () => {
  const mac = await browser.newPage({ viewport: { width: 252, height: 700 } });
  mac.on("pageerror", (e) => errors.push(`chrome.html (macOS): ${e.message}`));
  await mac.addInitScript(() => Object.defineProperty(Navigator.prototype, "platform", { get: () => "MacIntel" }));
  await mac.addInitScript(fakeHost);
  await mac.goto(UI + "chrome.html");
  await mac.evaluate((d) => window.__deliver(d), baseState);
  const get = (fn, arg) => mac.evaluate(fn, arg);
  eq(await get(() => document.body.classList.contains("mac")), true);
  eq(await get(() => getComputedStyle(document.querySelector("#window-controls")).display), "none");
  eq(await get(() => getComputedStyle(document.querySelector("#topbar")).paddingLeft), "64px");
  eq(await get(() => document.querySelector("#back").title), "Back (⌘[)");
  eq(await get(() => document.querySelector("#forward").title), "Forward (⌘])");
  eq(await get(() => document.querySelector("#new-tab kbd").textContent), "⌘T");
  eq(await get(() => document.querySelector("#reload").title), "Reload (⌘R)");
  eq(await get(() => document.querySelector("#toggle-sidebar").title), "Hide sidebar (⌘S)");
  eq(await get(() => document.querySelector(".space-button").title), "Home (⌃1)");
  eq(await get(() => document.querySelector('.tab[data-id="5"] .tab-close').title), "Close tab (⌘W)");
  eq(await get(() => document.querySelector('.tab[data-id="3"] .tab-split').title), "In split view (⌘\\)");
  // Only the hints change, never a page's address.
  eq(await get(() => document.querySelector("#address").title), "https://t3.example/ (⌘L)");
  eq((await get(() => window.__sent))[0].cmd, "ready");
  await mac.close();
});

// The real macOS bridge (ui/mac-host.js) over a stand-in for wry's `window.ipc`.
const macHost = readFileSync(new URL("../ui/mac-host.js", import.meta.url), "utf8");
const macHostPage = async (file, width, height) => {
  const page = await browser.newPage({ viewport: { width, height } });
  page.on("pageerror", (e) => errors.push(`${file} (mac host): ${e.message}`));
  await page.addInitScript(() => {
    window.__ipc = [];
    window.ipc = { postMessage: (m) => window.__ipc.push(m) };
  });
  await page.addInitScript({ content: macHost });
  await page.goto(UI + file);
  return page;
};
// What the host does in View::post_json.
const receive = (page, data) => page.evaluate((json) => eval(`window.__browserReceive(${json})`), JSON.stringify(data));
const ipc = (page) => page.evaluate(() => window.__ipc.map((m) => JSON.parse(m)));

await check("mac host bridge: the sidebar talks to the host through window.ipc", async () => {
  const page = await macHostPage("chrome.html", 252, 700);
  eq((await ipc(page))[0], { cmd: "ready" });
  await receive(page, baseState);
  eq(await page.evaluate(() => [...document.querySelector("#tabs").children].map((e) => e.dataset.id || e.id)),
    ["1", "2", "today", "3", "4", "5"]);
  await page.click('.tab[data-id="5"]');
  eq((await ipc(page)).filter((m) => m.cmd === "activate").pop(), { cmd: "activate", id: 5 });
  await page.close();
});

await check("mac host bridge: the command bar opens, presents and answers", async () => {
  const page = await macHostPage("command.html", 680, 520);
  eq((await ipc(page))[0], { cmd: "ready" });
  await receive(page, { type: "open", mode: "newTab", text: "", active: null, tabs, suggestions: [] });
  await page.waitForFunction(() => window.__ipc.some((m) => JSON.parse(m).cmd === "present"), null, { timeout: 2000 });
  await page.keyboard.type("example.com");
  await page.keyboard.press("Enter");
  const submit = (await ipc(page)).filter((m) => m.cmd === "submit").pop();
  eq([submit.url, submit.newTab], ["https://example.com", true]);
  await page.close();
});

await check("no script errors", async () => eq(errors, []));
await browser.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
