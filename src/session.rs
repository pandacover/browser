//! Remembers spaces, open tabs, the sidebar and the window's place between runs, and where
//! the library (history, bookmarks, archive) is kept.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::library::Library;

/// Folder under %LOCALAPPDATA% (Windows) or ~/Library/Application Support (macOS) that holds
/// the profile.
const APP_DIR: &str = "Browser";

/// Where apps keep their data on this system.
fn app_data_base() -> PathBuf {
    let var = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Application Support")).unwrap_or_else(|| ".".into())
    } else {
        var("LOCALAPPDATA").unwrap_or_else(|| ".".into())
    }
}

/// Profile folder. `BROWSER_USER_DATA` overrides it, so tests can run beside a normal
/// session without sharing (or disturbing) its browser process.
pub fn user_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BROWSER_USER_DATA") {
        return dir.into();
    }
    let base = app_data_base();
    let dir = base.join(APP_DIR);
    // Earlier builds kept the profile under another name; carry logins and history over once.
    let old = base.join("Singularity");
    if !dir.exists() && old.join("User Data").exists() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir.join("User Data")
}

fn session_path() -> PathBuf {
    user_data_dir().join("session.json")
}

fn library_path() -> PathBuf {
    user_data_dir().join("library.json")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedPage {
    pub url: String,
    pub title: String,
    pub favicon: String,
}

#[derive(Debug, Default)]
pub struct SavedTab {
    pub page: SavedPage,
    /// Index into `Session::spaces`.
    pub space: usize,
    /// A pinned tab's own page (it goes back there when closed).
    pub pin: Option<SavedPage>,
    /// When it was last on screen, in Unix seconds (0: unknown).
    pub last_used: u64,
}

#[derive(Debug, Default)]
pub struct SavedSpace {
    pub name: String,
    pub color: String,
    /// Index into `Session::tabs` of the tab that was showing in this space.
    pub active: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Session {
    pub tabs: Vec<SavedTab>,
    /// Never empty after `parse`.
    pub spaces: Vec<SavedSpace>,
    /// The space that was showing.
    pub space: usize,
    /// Two tabs (indexes into `tabs`) side by side.
    pub split: Option<(usize, usize)>,
    pub sidebar_width: Option<i32>,
    pub sidebar_docked: Option<bool>,
    /// Normal (restored) window rect in screen pixels, and whether it was maximized.
    pub window: Option<([i32; 4], bool)>,
}

pub fn load() -> Option<Session> {
    parse(&std::fs::read_to_string(session_path()).ok()?)
}

/// Reads both this format (version 2: spaces, pinned tabs) and the first one (one flat list
/// of tabs and the index of the active one).
pub fn parse(text: &str) -> Option<Session> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let page = |v: &Value| SavedPage { url: s(&v["url"]), title: s(&v["title"]), favicon: s(&v["favicon"]) };
    let index = |v: &Value| v.as_u64().map(|i| i as usize);

    let mut spaces: Vec<SavedSpace> = v["spaces"]
        .as_array()
        .map(|a| a.iter().map(|sp| SavedSpace { name: s(&sp["name"]), color: s(&sp["color"]), active: index(&sp["active"]) }).collect())
        .unwrap_or_default();
    if spaces.is_empty() {
        // Version 1: one space, whose active tab was "active".
        spaces.push(SavedSpace { active: index(&v["active"]), ..Default::default() });
    }
    let space_count = spaces.len();

    // Tabs without a URL are dropped, so indexes saved alongside them are remapped.
    let mut remap: Vec<Option<usize>> = Vec::new();
    let mut tabs = Vec::new();
    for t in v["tabs"].as_array()? {
        let tab = SavedTab {
            page: page(t),
            space: index(&t["space"]).filter(|&i| i < space_count).unwrap_or(0),
            pin: t["pin"].is_object().then(|| page(&t["pin"])).filter(|p| !p.url.is_empty()),
            last_used: t["lastUsed"].as_u64().unwrap_or(0),
        };
        if tab.page.url.is_empty() {
            remap.push(None);
        } else {
            remap.push(Some(tabs.len()));
            tabs.push(tab);
        }
    }
    let map = |i: Option<usize>| i.and_then(|i| remap.get(i).copied().flatten());
    for sp in &mut spaces {
        sp.active = map(sp.active);
    }
    let split = v["split"].as_array().and_then(|a| {
        let (l, r) = (map(index(a.first()?)), map(index(a.get(1)?)));
        match (l, r) {
            (Some(l), Some(r)) if l != r && tabs[l].space == tabs[r].space => Some((l, r)),
            _ => None,
        }
    });
    let window = v["window"]["rect"].as_array().and_then(|r| {
        let n: Vec<i32> = r.iter().filter_map(|x| x.as_i64().map(|x| x as i32)).collect();
        (n.len() == 4).then(|| ([n[0], n[1], n[2], n[3]], v["window"]["maximized"].as_bool().unwrap_or(false)))
    });
    Some(Session {
        tabs,
        space: index(&v["space"]).filter(|&i| i < space_count).unwrap_or(0),
        spaces,
        split,
        sidebar_width: v["sidebarWidth"].as_i64().map(|w| w as i32),
        sidebar_docked: v["sidebarDocked"].as_bool(),
        window,
    })
}

pub fn save(json: &str) {
    write_atomic(&session_path(), json);
}

pub fn load_library() -> Library {
    std::fs::read_to_string(library_path()).map(|t| Library::from_json(&t)).unwrap_or_default()
}

pub fn save_library(library: &mut Library) {
    if library.dirty {
        write_atomic(&library_path(), &library.to_json());
        library.dirty = false;
    }
}

/// Written to a temporary file first, so a crash mid-write never loses what was there.
/// The profile folder is made if needed (WebView2 makes it on Windows; WebKit keeps its own
/// data elsewhere, so on macOS nothing else does).
fn write_atomic(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_first_format() {
        let s = parse(
            r#"{"version":1,"tabs":[{"url":"https://a.example/","title":"A","favicon":""},
                {"url":"","title":"broken"},{"url":"https://b.example/","title":"B","favicon":"f"}],
                "active":2,"sidebarWidth":300,"sidebarDocked":false,
                "window":{"rect":[1,2,3,4],"maximized":true}}"#,
        )
        .unwrap();
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.spaces.len(), 1);
        assert_eq!(s.spaces[0].active, Some(1), "index remapped past the dropped tab");
        assert_eq!(s.space, 0);
        assert!(s.tabs.iter().all(|t| t.pin.is_none() && t.space == 0));
        assert_eq!(s.sidebar_width, Some(300));
        assert_eq!(s.sidebar_docked, Some(false));
        assert_eq!(s.window, Some(([1, 2, 3, 4], true)));
    }

    #[test]
    fn reads_spaces_pins_and_split() {
        let s = parse(
            r##"{"version":2,"space":1,
                "spaces":[{"name":"Home","color":"#5b6cf0","active":0},{"name":"Work","color":"#e5484d","active":2}],
                "tabs":[{"url":"https://mail.example/inbox","title":"Inbox","favicon":"","space":0,
                         "pin":{"url":"https://mail.example/","title":"Mail","favicon":"m"},"lastUsed":5},
                        {"url":"https://a.example/","space":1,"lastUsed":7},
                        {"url":"https://b.example/","space":1},
                        {"url":"https://c.example/","space":9}],
                "split":[1,2]}"##,
        )
        .unwrap();
        assert_eq!(s.space, 1);
        assert_eq!(s.spaces[1].name, "Work");
        assert_eq!(s.spaces[1].active, Some(2));
        let pin = s.tabs[0].pin.as_ref().unwrap();
        assert_eq!((pin.url.as_str(), pin.title.as_str()), ("https://mail.example/", "Mail"));
        assert_eq!(s.tabs[0].last_used, 5);
        assert_eq!(s.tabs[3].space, 0, "unknown space falls back to the first");
        assert_eq!(s.split, Some((1, 2)));
    }

    #[test]
    fn ignores_a_split_across_spaces_or_of_one_tab() {
        let tabs = r#""spaces":[{},{}],"tabs":[{"url":"https://a/","space":0},{"url":"https://b/","space":1}]"#;
        assert_eq!(parse(&format!(r#"{{{tabs},"split":[0,1]}}"#)).unwrap().split, None);
        assert_eq!(parse(&format!(r#"{{{tabs},"split":[0,0]}}"#)).unwrap().split, None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("nope").is_none());
        assert!(parse("{}").is_none());
    }
}
