//! Prototype user interface for Linux (X11). Same principles as the Windows
//! UI: all filesystem work happens on background threads; the UI thread only
//! drains a message channel and paints the visible rows.
//!
//! In scope: browsing, sorting, live folder updates, type-to-find, and
//! indexed search with metadata filled in after the query. Not yet: file
//! operations, clipboard file transfer, drag and drop, tabs, properties.

use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Button, Color32, FontId, Key, Modifiers, Rect, ScrollArea, Sense, Stroke, TextEdit, Ui};

use super::fsys::{self, Row, SortKey};
use super::index::{Build, Index, Matcher};
use super::proto;
use super::sys;
use super::watch::{self, Change, Watcher};
use crate::typeahead;

/// Search results materialized for display; the total count is still exact.
const MAX_RESULTS: usize = 250_000;
/// Rows in the first chunk of a daemon reply: more than a tall window shows.
const SCREENFUL: u32 = 200;
const QUERY_DEBOUNCE: Duration = Duration::from_millis(30);
const MAX_INDEXES: usize = 4;
const TYPEAHEAD_RESET: Duration = Duration::from_millis(1200);
const ROW_H: f32 = 22.0;
const RED: Color32 = Color32::from_rgb(230, 80, 80);

enum Msg {
    Listing { generation: u64, result: Result<Vec<Row>, String>, took: Duration },
    Changes { generation: u64, changes: Vec<Change> },
    Built(Option<Index>),
    /// The first rows of a search (all of them for in-process searches).
    Results { generation: u64, rows: Vec<Row>, total: usize, first: Duration, complete: bool },
    /// More rows of a streamed search; `complete` on the last batch.
    MoreRows { generation: u64, rows: Vec<Row>, complete: bool },
    /// Sizes and dates for result rows, by row id.
    Meta { generation: u64, filled: Vec<(u32, u64, i64)> },
    Fonts(Option<String>),
    /// Latest status of the index daemon (`None`: not running).
    Daemon(Option<proto::Status>),
    /// The daemon couldn't answer this query; search in-process instead.
    DaemonFailed { generation: u64 },
}

struct Results {
    rows: Vec<Row>,
    total: usize,
    started: Instant,
    /// Time until the first rows were shown.
    first: Duration,
    /// Time until every row had arrived (None while streaming).
    all: Option<Duration>,
    /// Whether each row (by id) has its size and date yet.
    filled: Vec<bool>,
    filled_count: usize,
    /// Time until every row had its metadata.
    details: Option<Duration>,
    /// Row id -> index in `rows` (rebuilt after sorting).
    pos: Vec<u32>,
    /// Feeds the metadata worker; dropping it stops the worker.
    backfill: Option<Sender<Fill>>,
    /// Ids last sent as visible, to avoid resending every frame.
    prioritized: Vec<u32>,
}

impl Results {
    fn reindex(&mut self) {
        self.pos.resize(self.rows.len(), 0);
        for (i, r) in self.rows.iter().enumerate() {
            self.pos[r.id as usize] = i as u32;
        }
    }

    fn is_filled(&self, row: &Row) -> bool {
        self.filled.get(row.id as usize).copied().unwrap_or(true)
    }
}

/// A result row's folder and name, as the backfill worker needs them.
type RowKey = (Arc<[u8]>, Box<[u8]>);

/// Work for the metadata backfill worker.
enum Fill {
    /// New rows (id, folder, name), in display order.
    Add(Vec<(u32, RowKey)>),
    /// Rows on screen now: done before anything else.
    Prioritize(Vec<u32>),
}

/// Rows statted per round; visible rows jump the queue between rounds.
const FILL_BATCH: usize = 2048;

/// Stat result rows in the background, visible rows first, and report
/// sizes and dates in batches. Ends when the search is replaced (its
/// channel closes or the generation moves on).
fn backfill(generation: u64, current: Arc<AtomicU64>, rx: Receiver<Fill>, tx: Sender<Msg>, ctx: egui::Context) {
    use std::collections::VecDeque;
    use std::sync::mpsc::TryRecvError;
    let mut keys: Vec<Option<RowKey>> = Vec::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut urgent: Vec<u32> = Vec::new();
    let take = |f: Fill, keys: &mut Vec<Option<RowKey>>, queue: &mut VecDeque<u32>, urgent: &mut Vec<u32>| match f {
        Fill::Add(rows) => {
            for (id, key) in rows {
                if keys.len() <= id as usize {
                    keys.resize(id as usize + 1, None);
                }
                keys[id as usize] = Some(key);
                queue.push_back(id);
            }
        }
        Fill::Prioritize(ids) => *urgent = ids,
    };
    loop {
        // Block only when there's nothing to do.
        if queue.is_empty() && urgent.is_empty() {
            match rx.recv() {
                Ok(f) => take(f, &mut keys, &mut queue, &mut urgent),
                Err(_) => return,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(f) => take(f, &mut keys, &mut queue, &mut urgent),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if current.load(Relaxed) != generation {
            return;
        }
        fn pick(id: u32, keys: &mut [Option<RowKey>], batch: &mut Vec<Row>) {
            if let Some((dir, name)) = keys.get_mut(id as usize).and_then(Option::take) {
                batch.push(Row { name, dir: Some(dir), size: 0, mtime: 0, flags: 0, sel: false, id });
            }
        }
        let mut batch: Vec<Row> = Vec::with_capacity(FILL_BATCH);
        for id in std::mem::take(&mut urgent) {
            pick(id, &mut keys, &mut batch);
        }
        while batch.len() < FILL_BATCH {
            let Some(id) = queue.pop_front() else { break };
            pick(id, &mut keys, &mut batch);
        }
        if batch.is_empty() {
            continue;
        }
        // Rows of one folder side by side share an open handle.
        batch.sort_unstable_by(|a, b| a.dir.as_deref().cmp(&b.dir.as_deref()));
        fsys::fill_meta(&mut batch);
        let filled = batch.into_iter().map(|r| (r.id, r.size, r.mtime)).collect();
        if tx.send(Msg::Meta { generation, filled }).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

struct TypeAhead {
    text: String,
    last: Instant,
    positions: Vec<usize>,
    path: Vec<u8>,
}

enum Action {
    Open(usize),
    OpenLocation(usize),
    CopyPath(usize),
    Sort(SortKey),
}

pub struct App {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    cwd: Vec<u8>,
    rows: Vec<Row>,
    /// Indices into the shown source (listing or results) that pass the
    /// hidden-file filter, in display order.
    view: Vec<usize>,
    loading: bool,
    error: Option<String>,
    listed_in: Duration,
    listing_gen: u64,
    watcher: Option<Watcher>,
    /// Entry to focus once the pending listing arrives (the folder we came
    /// up from, or a search result whose location was opened).
    pending_keep: Option<Vec<u8>>,
    back: Vec<Vec<u8>>,
    forward: Vec<Vec<u8>>,
    sort: SortKey,
    descending: bool,
    show_hidden: bool,

    path_edit: Option<String>,
    /// Focus the path field and select its text on the next frame.
    path_select: bool,
    query: String,
    regex: bool,
    query_dirty: Option<Instant>,
    query_gen: Arc<AtomicU64>,
    matcher: Option<Arc<Matcher>>,
    results: Option<Results>,
    search_error: Option<String>,
    searching: bool,
    indexes: Vec<Arc<Index>>,
    build: Option<Arc<Build>>,

    focus: Option<usize>,
    anchor: Option<usize>,
    scroll_to: Option<usize>,
    scroll_y: f32,
    view_h: f32,
    typeahead: Option<TypeAhead>,

    places: Vec<(String, Vec<u8>)>,
    volumes: Vec<(String, Vec<u8>)>,
    font: Option<String>,
    /// The system index daemon, when one is running.
    daemon: Option<proto::Status>,
    /// It failed a query; use the in-process index until its next status.
    daemon_failed: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<String>) -> App {
        install_style(&cc.egui_ctx);
        let (tx, rx) = channel();
        let home = sys::home();
        let cwd = match start {
            Some(s) => fsys::resolve_input(&s, &std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or(home)),
            None => sys::home(),
        };
        let volumes = sys::volumes()
            .into_iter()
            .map(|m| {
                let label = if m.point == b"/" { "File system".to_string() } else { String::from_utf8_lossy(fsys::base_name(&m.point)).into_owned() };
                (format!("{label}  ({})", m.fstype), m.point)
            })
            .collect();
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            cwd: Vec::new(),
            rows: Vec::new(),
            view: Vec::new(),
            loading: false,
            error: None,
            listed_in: Duration::ZERO,
            listing_gen: 0,
            watcher: None,
            pending_keep: None,
            back: Vec::new(),
            forward: Vec::new(),
            sort: SortKey::Name,
            descending: false,
            show_hidden: false,
            path_edit: None,
            path_select: false,
            query: String::new(),
            regex: false,
            query_dirty: None,
            query_gen: Arc::new(AtomicU64::new(0)),
            matcher: None,
            results: None,
            search_error: None,
            searching: false,
            indexes: Vec::new(),
            build: None,
            focus: None,
            anchor: None,
            scroll_to: None,
            scroll_y: 0.0,
            view_h: 0.0,
            typeahead: None,
            places: sys::quick_access(),
            volumes,
            font: None,
            daemon: None,
            daemon_failed: false,
        };
        app.load_system_font();
        app.watch_daemon();
        app.navigate(cwd, false);
        app
    }

    /// The desktop's UI font (via fontconfig), loaded off the UI thread.
    /// egui's built-in fonts stay as the fallback for glyphs it lacks.
    fn load_system_font(&self) {
        let (ctx, tx) = (self.ctx.clone(), self.tx.clone());
        thread::spawn(move || {
            let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif"]).output().ok();
            let file = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).filter(|f| f.ends_with(".ttf") || f.ends_with(".otf"));
            let loaded = file.and_then(|f| std::fs::read(&f).ok().map(|bytes| (f, bytes)));
            let name = loaded.map(|(f, bytes)| {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert("system".into(), Arc::new(egui::FontData::from_owned(bytes)));
                fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "system".into());
                ctx.set_fonts(fonts);
                f.rsplit('/').next().unwrap_or(&f).to_string()
            });
            let _ = tx.send(Msg::Fonts(name));
            ctx.request_repaint();
        });
    }

    /// Poll the index daemon's status every few seconds (cheap: one small
    /// message over a local socket).
    fn watch_daemon(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            loop {
                if tx.send(Msg::Daemon(proto::status().ok())).is_err() {
                    return;
                }
                ctx.request_repaint();
                thread::sleep(Duration::from_secs(5));
            }
        });
    }

    fn daemon_covers(&self, path: &[u8]) -> bool {
        !self.daemon_failed && self.daemon.as_ref().is_some_and(|d| d.ready && fsys::in_scope(path, &d.root))
    }

    // ------------------------------------------------------------ model

    fn in_results(&self) -> bool {
        self.results.is_some()
    }

    fn source(&self) -> &[Row] {
        self.results.as_ref().map_or(&self.rows, |r| &r.rows)
    }

    fn source_mut(&mut self) -> &mut Vec<Row> {
        match &mut self.results {
            Some(r) => &mut r.rows,
            None => &mut self.rows,
        }
    }

    fn row(&self, vi: usize) -> Option<&Row> {
        self.view.get(vi).and_then(|&i| self.source().get(i))
    }

    fn focus_path(&self) -> Option<Vec<u8>> {
        self.focus.and_then(|vi| self.row(vi)).map(|r| r.full_path(&self.cwd))
    }

    fn rebuild_view(&mut self, keep: Option<Vec<u8>>) {
        let hidden = self.show_hidden;
        self.view = self.source().iter().enumerate().filter(|(_, r)| hidden || !r.is_hidden()).map(|(i, _)| i).collect();
        self.focus = keep.and_then(|k| (0..self.view.len()).find(|&vi| self.row(vi).is_some_and(|r| r.full_path(&self.cwd) == k)));
        self.anchor = self.focus;
    }

    fn resort(&mut self) {
        let keep = self.focus_path();
        let (key, desc) = (self.sort, self.descending);
        fsys::sort_rows(self.source_mut(), key, desc);
        if let Some(r) = self.results.as_mut() {
            r.reindex();
        }
        self.rebuild_view(keep);
        self.scroll_to = self.focus;
    }

    fn select_only(&mut self, vi: usize) {
        for i in 0..self.view.len() {
            let idx = self.view[i];
            self.source_mut()[idx].sel = i == vi;
        }
        self.focus = Some(vi);
        self.anchor = Some(vi);
    }

    fn select_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        for i in 0..self.view.len() {
            let idx = self.view[i];
            self.source_mut()[idx].sel = (lo..=hi).contains(&i);
        }
        self.focus = Some(b);
    }

    fn toggle(&mut self, vi: usize) {
        if let Some(&idx) = self.view.get(vi) {
            let r = &mut self.source_mut()[idx];
            r.sel = !r.sel;
        }
        self.focus = Some(vi);
        self.anchor = Some(vi);
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, path: Vec<u8>, record: bool) {
        if record && path != self.cwd {
            self.back.push(std::mem::take(&mut self.cwd));
            self.forward.clear();
        }
        self.cwd = path;
        self.path_edit = None;
        self.typeahead = None;
        if !self.query.is_empty() {
            self.query.clear();
            self.clear_search();
        }
        self.load_listing(None);
    }

    fn load_listing(&mut self, keep: Option<Vec<u8>>) {
        self.listing_gen += 1;
        let generation = self.listing_gen;
        self.loading = true;
        self.error = None;
        self.watcher = None;
        if keep.is_none() {
            self.rows.clear();
            self.view.clear();
            self.focus = None;
            self.scroll_to = Some(0);
        }
        let (path, tx, ctx) = (self.cwd.clone(), self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let t = Instant::now();
            let result = fsys::list_dir(&path);
            let _ = tx.send(Msg::Listing { generation, result, took: t.elapsed() });
            ctx.request_repaint();
        });
        // Watch from the start, so nothing between listing and watching is missed.
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.watcher = watch::watch(self.cwd.clone(), move |changes| {
            let _ = tx.send(Msg::Changes { generation, changes });
            ctx.request_repaint();
        });
        self.pending_keep = keep;
    }

    fn go_up(&mut self) {
        if let Some(p) = fsys::parent(&self.cwd) {
            let child = self.cwd.clone();
            self.navigate(p, true);
            self.pending_keep = Some(child);
        }
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            self.forward.push(self.cwd.clone());
            self.navigate(p, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(self.cwd.clone());
            self.navigate(p, false);
        }
    }

    fn refresh(&mut self) {
        if self.in_results() {
            // F5 in search results rebuilds the index, as on Windows.
            self.indexes.retain(|i| !i.covers(&self.cwd));
            self.start_search();
        } else {
            let keep = self.focus_path();
            self.load_listing(keep);
        }
    }

    fn open(&mut self, vi: usize) {
        let Some(row) = self.row(vi) else { return };
        let path = row.full_path(&self.cwd);
        if row.is_dir() {
            self.navigate(path, true);
        } else if let Err(e) = sys::open_with_default(&path) {
            self.error = Some(e);
        }
    }

    fn apply_changes(&mut self, changes: Vec<Change>) {
        if changes.iter().any(|c| matches!(c, Change::Rescan)) {
            let keep = self.focus_path();
            return self.load_listing(keep);
        }
        let keep = self.focus_path();
        let mut at: std::collections::HashMap<Box<[u8]>, usize> =
            self.rows.iter().enumerate().map(|(i, r)| (r.name.clone(), i)).collect();
        let mut removed = Vec::new();
        for c in changes {
            match c {
                Change::Upsert(mut row) => match at.get(&row.name) {
                    Some(&i) => {
                        row.sel = self.rows[i].sel;
                        self.rows[i] = row;
                    }
                    None => {
                        at.insert(row.name.clone(), self.rows.len());
                        self.rows.push(row);
                    }
                },
                Change::Removed(name) => {
                    if let Some(i) = at.remove(&name) {
                        removed.push(i);
                    }
                }
                Change::Rescan => {}
            }
        }
        removed.sort_unstable_by(|a, b| b.cmp(a));
        for i in removed {
            self.rows.swap_remove(i);
        }
        fsys::sort_rows(&mut self.rows, self.sort, self.descending);
        if !self.in_results() {
            self.rebuild_view(keep);
        }
    }

    // ------------------------------------------------------------ search

    fn clear_search(&mut self) {
        self.query_gen.fetch_add(1, Relaxed);
        self.matcher = None;
        self.search_error = None;
        self.searching = false;
        if self.results.take().is_some() {
            self.rebuild_view(None);
        }
    }

    fn start_search(&mut self) {
        self.query_dirty = None;
        let m = match Matcher::new(&self.query, self.regex) {
            Ok(Some(m)) => Arc::new(m),
            Ok(None) => return self.clear_search(),
            Err(e) => {
                self.search_error = Some(e);
                return;
            }
        };
        self.search_error = None;
        self.matcher = Some(m.clone());
        if self.daemon_covers(&self.cwd) {
            return self.run_daemon_query();
        }
        if let Some(index) = self.indexes.iter().find(|i| i.covers(&self.cwd)).cloned() {
            return self.run_query(index, m);
        }
        if self.build.as_ref().is_some_and(|b| fsys::in_scope(&self.cwd, &b.root)) {
            return; // the query runs when the crawl finishes
        }
        if let Some(b) = self.build.take() {
            b.cancel();
        }
        let build = Build::new(&self.cwd, false);
        self.build = Some(build.clone());
        self.searching = true;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let index = build.run();
            let _ = tx.send(Msg::Built(index));
            ctx.request_repaint();
        });
    }

    /// Ask the daemon: its index is always ready and current, so there is no
    /// crawl. The first screenful arrives sorted and is shown at once; the
    /// rest streams in behind it. Rows come without metadata, which is
    /// backfilled here, as the user.
    fn run_daemon_query(&mut self) {
        let generation = self.query_gen.fetch_add(1, Relaxed) + 1;
        self.searching = true;
        let current = self.query_gen.clone();
        let (scope, text, regex, tx, ctx) = (self.cwd.clone(), self.query.clone(), self.regex, self.tx.clone(), self.ctx.clone());
        let order = match (self.sort, self.descending) {
            (SortKey::Name, false) => proto::NAME_ASC,
            (SortKey::Name, true) => proto::NAME_DESC,
            _ => proto::UNORDERED, // sizes and dates aren't known yet
        };
        thread::spawn(move || {
            let t = Instant::now();
            let send = |msg: Msg| {
                let ok = current.load(Relaxed) == generation && tx.send(msg).is_ok();
                ctx.request_repaint();
                ok
            };
            match proto::query(&scope, &text, regex, MAX_RESULTS as u32, order, SCREENFUL) {
                Ok(proto::Reply::Rows { rows, total, mut rest }) => {
                    if !send(Msg::Results { generation, rows, total, first: t.elapsed(), complete: false }) {
                        return;
                    }
                    loop {
                        let (rows, complete) = match rest.next() {
                            Ok(Some(rows)) => (rows, false),
                            Ok(None) | Err(_) => (Vec::new(), true),
                        };
                        if !send(Msg::MoreRows { generation, rows, complete }) || complete {
                            return;
                        }
                    }
                }
                _ => {
                    send(Msg::DaemonFailed { generation });
                }
            }
        });
    }

    fn run_query(&mut self, index: Arc<Index>, m: Arc<Matcher>) {
        let generation = self.query_gen.fetch_add(1, Relaxed) + 1;
        self.searching = true;
        let current = self.query_gen.clone();
        let (scope, tx, ctx) = (self.cwd.clone(), self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let t = Instant::now();
            let cancelled = || current.load(Relaxed) != generation;
            let Some((rows, total)) = index.query(&m, &scope, MAX_RESULTS, &cancelled) else { return };
            if !cancelled() {
                let _ = tx.send(Msg::Results { generation, rows, total, first: t.elapsed(), complete: true });
                ctx.request_repaint();
            }
        });
    }

    /// Give arriving rows ids, sort, and queue them for metadata.
    fn add_result_rows(&mut self, mut rows: Vec<Row>) {
        let Some(r) = self.results.as_mut() else { return };
        let base = r.rows.len() as u32;
        for (i, row) in rows.iter_mut().enumerate() {
            row.id = base + i as u32;
        }
        let known = rows.iter().filter(|row| row.size != 0 || row.mtime != 0).count();
        r.filled.extend(rows.iter().map(|row| row.size != 0 || row.mtime != 0));
        r.filled_count += known;
        if let Some(b) = &r.backfill {
            let _ = b.send(Fill::Add(rows.iter().filter(|row| row.size == 0 && row.mtime == 0).map(|row| (row.id, (row.dir.clone().unwrap_or_else(|| self.cwd.as_slice().into()), row.name.clone()))).collect()));
        }
        r.rows.extend(rows);
        let keep = self.focus_path();
        let (key, desc) = (self.sort, self.descending);
        if let Some(r) = self.results.as_mut() {
            fsys::sort_rows(&mut r.rows, key, desc);
            r.reindex();
        }
        self.rebuild_view(keep);
    }

    fn apply_meta(&mut self, filled: Vec<(u32, u64, i64)>) {
        let Some(r) = self.results.as_mut() else { return };
        for (id, size, mtime) in filled {
            let (Some(&p), Some(f)) = (r.pos.get(id as usize), r.filled.get_mut(id as usize)) else { continue };
            let row = &mut r.rows[p as usize];
            row.size = if row.is_dir() { 0 } else { size };
            row.mtime = mtime;
            if !*f {
                *f = true;
                r.filled_count += 1;
            }
        }
        if r.all.is_some() && r.details.is_none() && r.filled_count >= r.rows.len() {
            r.details = Some(r.started.elapsed());
            r.backfill = None;
            // Order by size or date only once every row has one.
            if matches!(self.sort, SortKey::Size | SortKey::Modified) {
                self.resort();
            }
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Listing { generation, result, took } if generation == self.listing_gen => {
                    self.loading = false;
                    self.listed_in = took;
                    match result {
                        Ok(rows) => {
                            self.rows = rows;
                            if !self.in_results() {
                                let keep = self.pending_keep.take();
                                fsys::sort_rows(&mut self.rows, self.sort, self.descending);
                                self.rebuild_view(keep);
                                self.scroll_to = self.focus;
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Changes { generation, changes } if generation == self.listing_gen && !self.loading => self.apply_changes(changes),
                Msg::Built(Some(index)) => {
                    let index = Arc::new(index);
                    self.build = None;
                    self.indexes.retain(|i| !index.covers(&i.root));
                    self.indexes.insert(0, index.clone());
                    self.indexes.truncate(MAX_INDEXES);
                    if let (Some(m), true) = (self.matcher.clone(), index.covers(&self.cwd)) {
                        self.run_query(index, m);
                    }
                }
                Msg::Built(None) => {}
                Msg::Results { generation, rows, total, first, complete } if generation == self.query_gen.load(Relaxed) => {
                    self.searching = false;
                    let (fill_tx, fill_rx) = channel();
                    let (current, tx, ctx) = (self.query_gen.clone(), self.tx.clone(), self.ctx.clone());
                    thread::spawn(move || backfill(generation, current, fill_rx, tx, ctx));
                    self.results = Some(Results {
                        rows: Vec::new(),
                        total,
                        started: Instant::now() - first,
                        first,
                        all: complete.then_some(first),
                        filled: Vec::new(),
                        filled_count: 0,
                        details: None,
                        pos: Vec::new(),
                        backfill: Some(fill_tx),
                        prioritized: Vec::new(),
                    });
                    self.add_result_rows(rows);
                    self.focus = None;
                    self.scroll_to = Some(0);
                    if complete {
                        self.apply_meta(Vec::new());
                    }
                }
                Msg::MoreRows { generation, rows, complete } if generation == self.query_gen.load(Relaxed) => {
                    self.add_result_rows(rows);
                    if let (true, Some(r)) = (complete, self.results.as_mut()) {
                        r.all = Some(r.started.elapsed());
                    }
                    self.apply_meta(Vec::new());
                }
                Msg::Meta { generation, filled } if generation == self.query_gen.load(Relaxed) => self.apply_meta(filled),
                Msg::Fonts(name) => self.font = name,
                Msg::Daemon(st) => {
                    self.daemon_failed = false;
                    self.daemon = st;
                }
                Msg::DaemonFailed { generation } if generation == self.query_gen.load(Relaxed) => {
                    self.daemon_failed = true;
                    self.start_search();
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ input

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if pressed(Modifiers::COMMAND, Key::F) || pressed(Modifiers::NONE, Key::F3) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique("search_bar")));
        }
        if pressed(Modifiers::COMMAND, Key::L) {
            self.edit_path();
        }
        if pressed(Modifiers::NONE, Key::F5) {
            self.refresh();
        }
        if pressed(Modifiers::COMMAND, Key::H) {
            self.show_hidden = !self.show_hidden;
            let keep = self.focus_path();
            self.rebuild_view(keep);
        }
        if pressed(Modifiers::ALT, Key::ArrowLeft) {
            self.go_back();
        }
        if pressed(Modifiers::ALT, Key::ArrowRight) {
            self.go_forward();
        }
        if pressed(Modifiers::ALT, Key::ArrowUp) {
            self.go_up();
        }
        if typing {
            if pressed(Modifiers::NONE, Key::Escape) {
                self.path_edit = None;
                if !self.query.is_empty() {
                    self.query.clear();
                    self.clear_search();
                }
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            return;
        }
        if pressed(Modifiers::NONE, Key::Backspace) && !self.in_results() {
            self.go_up();
        }
        if pressed(Modifiers::NONE, Key::Escape) && !self.query.is_empty() {
            self.query.clear();
            self.clear_search();
        }
        if pressed(Modifiers::NONE, Key::Enter)
            && let Some(vi) = self.focus
        {
            self.open(vi);
        }
        if pressed(Modifiers::COMMAND, Key::A) {
            for i in 0..self.view.len() {
                let idx = self.view[i];
                self.source_mut()[idx].sel = true;
            }
        }
        let n = self.view.len();
        if n == 0 {
            return;
        }
        let page = ((self.view_h / ROW_H) as usize).max(1);
        let cur = self.focus;
        let moves = [
            (Key::ArrowDown, cur.map_or(0, |f| (f + 1).min(n - 1))),
            (Key::ArrowUp, cur.map_or(0, |f| f.saturating_sub(1))),
            (Key::PageDown, cur.map_or(0, |f| (f + page).min(n - 1))),
            (Key::PageUp, cur.map_or(0, |f| f.saturating_sub(page))),
            (Key::Home, 0),
            (Key::End, n - 1),
        ];
        for (key, to) in moves {
            if pressed(Modifiers::SHIFT, key) {
                let a = self.anchor.unwrap_or(to);
                self.select_range(a, to);
                self.scroll_to = Some(to);
            } else if pressed(Modifiers::NONE, key) {
                self.select_only(to);
                self.scroll_to = Some(to);
            }
        }
        let typed: String = ctx.input(|i| {
            i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
        });
        if !typed.is_empty() && !ctx.input(|i| i.modifiers.command || i.modifiers.alt) {
            self.type_to_find(typed);
        }
    }

    /// Jump to the best-ranked name for what has been typed (shared ranking
    /// with the Windows build: prefix, word start, substring, fuzzy).
    fn type_to_find(&mut self, text: String) {
        let mut q = match self.typeahead.take() {
            Some(t) if t.last.elapsed() < TYPEAHEAD_RESET => t.text,
            _ => String::new(),
        };
        q.push_str(&text);
        let best = (0..self.view.len())
            .filter_map(|vi| self.row(vi).and_then(|r| typeahead::score(&q, &r.display()).map(|m| (vi, m))))
            .max_by(|a, b| a.1.score.cmp(&b.1.score).then(b.0.cmp(&a.0)));
        let (positions, path) = match best {
            Some((vi, m)) => {
                self.select_only(vi);
                self.scroll_to = Some(vi);
                (m.positions, self.focus_path().unwrap_or_default())
            }
            None => (Vec::new(), Vec::new()),
        };
        self.typeahead = Some(TypeAhead { text: q, last: Instant::now(), positions, path });
        self.ctx.request_repaint_after(TYPEAHEAD_RESET);
    }

    fn run_action(&mut self, action: Option<Action>) {
        match action {
            Some(Action::Open(vi)) => self.open(vi),
            Some(Action::OpenLocation(vi)) => {
                if let Some(row) = self.row(vi) {
                    let path = row.full_path(&self.cwd);
                    if let Some(dir) = row.dir.as_ref().map(|d| d.to_vec()) {
                        self.navigate(dir, true);
                        self.pending_keep = Some(path);
                    }
                }
            }
            Some(Action::CopyPath(vi)) => {
                if let Some(row) = self.row(vi) {
                    self.ctx.copy_text(String::from_utf8_lossy(&row.full_path(&self.cwd)).into_owned());
                }
            }
            Some(Action::Sort(key)) => {
                if self.sort == key {
                    self.descending = !self.descending;
                } else {
                    self.sort = key;
                    self.descending = matches!(key, SortKey::Modified | SortKey::Size);
                }
                self.resort();
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ panels

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let nav = |ui: &mut Ui, enabled: bool, icon: &str, tip: &str| {
                ui.add_enabled(enabled, Button::new(icon).frame(false).min_size(egui::vec2(22.0, 22.0))).on_hover_text(tip).clicked()
            };
            if nav(ui, !self.back.is_empty(), "⬅", "Back (Alt+←)") {
                self.go_back();
            }
            if nav(ui, !self.forward.is_empty(), "➡", "Forward (Alt+→)") {
                self.go_forward();
            }
            if nav(ui, fsys::parent(&self.cwd).is_some(), "⬆", "Up (Alt+↑, Backspace)") {
                self.go_up();
            }
            if nav(ui, true, "⟳", "Refresh (F5)") {
                self.refresh();
            }

            let search_w = 240.0;
            let path_w = (ui.available_width() - search_w - 60.0).max(120.0);
            match &mut self.path_edit {
                Some(text) => {
                    let id = egui::Id::unique("path_edit");
                    let resp = ui.add(TextEdit::singleline(text).desired_width(path_w).id(id));
                    if std::mem::take(&mut self.path_select) {
                        resp.request_focus();
                        let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
                        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        state.cursor.set_char_range(Some(all));
                        state.store(ui.ctx(), id);
                    } else if resp.lost_focus() {
                        // Enter submits; Escape or clicking elsewhere cancels.
                        let submitted = ui.input(|i| i.key_pressed(Key::Enter));
                        let target = fsys::resolve_input(text, &self.cwd);
                        self.path_edit = None;
                        if submitted {
                            if sys::is_dir(&target) {
                                self.navigate(target, true);
                            } else {
                                self.error = Some(format!("{}: not a folder", String::from_utf8_lossy(&target)));
                            }
                        }
                    }
                }
                None => {
                    ui.allocate_ui(egui::vec2(path_w, 22.0), |ui| self.breadcrumbs(ui));
                }
            }

            let hint = if self.daemon_covers(&self.cwd) || self.indexes.iter().any(|i| i.covers(&self.cwd)) { "Search (indexed)" } else { "Search" };
            let resp = ui.add(TextEdit::singleline(&mut self.query).id(egui::Id::unique("search_bar")).hint_text(hint).desired_width(search_w));
            if resp.changed() {
                self.query_dirty = Some(Instant::now());
            }
            if ui.selectable_label(self.regex, ".*").on_hover_text("Regular expression").clicked() {
                self.regex = !self.regex;
                if !self.query.is_empty() {
                    self.query_dirty = Some(Instant::now());
                }
            }
        });
    }

    fn edit_path(&mut self) {
        self.path_edit = Some(String::from_utf8_lossy(&self.cwd).into_owned());
        self.path_select = true;
    }

    fn breadcrumbs(&mut self, ui: &mut Ui) {
        let mut action = None;
        let resp = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let mut acc = Vec::new();
            let parts: Vec<&[u8]> = self.cwd.split(|&b| b == b'/').filter(|p| !p.is_empty()).collect();
            if ui.add(Button::new("/").frame(false)).clicked() {
                action = Some(b"/".to_vec());
            }
            for (i, p) in parts.iter().enumerate() {
                acc.push(b'/');
                acc.extend_from_slice(p);
                if i > 0 {
                    ui.weak("›");
                }
                if ui.add(Button::new(String::from_utf8_lossy(p)).frame(false)).clicked() {
                    action = Some(acc.clone());
                }
            }
            let rest = ui.available_rect_before_wrap();
            if rest.width() > 0.0 && ui.allocate_rect(rest, Sense::click()).clicked() {
                self.edit_path();
            }
        });
        resp.response.on_hover_text("Click empty space or press Ctrl+L to type a path");
        if let Some(p) = action {
            self.navigate(p, true);
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let mut target = None;
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.weak("Places");
            for (label, path) in &self.places {
                let icon = if label == "Home" { "🏠" } else { "🗀" };
                if ui.selectable_label(*path == self.cwd, format!("{icon} {label}")).clicked() {
                    target = Some(path.clone());
                }
            }
            ui.add_space(8.0);
            ui.weak("Volumes");
            for (label, path) in &self.volumes {
                let r = ui.selectable_label(*path == self.cwd, format!("🖴 {label}"));
                if r.on_hover_text(String::from_utf8_lossy(path)).clicked() {
                    target = Some(path.clone());
                }
            }
        });
        if let Some(t) = target {
            self.navigate(t, true);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let selected = self.view.iter().filter(|&&i| self.source()[i].sel).count();
            let sel = if selected > 0 { format!(" · {selected} selected") } else { String::new() };
            if let Some(e) = &self.search_error {
                ui.colored_label(RED, e);
            } else if let Some(b) = &self.build {
                ui.label(format!("Indexing {} … {} entries", String::from_utf8_lossy(&b.root), b.seen.load(Relaxed)));
            } else if let Some(r) = &self.results {
                let msec = |d: Duration| format!("{:.1} ms", d.as_secs_f64() * 1e3);
                let shown = match r.all {
                    None => format!(" · receiving {} rows", r.rows.len()),
                    Some(_) if r.total > r.rows.len() => format!(" (showing {})", r.rows.len()),
                    Some(_) => String::new(),
                };
                let all = r.all.map_or(String::new(), |d| format!(" · all rows {}", msec(d)));
                let details = match r.details {
                    Some(d) => format!(" · details {}", msec(d)),
                    None if !r.rows.is_empty() => format!(" · details {}%", r.filled_count * 100 / r.rows.len()),
                    None => String::new(),
                };
                ui.label(format!("{} matches{shown}{sel} · first rows {}{all}{details}", r.total, msec(r.first)));
            } else if let Some(e) = &self.error {
                ui.colored_label(RED, e);
            } else if self.loading {
                ui.label("Loading…");
            } else {
                let hidden = self.rows.len() - self.view.len();
                let hidden = if hidden > 0 && !self.show_hidden { format!(" ({hidden} hidden, Ctrl+H)") } else { String::new() };
                ui.label(format!("{} items{hidden}{sel} · listed in {:.1} ms", self.view.len(), self.listed_in.as_secs_f64() * 1e3));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("X11 prototype");
                if let (true, Some(d)) = (self.daemon_covers(&self.cwd), &self.daemon) {
                    let age = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |n| n.as_secs().saturating_sub(d.verified_ns / 1_000_000_000));
                    let live = if d.watched > 0 { "live" } else { "swept" };
                    let paused = if d.paused.is_empty() { String::new() } else { format!(", paused: {}", d.paused) };
                    ui.weak(format!("index: daemon, {} entries, {live}, verified {age} s ago{paused} ·", d.entries));
                } else if let Some(i) = self.indexes.iter().find(|i| i.covers(&self.cwd)) {
                    ui.weak(format!(
                        "index: {} entries, {} dirs, {:.0} MB, crawled in {:.2} s ·",
                        i.entries,
                        i.dirs,
                        i.bytes() as f64 / 1048576.0,
                        i.build_time.as_secs_f64()
                    ));
                }
            });
        });
    }

    fn table(&mut self, ui: &mut Ui) {
        let results = self.in_results();
        let width = ui.available_width();
        let (date_w, size_w, type_w) = (140.0, 90.0, if results { 0.0 } else { 120.0 });
        let flex = (width - date_w - size_w - type_w).max(120.0);
        let (name_w, folder_w) = if results { (flex * 0.45, flex * 0.55) } else { (flex, 0.0) };
        let mut cols: Vec<(&str, Option<SortKey>, f32, f32, bool)> = Vec::new();
        let mut x = 0.0;
        let mut add = |label, key, w, right| {
            cols.push((label, key, x, w, right));
            x += w;
        };
        add("Name", Some(SortKey::Name), name_w, false);
        if results {
            add("Folder", None, folder_w, false);
        }
        add("Date modified", Some(SortKey::Modified), date_w, false);
        if !results {
            add("Type", Some(SortKey::Type), type_w, false);
        }
        add("Size", Some(SortKey::Size), size_w, true);

        let font = FontId::proportional(13.5);
        let visuals = ui.visuals().clone();
        let mods = ui.input(|i| i.modifiers);
        let mut action = None;

        let (hrect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::hover());
        for (i, &(label, key, cx, w, right)) in cols.iter().enumerate() {
            let rect = Rect::from_min_size(egui::pos2(hrect.left() + cx, hrect.top()), egui::vec2(w, ROW_H));
            let resp = ui.interact(rect, ui.scope_id().with(("hdr", i)), Sense::click());
            if resp.hovered() && key.is_some() {
                ui.painter().rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
            }
            let arrow = match key {
                Some(k) if k == self.sort => if self.descending { " ▼" } else { " ▲" },
                _ => "",
            };
            cell(ui.painter(), rect, &format!("{label}{arrow}"), &font, visuals.strong_text_color(), right);
            if resp.clicked()
                && let Some(k) = key
            {
                action = Some(Action::Sort(k));
            }
        }
        ui.separator();

        if self.view.is_empty() {
            let msg = if self.loading || self.searching || self.build.is_some() {
                "Working…"
            } else if self.error.is_some() && !results {
                "Folder could not be read"
            } else if self.matcher.is_some() {
                "No matches"
            } else {
                "This folder is empty"
            };
            let rest = ui.available_rect_before_wrap();
            ui.painter().text(rest.center_top() + egui::vec2(0.0, 30.0), Align2::CENTER_CENTER, msg, font, visuals.weak_text_color());
            self.run_action(action);
            return;
        }

        let mut area = ScrollArea::vertical().auto_shrink(false).id_salt("table");
        if let Some(i) = self.scroll_to.take() {
            let top = i as f32 * ROW_H;
            let mut y = self.scroll_y;
            if top < y {
                y = top;
            } else if top + ROW_H > y + self.view_h {
                y = top + ROW_H - self.view_h;
            }
            area = area.vertical_scroll_offset(y);
        }
        let typeahead_hit = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET && !t.positions.is_empty());
        let mut clicked: Option<(usize, bool)> = None; // (view index, double)
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut visible = 0..0;
        let out = area.show_rows(ui, ROW_H, self.view.len(), |ui, range| {
            visible = range.clone();
            for vi in range {
                let Some(row) = self.row(vi) else { continue };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click());
                let painter = ui.painter();
                if row.sel {
                    painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                }
                if self.focus == Some(vi) && !row.sel {
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, visuals.weak_text_color()), egui::StrokeKind::Inside);
                }
                let mut color = if row.sel { visuals.selection.stroke.color } else { visuals.text_color() };
                let mut weak = if row.sel { color } else { visuals.weak_text_color() };
                if row.is_hidden() {
                    color = color.gamma_multiply(0.55);
                    weak = weak.gamma_multiply(0.55);
                }
                let icon = match (row.is_dir(), row.is_link()) {
                    (true, false) => "🗀",
                    (true, true) => "↗",
                    (false, true) => "↗",
                    (false, false) => "🗋",
                };
                let full = row.full_path(&self.cwd);
                // Sizes and dates of results stay blank until backfilled.
                let known = self.results.as_ref().is_none_or(|r| r.is_filled(row));
                for &(label, _, cx, w, right) in &cols {
                    let r = Rect::from_min_size(egui::pos2(rect.left() + cx, rect.top()), egui::vec2(w, ROW_H));
                    match label {
                        "Name" => match typeahead_hit.filter(|t| t.path == full) {
                            Some(t) => highlighted_cell(painter, r, &format!("{icon}  "), &row.display(), &t.positions, &font, color, &visuals),
                            None => cell(painter, r, &format!("{icon}  {}", row.display()), &font, color, false),
                        },
                        "Folder" => cell(painter, r, &String::from_utf8_lossy(row.dir.as_deref().unwrap_or(&self.cwd)), &font, weak, false),
                        "Date modified" if known => cell(painter, r, &sys::format_time(row.mtime), &font, weak, false),
                        "Type" => cell(painter, r, &row.type_name(), &font, weak, false),
                        "Size" if known && !row.is_dir() => cell(painter, r, &fsys::format_size(row.size), &font, weak, right),
                        _ => {}
                    }
                }
                if resp.double_clicked() {
                    clicked = Some((vi, true));
                } else if resp.clicked() || resp.secondary_clicked() && !row.sel {
                    clicked = Some((vi, false));
                }
                let is_dir = row.is_dir();
                resp.context_menu(|ui| {
                    if ui.button(if is_dir { "Open folder" } else { "Open" }).clicked() {
                        action = Some(Action::Open(vi));
                        ui.close();
                    }
                    if results && ui.button("Open file location").clicked() {
                        action = Some(Action::OpenLocation(vi));
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        action = Some(Action::CopyPath(vi));
                        ui.close();
                    }
                });
            }
        });
        self.scroll_y = out.state.clamped_offset().y;
        self.prioritize_visible(visible);
        self.view_h = out.inner_rect.height();
        match clicked {
            Some((vi, true)) => action = Some(Action::Open(vi)),
            Some((vi, false)) if mods.shift => {
                let a = self.anchor.unwrap_or(vi);
                self.select_range(a, vi);
            }
            Some((vi, false)) if mods.command => self.toggle(vi),
            Some((vi, false)) => self.select_only(vi),
            None => {}
        }
        self.typeahead_overlay(ui);
        self.run_action(action);
    }

    /// Ask the backfill worker for the rows on screen first.
    fn prioritize_visible(&mut self, visible: std::ops::Range<usize>) {
        let missing: Vec<u32> = visible
            .filter_map(|vi| self.row(vi))
            .filter(|row| self.results.as_ref().is_some_and(|r| !r.is_filled(row)))
            .map(|row| row.id)
            .collect();
        if let Some(r) = self.results.as_mut()
            && !missing.is_empty()
            && missing != r.prioritized
            && let Some(b) = &r.backfill
        {
            let _ = b.send(Fill::Prioritize(missing.clone()));
            r.prioritized = missing;
        }
    }

    fn typeahead_overlay(&self, ui: &Ui) {
        let Some(t) = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET) else { return };
        let rect = ui.clip_rect();
        let color = if t.positions.is_empty() { RED } else { ui.visuals().strong_text_color() };
        let pos = rect.right_bottom() - egui::vec2(12.0, 10.0);
        let galley = ui.painter().layout_no_wrap(t.text.clone(), FontId::proportional(14.0), color);
        let bg = Rect::from_min_max(pos - galley.size() - egui::vec2(8.0, 4.0), pos + egui::vec2(4.0, 4.0));
        ui.painter().rect_filled(bg, 4.0, ui.visuals().extreme_bg_color);
        ui.painter().galley(bg.min + egui::vec2(6.0, 4.0), galley, color);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.drain_messages();
        if let Some(t) = self.query_dirty {
            let waited = t.elapsed();
            if waited >= QUERY_DEBOUNCE {
                self.start_search();
            } else {
                ui.ctx().request_repaint_after(QUERY_DEBOUNCE - waited);
            }
        }
        let ctx = ui.ctx().clone();
        self.handle_keys(&ctx);
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("sidebar").default_size(190.0).resizable(true).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.table(ui));
        if self.build.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let title = format!("{} — FEx", String::from_utf8_lossy(fsys::base_name(&self.cwd)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
    }
}

/// A plain, still look (as on Windows): no animations or shadows, so egui
/// only redraws on input.
fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.animation_time = 0.0;
        s.visuals.window_shadow = egui::Shadow::NONE;
        s.visuals.popup_shadow = egui::Shadow::NONE;
        s.spacing.scroll = egui::style::ScrollStyle::solid();
    });
}

fn cell(painter: &egui::Painter, rect: Rect, text: &str, font: &FontId, color: Color32, right: bool) {
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    let p = painter.with_clip_rect(clip);
    if right {
        p.text(egui::pos2(clip.right(), rect.center().y), Align2::RIGHT_CENTER, text, font.clone(), color);
    } else {
        p.text(egui::pos2(clip.left(), rect.center().y), Align2::LEFT_CENTER, text, font.clone(), color);
    }
}

/// Name cell with type-to-find matches emphasized.
#[allow(clippy::too_many_arguments)]
fn highlighted_cell(painter: &egui::Painter, rect: Rect, prefix: &str, name: &str, positions: &[usize], font: &FontId, color: Color32, v: &egui::Visuals) {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    job.append(prefix, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    for (i, c) in name.chars().enumerate() {
        let hit = positions.contains(&i);
        let format = TextFormat {
            font_id: font.clone(),
            color: if hit { v.strong_text_color() } else { color },
            underline: if hit { Stroke::new(1.0, color) } else { Stroke::NONE },
            ..Default::default()
        };
        job.append(c.encode_utf8(&mut [0; 4]), 0.0, format);
    }
    let galley = painter.layout_job(job);
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    painter.with_clip_rect(clip).galley(egui::pos2(clip.left(), rect.center().y - galley.size().y / 2.0), galley, color);
}
