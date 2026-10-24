//! One file browser: a directory, its rows (list or tree) and the cursor.
//! Reads the file system but makes no host calls.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use proteus_plugin::List;

use crate::fsys::{self, Entry, Sort};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sidebar,
    Pane,
    Picker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    List,
    Tree,
    Miller,
}

impl Layout {
    pub fn next(self) -> Layout {
        match self {
            Layout::Miller => Layout::Tree,
            Layout::Tree => Layout::List,
            Layout::List => Layout::Miller,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Layout::List => "layout-list",
            Layout::Tree => "layout-tree",
            Layout::Miller => "layout-columns",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub path: PathBuf,
    pub depth: usize,
    pub entry: Entry,
    /// An expanded directory (tree).
    pub open: bool,
}

/// At most this many entries are shown per directory.
const MAX_ENTRIES: usize = 100_000;

pub struct View {
    pub kind: Kind,
    pub layout: Layout,
    pub dir: PathBuf,
    /// Sorted listings (the directory, open tree nodes, the miller parent
    /// and preview); dropped by `reload`.
    listings: HashMap<PathBuf, Result<Vec<Entry>, String>>,
    pub expanded: BTreeSet<PathBuf>,
    pub rows: Vec<Row>,
    pub list: List,
    pub filter: String,
    pub hidden: bool,
    pub sort: Sort,
    pub selected: BTreeSet<PathBuf>,
    /// `v`: the anchor row and the marks from before.
    pub visual: Option<(usize, BTreeSet<PathBuf>)>,
}

impl View {
    pub fn new(kind: Kind, dir: PathBuf) -> View {
        let layout = match kind {
            Kind::Sidebar => Layout::Tree,
            Kind::Pane => Layout::Miller,
            Kind::Picker => Layout::List,
        };
        let mut v = View {
            kind,
            layout,
            dir,
            listings: HashMap::new(),
            expanded: BTreeSet::new(),
            rows: Vec::new(),
            list: List::default(),
            filter: String::new(),
            hidden: false,
            sort: Sort::default(),
            selected: BTreeSet::new(),
            visual: None,
        };
        v.rebuild();
        v
    }

    /// The (cached) sorted listing of `dir`.
    pub fn listing(&mut self, dir: &Path) -> &Result<Vec<Entry>, String> {
        let sort = self.sort;
        self.listings.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut v = fsys::list(dir).map_err(|e| e.to_string())?;
            sort.apply(&mut v);
            v.truncate(MAX_ENTRIES);
            Ok(v)
        })
    }

    /// Why the directory could not be listed.
    pub fn error(&self) -> Option<&str> {
        self.listings.get(&self.dir)?.as_ref().err().map(String::as_str)
    }

    /// Read everything again.
    pub fn reload(&mut self) {
        self.listings.clear();
        self.rebuild();
    }

    /// Rows from the listings, keeping the cursor on the same path.
    pub fn rebuild(&mut self) {
        let keep = self.current().map(|r| r.path.clone());
        let mut rows = Vec::new();
        let dir = self.dir.clone();
        self.add_rows(&dir, 0, &mut rows);
        self.rows = rows;
        self.list.len = self.rows.len();
        if let Some(p) = keep {
            self.focus(&p);
        }
        self.list.selected = self.list.selected.min(self.list.len.saturating_sub(1));
    }

    /// Rows of `dir` at `depth`; returns whether any row was added.
    fn add_rows(&mut self, dir: &Path, depth: usize, rows: &mut Vec<Row>) -> bool {
        let entries = match self.listing(dir) {
            Ok(v) => v.clone(),
            Err(_) => return false,
        };
        let start = rows.len();
        let hidden = self.hidden;
        for e in entries.into_iter().filter(|e| hidden || !e.hidden()) {
            let path = dir.join(&e.name);
            let open = self.layout == Layout::Tree && e.dir && self.expanded.contains(&path);
            let matched = self.filter.is_empty() || fsys::fuzzy(&self.filter, &e.name);
            let at = rows.len();
            rows.push(Row { path: path.clone(), depth, entry: e, open });
            let inner = open && self.add_rows(&path, depth + 1, rows);
            if !matched && !inner {
                rows.truncate(at);
            }
        }
        rows.len() > start
    }

    pub fn current(&self) -> Option<&Row> {
        self.rows.get(self.list.selected)
    }

    /// Put the cursor on `path` if it is shown.
    pub fn focus(&mut self, path: &Path) -> bool {
        match self.rows.iter().position(|r| r.path == path) {
            Some(i) => {
                self.list.selected = i;
                true
            }
            None => false,
        }
    }

    /// Show `dir`; the cursor goes to `focus` (or the top).
    pub fn cd(&mut self, dir: PathBuf, focus: Option<&Path>) {
        if dir != self.dir {
            self.dir = dir;
            self.filter.clear();
            self.expanded.clear();
            self.listings.clear();
            self.list = List::default();
        }
        self.rebuild();
        if let Some(f) = focus {
            self.focus(f);
        }
    }

    /// The parent directory, with the cursor on where we came from.
    pub fn up(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else { return };
        let from = self.dir.clone();
        self.cd(parent, Some(&from));
    }

    /// Tree: open or close the directory under the cursor.
    pub fn toggle_open(&mut self) {
        let Some(r) = self.current().filter(|r| r.entry.dir) else { return };
        let p = r.path.clone();
        if !self.expanded.remove(&p) {
            self.expanded.insert(p);
        }
        self.rebuild();
    }

    /// Tree `h`: close the open directory, or go to the parent row (at the
    /// top level: the parent directory).
    pub fn tree_back(&mut self) {
        let Some(r) = self.current() else { return self.up() };
        if r.open {
            let p = r.path.clone();
            self.expanded.remove(&p);
            return self.rebuild();
        }
        if r.depth == 0 {
            return self.up();
        }
        let parent = r.path.parent().map(Path::to_path_buf);
        if let Some(p) = parent {
            self.focus(&p);
        }
    }

    pub fn set_filter(&mut self, f: &str) {
        self.filter = f.to_string();
        self.rebuild();
        if self.current().is_none_or(|r| !self.filter.is_empty() && !fsys::fuzzy(&self.filter, &r.entry.name)) {
            self.list.selected = self.rows.iter().position(|r| fsys::fuzzy(&self.filter, &r.entry.name)).unwrap_or(0);
        }
    }

    pub fn set_sort(&mut self, sort: Sort) {
        self.sort = sort;
        self.reload();
    }

    pub fn toggle_hidden(&mut self) {
        self.hidden = !self.hidden;
        self.rebuild();
    }

    pub fn set_layout(&mut self, l: Layout) {
        self.layout = l;
        self.rebuild();
    }

    /// Space: mark the cursor row and move down.
    pub fn toggle_selected(&mut self) {
        let Some(p) = self.current().map(|r| r.path.clone()) else { return };
        if !self.selected.remove(&p) {
            self.selected.insert(p);
        }
        self.list.selected = (self.list.selected + 1).min(self.list.len.saturating_sub(1));
    }

    /// `v`: start or end marking the rows between an anchor and the cursor.
    pub fn toggle_visual(&mut self) {
        if self.visual.take().is_none() {
            self.visual = Some((self.list.selected, self.selected.clone()));
            self.update_visual();
        }
    }

    /// The cursor moved: mark the range.
    pub fn update_visual(&mut self) {
        let Some((anchor, base)) = &self.visual else { return };
        let (a, b) = (*anchor.min(&self.list.selected), *anchor.max(&self.list.selected));
        let mut sel = base.clone();
        sel.extend(self.rows.iter().skip(a).take(b + 1 - a).map(|r| r.path.clone()));
        self.selected = sel;
    }

    /// `*`: invert the marks of the shown rows.
    pub fn invert_selected(&mut self) {
        for r in &self.rows {
            if !self.selected.remove(&r.path) {
                self.selected.insert(r.path.clone());
            }
        }
    }

    /// What an action applies to: the marked paths, else the cursor row.
    pub fn targets(&self) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            self.current().map(|r| vec![r.path.clone()]).unwrap_or_default()
        } else {
            self.selected.iter().cloned().collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("proteus-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for p in ["src/deep", "docs", ".git"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        for f in ["src/main.rs", "src/lib.rs", "src/deep/x.rs", "README.md", ".hidden", "file10", "file2"] {
            std::fs::write(d.join(f), f).unwrap();
        }
        d
    }

    fn names(v: &View) -> Vec<String> {
        v.rows.iter().map(|r| format!("{}{}", "-".repeat(r.depth), r.entry.name)).collect()
    }

    #[test]
    fn lists_sorted_and_hidden() {
        let d = tmp("list");
        let mut v = View::new(Kind::Pane, d.clone());
        assert_eq!(names(&v), ["docs", "src", "file2", "file10", "README.md"]);
        v.toggle_hidden();
        assert_eq!(names(&v)[0], ".git");
        v.toggle_hidden();
        v.list.selected = 1;
        v.cd(d.join("src"), None);
        assert_eq!(names(&v), ["deep", "lib.rs", "main.rs"]);
        v.up();
        assert_eq!(v.current().unwrap().entry.name, "src");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tree_and_filter() {
        let d = tmp("tree");
        let mut v = View::new(Kind::Sidebar, d.clone());
        v.focus(&d.join("src"));
        v.toggle_open();
        assert_eq!(names(&v), ["docs", "src", "-deep", "-lib.rs", "-main.rs", "file2", "file10", "README.md"]);
        v.focus(&d.join("src/deep"));
        v.toggle_open();
        assert!(names(&v).contains(&"--x.rs".to_string()));
        // A filter keeps the directories leading to a match.
        v.set_filter("x.r");
        assert_eq!(names(&v), ["src", "-deep", "--x.rs"]);
        assert_eq!(v.current().unwrap().entry.name, "x.rs");
        v.set_filter("");
        v.focus(&d.join("src/lib.rs"));
        v.tree_back();
        assert_eq!(v.current().unwrap().entry.name, "src");
        v.tree_back();
        assert!(!names(&v).contains(&"-lib.rs".to_string()));
        v.toggle_selected();
        assert_eq!(v.targets(), [d.join("src")]);
        v.selected.clear();
        v.list.selected = 0;
        v.toggle_visual();
        v.list.selected = 2;
        v.update_visual();
        assert_eq!(v.targets(), [d.join("docs"), d.join("file2"), d.join("src")]);
        v.toggle_visual();
        assert!(v.visual.is_none() && v.selected.len() == 3);
        let _ = std::fs::remove_dir_all(&d);
    }
}
