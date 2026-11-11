//! History, bookmarks and archived tabs, kept in `library.json` in the profile, and the
//! search over them that feeds the command bar. No WebView2 or Win32 here, so it is unit tested.

use serde_json::{Value, json};

const MAX_HISTORY: usize = 10_000;
const HISTORY_DAYS: u64 = 90;
const MAX_ARCHIVE: usize = 500;
/// Favicons are usually short URLs; the odd huge data: URI isn't worth keeping.
const MAX_FAVICON_LEN: usize = 2048;
const DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub favicon: String,
}

impl Page {
    pub fn new(url: &str, title: &str, favicon: &str) -> Self {
        let favicon = if favicon.len() > MAX_FAVICON_LEN { "" } else { favicon };
        Page { url: url.to_string(), title: title.to_string(), favicon: favicon.to_string() }
    }
}

pub struct Visit {
    pub page: Page,
    pub visits: u32,
    /// Unix seconds.
    pub last: u64,
}

pub struct Archived {
    pub page: Page,
    /// When it was archived, in Unix seconds.
    pub at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bookmark,
    History,
    Archived,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bookmark => "bookmark",
            Kind::History => "history",
            Kind::Archived => "archived",
        }
    }
}

#[derive(Debug)]
pub struct Suggestion {
    pub kind: Kind,
    pub page: Page,
    /// Last visit, or when it was archived.
    pub at: u64,
}

#[derive(Default)]
pub struct Library {
    /// Unordered; search sorts what it finds.
    pub history: Vec<Visit>,
    /// Newest first.
    pub bookmarks: Vec<Page>,
    /// Newest first.
    pub archive: Vec<Archived>,
    /// Changed since it was last saved.
    pub dirty: bool,
}

/// Only real web pages (and local files) go into history and the archive.
pub fn recordable(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file:"))
        && !url.starts_with("https://browser.ui/")
}

/// "https://www.github.com/x" -> "github.com/x"
fn bare(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.strip_prefix("www.").unwrap_or(rest)
}

impl Library {
    pub fn from_json(text: &str) -> Library {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return Library::default() };
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let page = |v: &Value| Page::new(&s(&v["url"]), &s(&v["title"]), &s(&v["favicon"]));
        let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
        let ok = |p: &Page| recordable(&p.url);
        Library {
            history: list("history")
                .iter()
                .map(|e| Visit { page: page(e), visits: e["visits"].as_u64().unwrap_or(1) as u32, last: e["last"].as_u64().unwrap_or(0) })
                .filter(|e| ok(&e.page))
                .collect(),
            bookmarks: list("bookmarks").iter().map(page).filter(ok).collect(),
            archive: list("archive")
                .iter()
                .map(|e| Archived { page: page(e), at: e["at"].as_u64().unwrap_or(0) })
                .filter(|e| ok(&e.page))
                .collect(),
            dirty: false,
        }
    }

    pub fn to_json(&self) -> String {
        let page = |p: &Page| json!({ "url": p.url, "title": p.title, "favicon": p.favicon });
        json!({
            "version": 1,
            "history": self.history.iter().map(|e| {
                let mut v = page(&e.page);
                v["visits"] = e.visits.into();
                v["last"] = e.last.into();
                v
            }).collect::<Vec<_>>(),
            "bookmarks": self.bookmarks.iter().map(page).collect::<Vec<_>>(),
            "archive": self.archive.iter().map(|e| {
                let mut v = page(&e.page);
                v["at"] = e.at.into();
                v
            }).collect::<Vec<_>>(),
        })
        .to_string()
    }

    // ------------------------------------------------------------ history

    /// A page was opened (a new URL in a tab).
    pub fn record_visit(&mut self, page: Page, now: u64) {
        if !recordable(&page.url) {
            return;
        }
        match self.history.iter_mut().find(|e| e.page.url == page.url) {
            Some(e) => {
                e.visits = e.visits.saturating_add(1);
                e.last = now;
                if !page.title.is_empty() {
                    e.page.title = page.title;
                }
                if !page.favicon.is_empty() {
                    e.page.favicon = page.favicon;
                }
            }
            None => self.history.push(Visit { page, visits: 1, last: now }),
        }
        if self.history.len() > MAX_HISTORY {
            self.history.sort_by_key(|e| std::cmp::Reverse(e.last));
            self.history.truncate(MAX_HISTORY);
        }
        self.dirty = true;
    }

    /// The title or icon of a page arrived after it was recorded.
    pub fn update_page(&mut self, page: &Page) {
        let mut changed = false;
        let mut update = |p: &mut Page| {
            if !page.title.is_empty() && p.title != page.title {
                p.title = page.title.clone();
                changed = true;
            }
            if !page.favicon.is_empty() && p.favicon != page.favicon {
                p.favicon = page.favicon.clone();
                changed = true;
            }
        };
        if let Some(e) = self.history.iter_mut().find(|e| e.page.url == page.url) {
            update(&mut e.page);
        }
        if let Some(b) = self.bookmarks.iter_mut().find(|b| b.url == page.url) {
            update(b);
        }
        self.dirty |= changed;
    }

    /// Forget history older than the limit.
    pub fn prune(&mut self, now: u64) {
        let before = self.history.len();
        self.history.retain(|e| now.saturating_sub(e.last) < HISTORY_DAYS * DAY);
        self.dirty |= self.history.len() != before;
    }

    // ------------------------------------------------------------ bookmarks

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url)
    }

    /// Returns whether the page is bookmarked now.
    pub fn toggle_bookmark(&mut self, page: Page) -> bool {
        if !recordable(&page.url) {
            return false;
        }
        self.dirty = true;
        if let Some(i) = self.bookmarks.iter().position(|b| b.url == page.url) {
            self.bookmarks.remove(i);
            false
        } else {
            self.bookmarks.insert(0, page);
            true
        }
    }

    // ------------------------------------------------------------ archive

    /// A tab was closed or archived automatically.
    pub fn archive(&mut self, page: Page, now: u64) {
        if !recordable(&page.url) {
            return;
        }
        self.archive.retain(|a| a.page.url != page.url);
        self.archive.insert(0, Archived { page, at: now });
        self.archive.truncate(MAX_ARCHIVE);
        self.dirty = true;
    }

    /// Take a page back out of the archive (it's open again).
    pub fn unarchive(&mut self, url: &str) {
        let before = self.archive.len();
        self.archive.retain(|a| a.page.url != url);
        self.dirty |= self.archive.len() != before;
    }

    // ------------------------------------------------------------ search

    /// Bookmarks and history matching every word of `query`, best first.
    pub fn search(&self, query: &str, now: u64, limit: usize) -> Vec<Suggestion> {
        let words = words(query);
        if words.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(f64, Suggestion)> = Vec::new();
        for b in &self.bookmarks {
            if let Some(s) = match_score(&b.url, &b.title, &words) {
                let visit = self.history.iter().find(|e| e.page.url == b.url);
                let (visits, last) = visit.map_or((0, 0), |e| (e.visits, e.last));
                let score = s + 4.0 + frecency(visits, last, now);
                found.push((score, Suggestion { kind: Kind::Bookmark, page: b.clone(), at: last }));
            }
        }
        for e in &self.history {
            if self.is_bookmarked(&e.page.url) {
                continue;
            }
            if let Some(s) = match_score(&e.page.url, &e.page.title, &words) {
                let score = s + frecency(e.visits, e.last, now);
                found.push((score, Suggestion { kind: Kind::History, page: e.page.clone(), at: e.last }));
            }
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.at.cmp(&a.1.at)));
        found.truncate(limit);
        found.into_iter().map(|(_, s)| s).collect()
    }

    /// Archived tabs matching `query` (all of them when it's empty), newest first.
    pub fn search_archive(&self, query: &str, limit: usize) -> Vec<Suggestion> {
        let words = words(query);
        self.archive
            .iter()
            .filter(|a| words.is_empty() || match_score(&a.page.url, &a.page.title, &words).is_some())
            .take(limit)
            .map(|a| Suggestion { kind: Kind::Archived, page: a.page.clone(), at: a.at })
            .collect()
    }
}

fn words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// None unless every word appears in the title or the URL. Higher when the site's name
/// starts with what was typed, or a word of the title does.
fn match_score(url: &str, title: &str, words: &[String]) -> Option<f64> {
    let url_l = bare(url).to_lowercase();
    let title_l = title.to_lowercase();
    let mut score = 0.0;
    for (i, w) in words.iter().enumerate() {
        let in_url = url_l.contains(w.as_str());
        let in_title = title_l.contains(w.as_str());
        if !in_url && !in_title {
            return None;
        }
        if i == 0 && url_l.starts_with(w.as_str()) {
            score += 6.0;
        } else if title_l.split(|c: char| !c.is_alphanumeric()).any(|t| t.starts_with(w.as_str())) {
            score += 2.0;
        } else if in_url && url_l.split(['.', '/', '-', '?', '=']).any(|t| t.starts_with(w.as_str())) {
            score += 1.0;
        }
    }
    Some(score)
}

/// How often and how recently a page was visited.
fn frecency(visits: u32, last: u64, now: u64) -> f64 {
    let age = now.saturating_sub(last);
    let recency = if last == 0 {
        0.0
    } else if age < DAY {
        3.0
    } else if age < 7 * DAY {
        2.0
    } else if age < 30 * DAY {
        1.0
    } else {
        0.0
    };
    (visits as f64).ln_1p() * 2.0 + recency
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn page(url: &str, title: &str) -> Page {
        Page::new(url, title, "")
    }

    fn urls(found: &[Suggestion]) -> Vec<&str> {
        found.iter().map(|s| s.page.url.as_str()).collect()
    }

    #[test]
    fn records_only_web_pages() {
        let mut lib = Library::default();
        lib.record_visit(page("https://browser.ui/newtab.html", "New Tab"), NOW);
        lib.record_visit(page("about:blank", ""), NOW);
        lib.record_visit(page("data:text/html,hi", ""), NOW);
        assert!(lib.history.is_empty());
        assert!(!lib.dirty);
        lib.record_visit(page("https://example.com/", "Example"), NOW);
        lib.record_visit(page("https://example.com/", ""), NOW + 5);
        assert_eq!(lib.history.len(), 1);
        assert_eq!(lib.history[0].visits, 2);
        assert_eq!(lib.history[0].last, NOW + 5);
        assert_eq!(lib.history[0].page.title, "Example", "an empty title doesn't overwrite");
        assert!(lib.dirty);
    }

    #[test]
    fn every_word_must_match() {
        let mut lib = Library::default();
        lib.record_visit(page("https://github.com/rust-lang/rust", "Rust language repo"), NOW);
        lib.record_visit(page("https://news.ycombinator.com/", "Hacker News"), NOW);
        assert_eq!(urls(&lib.search("rust repo", NOW, 10)), ["https://github.com/rust-lang/rust"]);
        assert_eq!(urls(&lib.search("HACKER", NOW, 10)), ["https://news.ycombinator.com/"]);
        assert!(lib.search("rust hacker", NOW, 10).is_empty());
        assert!(lib.search("   ", NOW, 10).is_empty());
    }

    #[test]
    fn site_name_prefix_ranks_first() {
        let mut lib = Library::default();
        // Mentions "git" in the title and was visited more, but its address doesn't start with it.
        for _ in 0..5 {
            lib.record_visit(page("https://example.com/how-to-git", "How to use git"), NOW);
        }
        lib.record_visit(page("https://www.github.com/", "GitHub"), NOW);
        assert_eq!(urls(&lib.search("git", NOW, 10))[0], "https://www.github.com/");
    }

    #[test]
    fn frequent_and_recent_ranks_higher() {
        let mut lib = Library::default();
        lib.record_visit(page("https://docs.example.com/old", "Docs old"), NOW - 60 * DAY);
        for _ in 0..3 {
            lib.record_visit(page("https://docs.example.com/new", "Docs new"), NOW - 60);
        }
        assert_eq!(urls(&lib.search("docs", NOW, 10)), ["https://docs.example.com/new", "https://docs.example.com/old"]);
    }

    #[test]
    fn bookmarks_rank_higher_and_are_not_repeated_as_history() {
        let mut lib = Library::default();
        // Visited a few more times than the bookmark, which still ranks first.
        for _ in 0..3 {
            lib.record_visit(page("https://a.example/", "Example A"), NOW);
        }
        lib.record_visit(page("https://b.example/", "Example B"), NOW);
        assert!(lib.toggle_bookmark(page("https://b.example/", "Example B")));
        assert!(lib.is_bookmarked("https://b.example/"));
        let found = lib.search("example", NOW, 10);
        assert_eq!(urls(&found), ["https://b.example/", "https://a.example/"]);
        assert_eq!(found[0].kind, Kind::Bookmark);
        assert_eq!(found[1].kind, Kind::History);
        assert!(!lib.toggle_bookmark(page("https://b.example/", "")));
        assert!(!lib.is_bookmarked("https://b.example/"));
        assert!(!lib.toggle_bookmark(page("https://browser.ui/newtab.html", "")), "can't bookmark the new tab page");
    }

    #[test]
    fn archive_is_newest_first_without_duplicates() {
        let mut lib = Library::default();
        lib.archive(page("https://one.example/", "One"), NOW);
        lib.archive(page("https://two.example/", "Two"), NOW + 1);
        lib.archive(page("https://one.example/", "One"), NOW + 2);
        lib.archive(page("https://browser.ui/newtab.html", "New Tab"), NOW + 3);
        assert_eq!(urls(&lib.search_archive("", 10)), ["https://one.example/", "https://two.example/"]);
        assert_eq!(urls(&lib.search_archive("two", 10)), ["https://two.example/"]);
        lib.unarchive("https://one.example/");
        assert_eq!(urls(&lib.search_archive("", 10)), ["https://two.example/"]);
    }

    #[test]
    fn prunes_old_history() {
        let mut lib = Library::default();
        lib.record_visit(page("https://old.example/", ""), NOW - 100 * DAY);
        lib.record_visit(page("https://new.example/", ""), NOW - DAY);
        lib.dirty = false;
        lib.prune(NOW);
        assert_eq!(lib.history.len(), 1);
        assert!(lib.dirty);
    }

    #[test]
    fn titles_and_icons_arriving_later_are_kept() {
        let mut lib = Library::default();
        lib.record_visit(page("https://example.com/", ""), NOW);
        lib.toggle_bookmark(page("https://example.com/", ""));
        lib.dirty = false;
        lib.update_page(&Page::new("https://example.com/", "Example", "https://example.com/favicon.ico"));
        assert!(lib.dirty);
        assert_eq!(lib.history[0].page.title, "Example");
        assert_eq!(lib.bookmarks[0].favicon, "https://example.com/favicon.ico");
    }

    #[test]
    fn round_trips_through_json() {
        let mut lib = Library::default();
        lib.record_visit(Page::new("https://example.com/", "Example", "https://example.com/i.ico"), NOW);
        lib.toggle_bookmark(page("https://b.example/", "B"));
        lib.archive(page("https://c.example/", "C"), NOW);
        let back = Library::from_json(&lib.to_json());
        assert_eq!(back.history.len(), 1);
        assert_eq!(back.history[0].page.favicon, "https://example.com/i.ico");
        assert_eq!(back.history[0].last, NOW);
        assert_eq!(back.bookmarks, vec![page("https://b.example/", "B")]);
        assert_eq!(back.archive[0].at, NOW);
        assert!(!back.dirty);
        assert!(Library::from_json("not json").history.is_empty());
    }

    #[test]
    fn huge_favicons_are_dropped() {
        let big = "x".repeat(MAX_FAVICON_LEN + 1);
        assert_eq!(Page::new("https://a.example/", "", &big).favicon, "");
    }
}
