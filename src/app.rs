//! FEx user interface. All filesystem work happens on background threads;
//! the UI thread only drains a message channel and paints visible rows.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Button, Color32, FontId, Key, Layout, Modifiers, PointerButton, Pos2, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, ViewportCommand,
};

use crate::fsys::{self, Row, SortKey};
use crate::history::{Entry, History, Step};
use crate::index::{self, Build, Freshness, Index, Live, Matcher};
use crate::ops::{self, Conflict, Job, Kind, Phase};
use crate::props::{self, Props};
use crate::watch::{self, Change, Watcher};
use crate::session::Session;
use crate::winsvc::proto as service;
use crate::{archive, clip, fileop, net, oledrag, oledrop, recyclebin, typeahead, win};
use windows_sys::Win32::Foundation::HWND;

#[path = "netview.rs"]
mod netview;
#[path = "driveview.rs"]
mod driveview;

/// Search results materialized for display; the total count is still exact.
const MAX_RESULTS: usize = 250_000;
/// Rows in the first chunk of an index service reply: more than a window shows.
const SCREENFUL: u32 = 200;
/// Queries are cancelled as soon as a newer one starts, so this only spares
/// the work of a keystroke typed in quick succession.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(30);
/// Search indexes kept in memory (and up to date) at once.
const MAX_INDEXES: usize = 4;
/// Changed indexes are saved this often (and on exit).
const SAVE_EVERY: Duration = Duration::from_secs(120);
/// After an index changes, the search shown re-runs this much later.
const REQUERY_DELAY: Duration = Duration::from_millis(400);
/// An index rebuilt more recently than this is kept when changes are lost.
const STALE_GRACE: Duration = Duration::from_secs(60);
const ROW_H: f32 = 22.0;
/// Blank rows kept after the last item, so a full folder still has empty
/// space to right-click (New, Paste) or start a selection box from.
const BLANK_ROWS: usize = 3;
const RED: Color32 = Color32::from_rgb(230, 80, 80);
/// Pseudo-location for the network page (shown in the path bar and history).
const NETWORK: &str = "Network";
const RECENT: &str = "Recent";
/// The F1 sheet; an empty row separates groups.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Open"),
    ("Ctrl+S", "Run as administrator"),
    ("Alt+← / Alt+→", "Back / forward"),
    ("Alt+↑, Backspace", "Up"),
    ("Ctrl+L", "Type a path"),
    ("Ctrl+F, F3", "Search"),
    ("F5", "Refresh, rebuild the search index"),
    ("", ""),
    ("Ctrl+C / Ctrl+X / Ctrl+V", "Copy / cut / paste"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
    ("F2", "Rename"),
    ("Del, Shift+Del", "Recycle, delete permanently"),
    ("Ctrl+Shift+N", "New folder"),
    ("Ctrl+Alt+N", "New text file (right-click empty space or ➕ for other types)"),
    ("Alt+Enter", "Properties"),
    ("Ctrl+A", "Select all"),
    ("", ""),
    ("Ctrl+T / Ctrl+W", "New tab / close tab"),
    ("Ctrl+Tab, Ctrl+1–9", "Switch tabs"),
    ("Ctrl+N", "New window"),
    ("Ctrl+H", "Show hidden files"),
    ("Middle-click", "Open a folder in a new tab"),
];

/// Hovering a dragged item on a tab this long switches to it.
const TAB_SPRING: Duration = Duration::from_millis(600);
/// Type-to-find forgets what was typed after this pause.
const TYPEAHEAD_RESET: Duration = Duration::from_millis(1200);

/// Keys typed into the file list to jump to a name.
struct TypeAhead {
    text: String,
    last: Instant,
    /// Highlighted characters of the matched row.
    path: String,
    positions: Vec<usize>,
}

enum Msg {
    /// `sorted`: the order the rows already have.
    Listing { generation: u64, result: Result<Vec<Row>, String>, elapsed: Duration, sorted: (SortKey, bool) },
    DirChanges { generation: u64, changes: Vec<Change> },
    Hits { generation: u64, rows: Vec<Row> },
    IndexReady { build: Arc<Build>, index: Option<Index> },
    /// A saved index covering `scope` was read from disk (or there was none).
    IndexLoaded { scope: String, index: Option<Index> },
    /// An index rooted here changed; a search over it is out of date.
    IndexChanged(Arc<str>),
    /// An index rooted here lost track of changes and needs a new crawl.
    IndexStale(Arc<str>),
    QueryDone { generation: u64, rows: Vec<Row>, total: usize, elapsed: Duration, sorted: (SortKey, bool) },
    /// Latest status of the index service (`None`: not running).
    Service(Option<service::Status>),
    /// More rows of a search answered by the index service; `done` on the last.
    ServiceMore { generation: u64, rows: Vec<Row>, done: bool },
    /// The index service couldn't answer. `definitive`: it said it can't
    /// (drive not covered, first index still building), so search with the
    /// app's own index; else it didn't answer twice running, so report it
    /// rather than start a crawl (the status poll notices a stopped service).
    ServiceFailed { generation: u64, definitive: bool },
    /// Segoe UI Emoji, loaded after startup (it's 13 MB).
    EmojiFont(Vec<u8>),
    /// `target`: what the letter pointed to when asked; a reply about a
    /// drive that has since been swapped is dropped.
    DriveSpace { path: String, target: Option<String>, is_network: bool, queried: bool, ejectable: bool, result: Option<(u64, u64)> },
    DriveEjected(String),
    /// Drive letters and volumes changed (or were read on request).
    Volumes(crate::volumes::Snapshot),
    /// An "Assign drive letter" finished.
    LetterAssigned { result: Result<(), String>, letter: char },
    ShellNewItems(Vec<crate::shellnew::Item>),
    VirtualDropDone { dir: String, created: Vec<String>, errors: Vec<String> },
    RecentChanged,
    Navigate(String),
    Created { dir: String, name: String },
    Renamed { old_path: String, new_name: String },
    /// Items sent to the Recycle Bin (undoable).
    Recycled(Vec<String>),
    /// Archive contents written to disk; optionally open a folder afterwards.
    Extracted { created: Vec<String>, open: Option<String>, errors: Vec<String> },
    /// An undo (or redo) step that didn't need a transfer job finished.
    HistoryDone { entry: Entry, undo: bool, result: Result<(), String> },
    Error(String),
    Connections(Vec<net::Connection>),
    Shares { server: String, result: Result<Vec<String>, String> },
    /// A connect/map/disconnect finished; optionally open a path afterwards.
    NetDone { result: Result<(), String>, open: Option<String> },
}

#[derive(Clone)]
enum Action {
    Click { vi: usize, ctrl: bool, shift: bool },
    Secondary(usize),
    Open(usize),
    OpenInNewTab(String),
    OpenInNewWindow(String),
    OpenArchive(usize),
    ExtractArchive(usize),
    /// Extract the selected archive members next to the archive.
    ExtractSelected,
    CreateLink(usize),
    EjectDrive(String),
    /// Give a volume another drive letter; `current` is its letter, if any.
    ChangeLetter { volume: String, name: String, current: Option<char>, why: Option<String> },
    RunAs(usize),
    OpenLocation(usize),
    CopyPath(usize),
    Sort(SortKey),
    Clip { cut: bool },
    Paste(Option<String>),
    Rename(usize),
    Delete { permanent: bool },
    Properties,
    /// Properties of the open folder itself.
    FolderProperties,
    NewFolder,
    /// An empty text file (Ctrl+Alt+N).
    NewFile,
    /// Create a file from the ShellNew template at this index.
    NewItem(usize),
    LoadShellNew,
    Undo,
    Redo,
    ClearSelection,
    StartBand { origin: Pos2, additive: bool },
    StartDrag(usize),
}

/// Rubber-band selection in progress. `origin_y` is in content coordinates
/// so the band keeps its anchor while the list scrolls.
struct Band {
    origin_x: f32,
    origin_y: f32,
    base: Vec<bool>,
}

/// Items being dragged to a folder.
struct ItemDrag {
    paths: Vec<String>,
    same_volume: Option<(String, bool)>,
}

/// A tab being dragged: reordered within the strip, or torn off when it leaves the window.
struct TabDrag {
    index: usize,
}

struct Rename {
    path: String,
    original: String,
    text: String,
    focused: bool,
}

#[derive(Clone)]
struct Tab {
    location: String,
    back: Vec<String>,
    forward: Vec<String>,
}

struct DriveSpace {
    is_network: bool,
    queried: bool,
    /// Can be safely removed (USB stick, card, optical, external disk).
    ejectable: bool,
    capacity: Option<(u64, u64)>,
    checked: Instant,
}

/// What a running transfer job means for the undo history when it finishes.
enum Pending {
    /// A new operation: record what it created.
    Record,
    /// Undoing this entry; it moves to the redo stack on success.
    Undo(Entry),
    Redo,
}

pub struct FexApp {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    /// Signed updates from GitHub releases (checked at start, then daily).
    updater: crate::update::Updater,

    cwd: String,
    path_text: String,
    back: Vec<String>,
    forward: Vec<String>,
    tabs: Vec<Tab>,
    active_tab: usize,

    listing: Vec<Row>,
    listing_gen: u64,
    loading: bool,
    list_time: Duration,
    error: Option<String>,
    /// Status-bar message that isn't an error ("You can now remove E:\").
    notice: Option<String>,
    watcher: Option<Watcher>,
    /// Changes that arrived while the listing was still loading.
    early_changes: Vec<Change>,

    query: String,
    regex_mode: bool,
    recursive: bool,
    query_dirty: Option<Instant>,
    matcher: Option<Arc<Matcher>>,
    query_error: Option<String>,
    generation: Arc<AtomicU64>,
    results: Vec<Row>,
    results_total: usize,
    searching: bool,
    search_time: Option<Duration>,
    /// Indexes in use, most recently used first.
    indexes: Vec<Arc<Live>>,
    /// A saved index covering this scope is being read from disk.
    opening: Option<String>,
    /// Re-run the search shown at this time (its index changed).
    requery_at: Option<Instant>,
    last_save: Instant,
    build: Option<Arc<Build>>,

    show_hidden: bool,
    sort: SortKey,
    descending: bool,
    /// Indices into the active source (`listing` or `results`) after filtering.
    view: Vec<u32>,
    /// Keyboard cursor and shift-selection anchor (view indices).
    focus: Option<usize>,
    anchor: Option<usize>,
    /// Name to select when it appears in the listing.
    pending_select: Option<Box<str>>,
    pending_rename: bool,
    scroll_to: Option<usize>,
    scroll_set: Option<f32>,
    scroll_y: f32,
    view_h: f32,
    rows_rect: Rect,

    band: Option<Band>,
    drag: Option<ItemDrag>,
    drop_target: Option<String>,
    renaming: Option<Rename>,
    confirm_delete: Option<(Vec<String>, bool)>,
    props: Option<Props>,

    jobs: Vec<Arc<Job>>,
    show_errors: Option<Arc<Job>>,
    /// Lowercased paths currently cut to the clipboard (drawn dimmed).
    cut_marks: HashSet<String>,
    cut_seq: u32,
    /// Cut-paste job whose completion should clear the clipboard.
    cut_job: Option<(Arc<Job>, u32)>,
    history: History,
    history_jobs: Vec<(Arc<Job>, Pending)>,
    /// Files dropped from other apps (None if the drop target failed to register).
    drops: Option<oledrop::Inbox>,
    hwnd: Option<HWND>,
    /// Folders under parts of the window this frame, for drops from other apps.
    zones: Vec<(Rect, String)>,
    tab_drag: Option<TabDrag>,
    /// Where each tab was drawn last frame.
    tab_rects: Vec<Rect>,
    /// A dragged item has rested on this tab since then; it opens after a moment.
    tab_spring: Option<(usize, Instant)>,
    /// An OLE drag swallowed the mouse release; tell egui on the next frame.
    release_pointer: bool,
    /// Normal (not maximized) window placement in points: outer top-left, inner size.
    placement: Option<([f32; 2], [f32; 2])>,
    maximized: bool,
    /// The path bar shows the editable text instead of breadcrumbs.
    path_editing: bool,

    drives: Vec<String>,
    /// What each letter is in this session, and volumes no letter reaches.
    volumes: crate::volumes::Snapshot,
    letter_dialog: Option<driveview::LetterDialog>,
    drive_space: HashMap<String, DriveSpace>,
    drive_space_pending: HashSet<String>,
    report_network_space: bool,
    help_open: bool,
    shell_new_items: Option<Vec<crate::shellnew::Item>>,
    shell_new_loading: bool,
    quick: Vec<(&'static str, String)>,
    /// Remembered servers (`\\name`), persisted in %APPDATA%\FEx\servers.txt.
    servers: Vec<String>,
    typeahead: Option<TypeAhead>,
    net: netview::NetState,
    /// FEx runs as administrator: nothing it does will ask for UAC approval.
    elevated: bool,
    /// The index service, when it's running: it answers searches on the
    /// drives it indexes, so the app doesn't crawl them itself.
    service: Option<service::Status>,
    /// It failed a search; use the app's own index until its next status.
    service_failed: bool,
}

impl FexApp {
    /// `restore`: reopen the session's tabs (no location was given).
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<String>, session: &Session, restore: bool) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        install_system_fonts(&cc.egui_ctx);
        install_style(&cc.egui_ctx);
        let tabs =if restore { session.tabs.clone() } else { Vec::new() };
        let start = start.or_else(|| tabs.get(session.active).cloned());
        let mut app = Self::with_context(cc.egui_ctx.clone(), start);
        app.apply_session(session, tabs);
        app.updater = {
            let ctx = cc.egui_ctx.clone();
            crate::update::Updater::start(move || ctx.request_repaint())
        };
        let hwnd = cc.window_handle().ok().and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get() as HWND),
            _ => None,
        });
        app.hwnd = hwnd;
        match hwnd.map(|hwnd| oledrop::register(hwnd, cc.egui_ctx.clone())) {
            Some(Ok(inbox)) => app.drops = Some(inbox),
            Some(Err(e)) => app.error = Some(e),
            None => app.error = Some("Dropping files into FEx is unavailable (no window handle)".into()),
        }
        thread::spawn(archive::clean_cache);
        app.watch_drives();
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        app.spawn_io(move || std::fs::read(format!(r"{windir}\Fonts\seguiemj.ttf")).ok().map(Msg::EmojiFont));
        app
    }

    fn with_context(ctx: egui::Context, start: Option<String>) -> Self {
        let (tx, rx) = channel();
        let quick = win::quick_access();
        let home = quick.first().map(|q| q.1.clone()).unwrap_or_else(|| "C:\\".into());
        let start = start
            .map(|s| {
                if s == RECENT || s == NETWORK || archive::is_location(&s) {
                    s
                } else {
                    fsys::resolve_input(&s, &home).unwrap_or(home.clone())
                }
            })
            .unwrap_or_else(|| RECENT.to_string());
        let mut app = FexApp {
            updater: crate::update::Updater::default(),
            tx,
            rx,
            ctx,
            cwd: String::new(),
            path_text: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            tabs: vec![Tab { location: start.clone(), back: Vec::new(), forward: Vec::new() }],
            active_tab: 0,
            listing: Vec::new(),
            listing_gen: 0,
            loading: false,
            list_time: Duration::ZERO,
            error: None,
            notice: None,
            watcher: None,
            early_changes: Vec::new(),
            query: String::new(),
            regex_mode: false,
            recursive: true,
            query_dirty: None,
            matcher: None,
            query_error: None,
            generation: Arc::new(AtomicU64::new(0)),
            results: Vec::new(),
            results_total: 0,
            searching: false,
            search_time: None,
            indexes: Vec::new(),
            opening: None,
            requery_at: None,
            last_save: Instant::now(),
            build: None,
            show_hidden: false,
            sort: SortKey::Name,
            descending: false,
            view: Vec::new(),
            focus: None,
            anchor: None,
            pending_select: None,
            pending_rename: false,
            scroll_to: None,
            scroll_set: None,
            scroll_y: 0.0,
            view_h: 0.0,
            rows_rect: Rect::NOTHING,
            band: None,
            drag: None,
            drop_target: None,
            renaming: None,
            confirm_delete: None,
            props: None,
            jobs: Vec::new(),
            show_errors: None,
            cut_marks: HashSet::new(),
            cut_seq: 0,
            cut_job: None,
            history: History::default(),
            history_jobs: Vec::new(),
            drops: None,
            hwnd: None,
            zones: Vec::new(),
            tab_drag: None,
            tab_rects: Vec::new(),
            tab_spring: None,
            release_pointer: false,
            placement: None,
            maximized: false,
            path_editing: false,
            drives: win::drives(),
            volumes: Default::default(),
            letter_dialog: None,
            drive_space: HashMap::new(),
            drive_space_pending: HashSet::new(),
            report_network_space: false,
            help_open: false,
            shell_new_items: None,
            shell_new_loading: false,
            quick,
            servers: net::load_servers(),
            typeahead: None,
            net: netview::NetState::default(),
            elevated: win::is_elevated(),
            service: None,
            service_failed: false,
        };
        app.watch_service();
        app.refresh_connections();
        app.navigate(start, false);
        app
    }

    /// View settings from the last session, and its tabs (the active one is
    /// already open).
    fn apply_session(&mut self, s: &Session, tabs: Vec<String>) {
        self.show_hidden = s.show_hidden;
        self.recursive = s.recursive;
        self.regex_mode = s.regex;
        if (self.sort, self.descending) != (s.sort, s.descending) {
            self.sort = s.sort;
            self.descending = s.descending;
            self.resort();
        } else {
            self.rebuild_view();
        }
        if tabs.len() > 1 {
            let active = s.active.min(tabs.len() - 1);
            self.tabs = tabs.into_iter().map(|location| Tab { location, back: Vec::new(), forward: Vec::new() }).collect();
            self.active_tab = active;
            self.sync_active_tab();
        }
    }

    fn session(&self) -> Session {
        Session {
            tabs: self.tabs.iter().enumerate().map(|(i, t)| if i == self.active_tab { self.cwd.clone() } else { t.location.clone() }).collect(),
            active: self.active_tab,
            sort: self.sort,
            descending: self.descending,
            show_hidden: self.show_hidden,
            recursive: self.recursive,
            regex: self.regex_mode,
            pos: self.placement.map(|(pos, _)| pos),
            size: self.placement.map(|(_, size)| size),
            maximized: self.maximized,
        }
    }

    // ------------------------------------------------------------ selection

    fn in_results(&self) -> bool {
        self.recursive && self.matcher.is_some()
    }

    fn source(&self) -> &[Row] {
        if self.in_results() { &self.results } else { &self.listing }
    }

    fn source_mut(&mut self) -> &mut Vec<Row> {
        if self.in_results() { &mut self.results } else { &mut self.listing }
    }

    fn row(&self, view_idx: usize) -> Option<&Row> {
        self.view.get(view_idx).and_then(|&i| self.source().get(i as usize))
    }

    /// Identity of the focused row. Capture it *before* changing `listing` or
    /// `results`: afterwards `view` holds stale indices until `rebuild_view_keeping`.
    fn focus_key(&self) -> Option<String> {
        self.focus.and_then(|i| self.row(i)).map(|r| r.full_path(&self.cwd))
    }

    fn visible(&self, r: &Row) -> bool {
        self.show_hidden || !r.is_hidden()
    }

    fn selected(&self) -> Vec<&Row> {
        let src = self.source();
        self.view.iter().filter_map(|&i| src.get(i as usize)).filter(|r| r.sel).collect()
    }

    fn selected_paths(&self) -> Vec<String> {
        self.selected().iter().map(|r| r.full_path(&self.cwd)).collect()
    }

    fn set_sel(&mut self, vi: usize, on: bool) {
        if let Some(&i) = self.view.get(vi) {
            if let Some(r) = self.source_mut().get_mut(i as usize) {
                r.sel = on;
            }
        }
    }

    fn clear_sel(&mut self) {
        for r in self.source_mut() {
            r.sel = false;
        }
    }

    fn select_only(&mut self, vi: usize) {
        self.clear_sel();
        self.set_sel(vi, true);
        self.focus = Some(vi);
        self.anchor = Some(vi);
    }

    fn select_range(&mut self, a: usize, b: usize) {
        for vi in a.min(b)..=a.max(b) {
            self.set_sel(vi, true);
        }
    }

    fn find_name(&self, name: &str) -> Option<usize> {
        (0..self.view.len()).find(|&i| self.row(i).is_some_and(|r| r.dir.is_none() && r.name.eq_ignore_ascii_case(name)))
    }

    /// Recompute `view`; only valid while `view` still matches the source.
    fn rebuild_view(&mut self) {
        let keep = self.focus_key();
        self.rebuild_view_keeping(keep);
    }

    /// Recompute `view` after the source changed, restoring focus to `keep`.
    fn rebuild_view_keeping(&mut self, keep: Option<String>) {
        let filter = (!self.in_results()).then(|| self.matcher.clone()).flatten();
        self.view = self
            .source()
            .iter()
            .enumerate()
            .filter(|(_, r)| self.visible(r) && filter.as_ref().is_none_or(|m| m.is_match_entry(&r.name, r.is_dir())))
            .map(|(i, _)| i as u32)
            .collect();
        self.focus = keep.and_then(|p| {
            (0..self.view.len()).find(|&i| self.row(i).is_some_and(|r| fsys::is_path(&p, r.dir.as_deref().unwrap_or(&self.cwd), &r.name)))
        });
        self.anchor = self.focus;
        if let Some(name) = self.pending_select.clone() {
            if let Some(vi) = self.find_name(&name) {
                self.pending_select = None;
                self.select_only(vi);
                self.scroll_to = Some(vi);
                if std::mem::take(&mut self.pending_rename) {
                    self.begin_rename(vi);
                }
            }
        }
    }

    fn resort(&mut self) {
        let keep = self.focus_key();
        fsys::sort_rows_par(&mut self.listing, self.sort, self.descending);
        fsys::sort_rows_par(&mut self.results, self.sort, self.descending);
        self.rebuild_view_keeping(keep);
    }

    // ----------------------------------------------------------- navigation

    fn sync_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.location = self.cwd.clone();
            tab.back.clone_from(&self.back);
            tab.forward.clone_from(&self.forward);
        }
    }

    /// Open `location` in a new tab after the current one and switch to it.
    fn open_tab(&mut self, location: String) {
        self.sync_active_tab();
        self.active_tab += 1;
        self.tabs.insert(self.active_tab, Tab { location: location.clone(), back: Vec::new(), forward: Vec::new() });
        self.back.clear();
        self.forward.clear();
        self.navigate(location, false);
    }

    /// Next (`step` = 1) or previous (`step` = -1) tab, wrapping around.
    fn cycle_tab(&mut self, step: isize) {
        let n = self.tabs.len() as isize;
        self.switch_tab((self.active_tab as isize + step).rem_euclid(n) as usize);
    }

    fn close_other_tabs(&mut self, keep: usize) {
        if keep >= self.tabs.len() {
            return;
        }
        self.sync_active_tab();
        let tab = self.tabs.swap_remove(keep);
        self.tabs = vec![tab];
        if keep != self.active_tab {
            self.active_tab = 0;
            let tab = self.tabs[0].clone();
            self.back = tab.back;
            self.forward = tab.forward;
            self.navigate(tab.location, false);
        }
        self.active_tab = 0;
    }

    fn switch_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active_tab {
            return;
        }
        self.sync_active_tab();
        self.active_tab = index;
        let tab = self.tabs[index].clone();
        self.back = tab.back;
        self.forward = tab.forward;
        self.navigate(tab.location, false);
    }

    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return;
        }
        self.sync_active_tab();
        self.tabs.remove(index);
        if index < self.active_tab {
            self.active_tab -= 1;
        } else if index == self.active_tab {
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            let tab = self.tabs[self.active_tab].clone();
            self.back = tab.back;
            self.forward = tab.forward;
            self.navigate(tab.location, false);
        }
    }

    fn navigate(&mut self, path: String, record: bool) {
        if record && path != self.cwd {
            self.back.push(std::mem::take(&mut self.cwd));
            self.forward.clear();
        }
        if path.starts_with(r"\\") {
            let server = path.split('\\').take(3).collect::<Vec<_>>().join("\\");
            self.remember_server(server);
        }
        self.ctx.send_viewport_cmd(ViewportCommand::Title(format!("FEx — {path}")));
        self.cwd = path;
        self.path_text = self.cwd.clone();
        self.notice = None;
        self.sync_active_tab();
        self.listing.clear();
        self.focus = None;
        self.anchor = None;
        self.renaming = None;
        self.typeahead = None;
        self.scroll_to = Some(0);
        if !self.query.is_empty() {
            self.query.clear();
            self.apply_query();
        }
        if self.cwd == NETWORK {
            self.listing_gen += 1; // drop any listing still in flight
            self.watcher = None;
            self.loading = false;
            self.error = None;
            self.rebuild_view();
            self.open_network_page();
        } else {
            self.load_listing();
        }
    }

    fn remember_server(&mut self, server: String) {
        if server.len() > 2 && !self.servers.iter().any(|s| s.eq_ignore_ascii_case(&server)) {
            self.servers.push(server);
            self.servers.sort_by_key(|s| s.to_lowercase());
            net::save_servers(&self.servers);
        }
    }

    fn forget_server(&mut self, server: &str) {
        self.servers.retain(|s| !s.eq_ignore_ascii_case(server));
        net::save_servers(&self.servers);
    }

    fn load_listing(&mut self) {
        self.listing_gen += 1;
        self.loading = true;
        self.error = None;
        self.early_changes.clear();
        let generation = self.listing_gen;
        // Start watching before listing so nothing slips between the two.
        self.watcher = (self.cwd != RECENT && !crate::archive::is_location(&self.cwd) && !fsys::is_server_root(&self.cwd)).then(|| {
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            watch::watch(self.cwd.clone(), move |changes| {
                let _ = tx.send(Msg::DirChanges { generation, changes });
                ctx.request_repaint();
            })
        });
        let path = self.cwd.clone();
        // Sorted here, off the UI thread (Recent keeps its most-recent-first order).
        let sorted = (self.sort, self.descending);
        self.spawn_io_or(
            move || {
                let t = Instant::now();
                let mut result = if path == RECENT {
                    Ok(crate::recent::rows())
                } else if crate::archive::is_location(&path) {
                    crate::archive::rows(&path)
                } else {
                    fsys::list_dir(&path)
                };
                if let (Ok(rows), false) = (&mut result, path == RECENT) {
                    fsys::sort_rows_par(rows, sorted.0, sorted.1);
                }
                Some(Msg::Listing { generation, result, elapsed: t.elapsed(), sorted })
            },
            move |e| Msg::Listing { generation, result: Err(e), elapsed: Duration::ZERO, sorted },
        );
        self.rebuild_view();
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            self.forward.push(std::mem::take(&mut self.cwd));
            self.navigate(p, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(std::mem::take(&mut self.cwd));
            self.navigate(p, false);
        }
    }

    fn go_up(&mut self) {
        if crate::archive::is_location(&self.cwd) {
            if let Some((archive, inner)) = crate::archive::parse_location(&self.cwd) {
                let child = if inner.is_empty() {
                    std::path::Path::new(archive).file_name().map(|name| name.to_string_lossy().into_owned())
                } else {
                    Some(inner.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(inner).to_string())
                };
                if let Some(parent) = crate::archive::parent_location(&self.cwd) {
                    self.navigate(parent, true);
                    self.pending_select = child.map(Into::into);
                    return;
                }
            }
        }
        if let Some(p) = fsys::parent(&self.cwd) {
            let child = self.cwd.trim_end_matches('\\').rsplit('\\').next().map(Box::from);
            self.navigate(p, true);
            self.pending_select = child; // land on the folder we came from
        }
    }

    fn refresh(&mut self) {
        self.rescan_drives();
        self.refresh_connections();
        if self.cwd == NETWORK {
            self.open_network_page();
            return;
        }
        // Rebuild: the next search reads the saved copy, then crawls afresh.
        self.indexes.retain(|live| !live.covers(&self.cwd));
        if let Some(b) = self.build.take() {
            b.cancel();
        }
        self.load_listing();
        if self.matcher.is_some() {
            self.apply_query();
        }
    }

    /// Resolve typed path off-thread (network paths can stall for seconds).
    fn submit_path(&mut self) {
        if crate::archive::is_location(self.path_text.trim()) {
            self.navigate(self.path_text.trim().to_string(), true);
            return;
        }
        if self.path_text.trim().eq_ignore_ascii_case(NETWORK) {
            self.navigate(NETWORK.into(), true);
            return;
        }
        if self.path_text.trim().eq_ignore_ascii_case(RECENT) {
            self.navigate(RECENT.into(), true);
            return;
        }
        let base = if self.cwd == NETWORK { self.quick.first().map_or("C:\\", |q| q.1.as_str()) } else { self.cwd.as_str() };
        let Some(path) = fsys::resolve_input(&self.path_text, base) else {
            self.path_text = self.cwd.clone();
            return;
        };
        self.spawn_io(move || {
            if fsys::is_server_root(&path) {
                return Some(Msg::Navigate(path));
            }
            Some(match win::attributes(&path) {
                Some(a) if a & win::ATTR_DIR != 0 => Msg::Navigate(path),
                Some(_) => match win::shell_open(&path) {
                    Ok(()) => {
                        crate::recent::record(&path);
                        Msg::RecentChanged
                    }
                    Err(e) => Msg::Error(e),
                },
                None => Msg::Error(format!("Cannot find '{path}'")),
            })
        });
    }

    fn open(&mut self, view_idx: usize) {
        let Some(row) = self.row(view_idx) else { return };
        let path = row.full_path(&self.cwd);
        if row.is_dir() {
            self.navigate(path, true);
        } else if crate::archive::is_location(&self.cwd) {
                let member = path.clone();
                self.spawn_io(move || match crate::archive::extract_member(&member) {
                    Ok(path) => win::shell_open(&path.to_string_lossy()).err().map(Msg::Error),
                    Err(error) => Some(Msg::Error(error)),
                });
        } else if crate::archive::is_archive_file(&path) {
            self.navigate(crate::archive::root_location(&path), true);
        } else {
            self.spawn_io(move || match win::shell_open(&path) {
                Ok(()) => {
                    crate::recent::record(&path);
                    Some(Msg::RecentChanged)
                }
                Err(e) => Some(Msg::Error(e)),
            });
        }
    }

    fn run_as(&mut self, view_idx: usize) {
        if crate::archive::is_location(&self.cwd) {
            return;
        }
        let Some(row) = self.row(view_idx).filter(|r| !r.is_dir()) else { return };
        let path = row.full_path(&self.cwd);
        self.spawn_io(move || match win::shell_run_as(&path) {
            Ok(()) => {
                crate::recent::record(&path);
                Some(Msg::RecentChanged)
            }
            Err(e) => Some(Msg::Error(e)),
        });
    }

    /// Run blocking shell/file work off the UI thread; a returned message is
    /// delivered. A panic is reported as an error.
    fn spawn_io(&self, f: impl FnOnce() -> Option<Msg> + Send + 'static) {
        self.spawn_io_or(f, Msg::Error);
    }

    /// `spawn_io`, where a panic delivers `on_panic(error)` instead, so state
    /// waiting for the answer (busy flags, spinners) is released.
    fn spawn_io_or(&self, f: impl FnOnce() -> Option<Msg> + Send + 'static, on_panic: impl FnOnce(String) -> Msg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let msg = catch_unwind(AssertUnwindSafe(f))
                .unwrap_or_else(|panic| Some(on_panic(format!("Internal error: {}", win::panic_text(&*panic)))));
            if let Some(msg) = msg {
                let _ = tx.send(msg);
                ctx.request_repaint();
            }
        });
    }

    // ------------------------------------------------------- file operations

    fn start_job(&mut self, kind: Kind, sources: Vec<String>, dest: String) -> Option<Arc<Job>> {
        if sources.is_empty() {
            return None;
        }
        if dest == RECENT
            || crate::archive::is_location(&dest)
            || sources.iter().any(|source| crate::archive::is_location(source))
            || fsys::is_server_root(&dest)
        {
            self.error = Some("Choose a real folder to paste into".into());
            return None;
        }
        let ctx = self.ctx.clone();
        let job = ops::start(kind, sources, dest, Arc::new(move || ctx.request_repaint()));
        self.jobs.push(job.clone());
        self.history_jobs.push((job.clone(), Pending::Record));
        Some(job)
    }

    // ------------------------------------------------------------- history

    fn undo(&mut self) {
        match self.history.take_undo() {
            Some(entry) => self.run_history(entry, true),
            None if self.history.busy => self.notice = Some("Wait for the last undo to finish".into()),
            None => {}
        }
    }

    fn redo(&mut self) {
        if let Some(entry) = self.history.take_redo() {
            self.run_history(entry, false);
        }
    }

    fn run_history(&mut self, entry: Entry, undo: bool) {
        let step = if undo { entry.undo_step() } else { entry.redo_step() };
        self.history.busy = true;
        self.notice = Some(format!("{} {}", if undo { "Undoing" } else { "Redoing" }, entry.describe()));
        if let Step::Transfer { kind, pairs } = step {
            let ctx = self.ctx.clone();
            let job = ops::start_pairs(kind, pairs, Arc::new(move || ctx.request_repaint()));
            self.jobs.push(job.clone());
            self.history_jobs.push((job, if undo { Pending::Undo(entry) } else { Pending::Redo }));
            return;
        }
        let retry = entry.clone();
        self.spawn_io_or(
            move || {
                let result = match step {
                    Step::Rename { from, to } => fileop::rename(&from, &to, false).map_err(|e| format!("{from}: {}", win::error_text(e))),
                    Step::Recycle(paths) => {
                        win::com_init();
                        fileop::recycle(&paths, false)
                    }
                    Step::Restore(paths) => recyclebin::restore(&paths),
                    Step::Transfer { .. } => Err("internal error: transfers run as jobs".into()),
                };
                Some(Msg::HistoryDone { entry, undo, result })
            },
            move |e| Msg::HistoryDone { entry: retry, undo, result: Err(e) },
        );
    }

    /// Record finished transfers in the history (and finish undo/redo jobs).
    fn finish_history_jobs(&mut self) {
        let mut i = 0;
        while i < self.history_jobs.len() {
            if !self.history_jobs[i].0.finished() {
                i += 1;
                continue;
            }
            let (job, pending) = self.history_jobs.remove(i);
            let created = Entry::Transfer { kind: job.kind, pairs: job.created.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() };
            let clean = job.phase() == Phase::Done && job.error_count() == 0;
            match pending {
                Pending::Record => self.history.record(created),
                Pending::Redo => {
                    self.history.busy = false;
                    self.notice = None;
                    self.history.redone(created);
                }
                Pending::Undo(entry) => {
                    self.history.busy = false;
                    self.notice = None;
                    if clean {
                        self.history.undone(entry);
                    } else {
                        self.error = Some(format!("Undo of {} did not complete", entry.describe()));
                    }
                }
            }
        }
    }

    fn clip_selection(&mut self, cut: bool) {
        if crate::archive::is_location(&self.cwd) {
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        match clip::set_files(&paths, cut) {
            Ok(seq) => {
                self.cut_marks = if cut { paths.iter().map(|p| p.to_lowercase()).collect() } else { HashSet::new() };
                self.cut_seq = seq;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn paste(&mut self, into: Option<String>) {
        if (into.is_none() && self.cwd == RECENT) || crate::archive::is_location(&self.cwd) {
            return;
        }
        let Some((files, cut)) = clip::get_files() else { return };
        let seq = clip::sequence();
        let dest = into.unwrap_or_else(|| self.cwd.clone());
        if let Some(job) = self.start_job(if cut { Kind::Move } else { Kind::Copy }, files, dest) {
            if cut {
                self.cut_job = Some((job, seq));
            }
        }
    }

    /// Drag/drop semantics match Explorer: Ctrl copies, Shift moves, and
    /// otherwise same volume moves while different volumes copy.
    fn drop_kind(mods: Modifiers, same_volume: bool) -> Kind {
        if mods.ctrl {
            Kind::Copy
        } else if mods.shift || same_volume {
            Kind::Move
        } else {
            Kind::Copy
        }
    }

    /// Jump to the best match for `text` (see `typeahead::score`). A single
    /// letter, or the same letter repeated ("ddd"), cycles through names
    /// starting with it, like Explorer.
    fn type_to_find(&mut self, text: String) {
        let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
        let first = text.chars().next().map(fold).unwrap_or(' ');
        let starts = |r: &Row| r.name.chars().next().map(fold) == Some(first);
        let repeated = text.chars().map(fold).all(|c| c == first);
        let n = self.view.len();
        let whole_prefix = |r: &Row| r.name.to_lowercase().starts_with(&text.to_lowercase());

        let found: Option<(usize, Vec<usize>)> = if repeated && (text.chars().count() == 1 || !(0..n).any(|vi| self.row(vi).is_some_and(whole_prefix))) {
            let from = match self.focus {
                Some(f) if self.row(f).is_some_and(starts) => f + 1,
                _ => 0,
            };
            (0..n).map(|k| (from + k) % n).find(|&vi| self.row(vi).is_some_and(starts)).map(|vi| (vi, vec![0]))
        } else {
            let mut best: Option<(i32, usize, Vec<usize>)> = None;
            for vi in 0..n {
                if let Some(m) = self.row(vi).and_then(|r| typeahead::score(&text, &r.name)) {
                    if best.as_ref().is_none_or(|b| m.score > b.0) {
                        best = Some((m.score, vi, m.positions));
                    }
                }
            }
            best.map(|(_, vi, p)| (vi, p))
        };

        let (path, positions) = match found {
            Some((vi, positions)) => {
                self.select_only(vi);
                self.scroll_to = Some(vi);
                (self.row(vi).map(|r| r.full_path(&self.cwd)).unwrap_or_default(), positions)
            }
            None => (String::new(), Vec::new()),
        };
        self.typeahead = Some(TypeAhead { text, last: Instant::now(), path, positions });
        self.ctx.request_repaint_after(TYPEAHEAD_RESET);
    }

    fn begin_rename(&mut self, vi: usize) {
        if crate::archive::is_location(&self.cwd) {
            return;
        }
        if let Some(r) = self.row(vi) {
            let name = r.name.to_string();
            self.renaming = Some(Rename { path: r.full_path(&self.cwd), original: name.clone(), text: name, focused: false });
        }
    }

    fn commit_rename(&mut self, r: Rename) {
        let new = r.text.trim().to_string();
        if new.is_empty() || new == r.original {
            return;
        }
        if new.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            self.error = Some(r#"A name can't contain \ / : * ? " < > |"#.into());
            return;
        }
        let dir = fsys::parent(&r.path).unwrap_or_default();
        let dst = fsys::join(&dir, &new);
        self.pending_select = Some(new.as_str().into());
        self.spawn_io(move || {
            // A case-only rename may be reported as colliding with itself;
            // any other failure is real.
            const ERROR_FILE_EXISTS: u32 = 80;
            const ERROR_ALREADY_EXISTS: u32 = 183;
            let case_only = new.eq_ignore_ascii_case(&r.original);
            Some(match fileop::rename(&r.path, &dst, false) {
                Ok(()) => Msg::Renamed { old_path: r.path, new_name: new },
                Err(ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS) if case_only => Msg::Renamed { old_path: r.path, new_name: new },
                Err(e) => Msg::Error(format!("Rename failed: {}", win::error_text(e))),
            })
        });
    }

    fn new_folder(&mut self) {
        if self.cwd == RECENT || crate::archive::is_location(&self.cwd) {
            return;
        }
        let dir = self.cwd.clone();
        self.spawn_io(move || {
            let name = ops::unique_name(&dir, "New folder", false);
            Some(match fileop::create_dir(&fsys::join(&dir, &name)) {
                Ok(()) => Msg::Created { dir, name },
                Err(e) => Msg::Error(format!("Could not create folder: {}", win::error_text(e))),
            })
        });
    }

    /// Open `location` in a new FEx window the size of this one, with its
    /// top-left at `at` (points) if given. True if it started.
    fn open_window(&mut self, location: &str, at: Option<[f32; 2]>) -> bool {
        let spawned = std::env::current_exe().and_then(|exe| {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg(location);
            if let Some([x, y]) = at {
                cmd.arg("--at").arg(format!("{x:.0},{y:.0}"));
            }
            if let Some((_, [w, h])) = self.placement {
                cmd.arg("--size").arg(format!("{w:.0},{h:.0}"));
            }
            cmd.spawn()
        });
        match spawned {
            Ok(_) => true,
            Err(error) => {
                self.error = Some(format!("Could not open a new FEx window: {error}"));
                false
            }
        }
    }

    /// A new empty text file, named like Explorer's, then rename it.
    fn new_file(&mut self) {
        if self.cwd == RECENT || self.cwd == NETWORK || archive::is_location(&self.cwd) || fsys::is_server_root(&self.cwd) {
            return;
        }
        let dir = self.cwd.clone();
        self.spawn_io(move || Some(match crate::shellnew::create(&dir, &crate::shellnew::Item::text_document()) {
            Ok(name) => Msg::Created { dir, name },
            Err(error) => Msg::Error(error),
        }));
    }

    fn load_shell_new_items(&mut self) {
        if self.shell_new_loading || self.shell_new_items.is_some() {
            return;
        }
        self.shell_new_loading = true;
        self.spawn_io_or(|| Some(Msg::ShellNewItems(crate::shellnew::load())), |_| Msg::ShellNewItems(Vec::new()));
    }

    fn create_shell_new_item(&mut self, index: usize) {
        if self.cwd == RECENT || self.cwd == NETWORK || archive::is_location(&self.cwd) {
            return;
        }
        let Some(item) = self.shell_new_items.as_ref().and_then(|items| items.get(index)).cloned() else { return };
        let dir = self.cwd.clone();
        self.spawn_io(move || Some(match crate::shellnew::create(&dir, &item) {
            Ok(name) => Msg::Created { dir, name },
            Err(error) => Msg::Error(error),
        }));
    }

    /// Extract archive members (`zip:` locations) into the folder `dest`.
    fn extract_to(&mut self, members: Vec<String>, dest: String) {
        if dest == RECENT || archive::is_location(&dest) || fsys::is_server_root(&dest) {
            self.error = Some("Choose a real folder to extract into".into());
            return;
        }
        self.notice = Some(format!("Extracting {} item(s)…", members.len()));
        self.spawn_io(move || {
            let (created, errors) = archive::extract_members(&members, &dest);
            Some(Msg::Extracted { created, open: None, errors })
        });
    }

    /// Prepare `drive` for removal. FEx first lets go of everything it holds
    /// open there (the folder watcher, a running index), since any open
    /// handle makes Windows refuse.
    fn eject(&mut self, drive: String) {
        let on_drive = |p: &str| index::in_scope(archive::parse_location(p).map_or(p, |(a, _)| a), &drive);
        if self.jobs.iter().any(|j| !j.finished() && (on_drive(&j.dest) || j.sources.iter().any(|s| on_drive(s)))) {
            self.error = Some(format!("Wait for transfers involving {drive} to finish before ejecting it"));
            return;
        }
        if on_drive(&self.cwd) {
            let home = self.quick.first().map_or_else(|| RECENT.to_string(), |q| q.1.clone());
            self.navigate(home, true);
        }
        if self.build.as_ref().is_some_and(|b| on_drive(&b.root)) {
            if let Some(b) = self.build.take() {
                b.cancel();
            }
        }
        // Indexes hold a watcher handle open on their drive.
        self.indexes.retain(|live| !on_drive(&live.root));
        self.error = None;
        self.notice = Some(format!("Preparing {drive} for removal…"));
        self.spawn_io(move || Some(match fileop::eject(&drive) {
            Ok(()) => Msg::DriveEjected(drive),
            Err(e) => Msg::Error(format!("Could not eject {drive}: {e}")),
        }));
    }

    fn open_properties(&mut self) {
        if crate::archive::is_location(&self.cwd) {
            return;
        }
        let items: Vec<props::Item> = {
            let sel = self.selected();
            if sel.is_empty() {
                // The folder's own attributes arrive with its details (read off this thread).
                let name = self.cwd.trim_end_matches('\\').rsplit('\\').next().unwrap_or(&self.cwd).to_string();
                vec![props::Item { path: self.cwd.clone(), name, attrs: win::ATTR_DIR, size: 0 }]
            } else {
                sel.iter()
                    .map(|r| props::Item { path: r.full_path(&self.cwd), name: r.name.to_string(), attrs: r.attrs, size: r.size })
                    .collect()
            }
        };
        self.props = Some(Props::new(items, self.ctx.clone()));
    }

    /// Fold watcher notifications into the listing: new entries are inserted
    /// in sort order, existing ones only get their metadata updated in place
    /// (no re-sorting), and deleted ones are removed.
    fn apply_changes(&mut self, changes: Vec<Change>) {
        if changes.iter().any(|c| matches!(c, Change::Rescan)) {
            self.load_listing();
            return;
        }
        // Find each changed name's row. A small batch scans the listing (no
        // allocation); a big one builds a lookup table once.
        let resolved: Vec<(Change, Option<usize>)> = {
            let table: Option<HashMap<String, usize>> =
                (changes.len() > 32).then(|| self.listing.iter().enumerate().map(|(i, r)| (r.name.to_lowercase(), i)).collect());
            let find = |name: &str| match &table {
                Some(t) => t.get(&name.to_lowercase()).copied(),
                None => self.listing.iter().position(|r| same_name(&r.name, name)),
            };
            changes
                .into_iter()
                .map(|c| {
                    let i = match &c {
                        Change::Upsert(row) => find(&row.name),
                        Change::Removed(name) => find(name),
                        Change::Rescan => None,
                    };
                    (c, i)
                })
                .collect()
        };
        let mut removed: HashSet<usize> = HashSet::new();
        let mut added: Vec<Row> = Vec::new();
        for (c, found) in resolved {
            match c {
                Change::Upsert(row) => match found {
                    Some(i) => {
                        let r = &mut self.listing[i];
                        r.size = row.size;
                        r.mtime = row.mtime;
                        r.attrs = row.attrs;
                        r.name = row.name; // picks up case-only renames
                        removed.remove(&i);
                    }
                    None if !added.iter().any(|a| a.name.eq_ignore_ascii_case(&row.name)) => added.push(row),
                    None => {}
                },
                Change::Removed(name) => {
                    if let Some(i) = found {
                        removed.insert(i);
                    }
                    added.retain(|a| !a.name.eq_ignore_ascii_case(&name));
                }
                Change::Rescan => {}
            }
        }
        if removed.is_empty() && added.is_empty() {
            return; // metadata-only: rows were updated in place
        }
        let keep = self.focus_key();
        if !removed.is_empty() {
            let mut i = 0;
            self.listing.retain(|_| {
                i += 1;
                !removed.contains(&(i - 1))
            });
        }
        for row in added {
            let pos = self.listing.partition_point(|r| fsys::cmp_rows(r, &row, self.sort, self.descending).is_lt());
            self.listing.insert(pos, row);
        }
        if !self.in_results() {
            self.rebuild_view_keeping(keep);
        }
    }

    // --------------------------------------------------------------- search

    fn apply_query(&mut self) {
        self.query_dirty = None;
        let generation = self.generation.fetch_add(1, Relaxed) + 1;
        match Matcher::new(&self.query, self.regex_mode) {
            Ok(m) => {
                self.matcher = m.map(Arc::new);
                self.query_error = None;
            }
            Err(e) => {
                self.matcher = None;
                self.query_error = Some(e.lines().last().unwrap_or("invalid pattern").trim().to_string());
            }
        }
        self.results.clear();
        self.results_total = 0;
        self.searching = false;
        self.search_time = None;
        self.focus = None;
        self.scroll_to = Some(0);
        if self.in_results() {
            self.start_search(generation);
        } else if let Some(b) = &self.build {
            b.set_matcher(generation, None);
        }
        self.rebuild_view();
    }

    fn start_search(&mut self, generation: u64) {
        let Some(m) = self.matcher.clone() else { return };
        self.searching = true;
        if self.cwd == RECENT || crate::archive::is_location(&self.cwd) {
            let t = Instant::now();
            self.results = self.listing.iter().filter(|r| m.is_match_entry(&r.name, r.is_dir())).cloned().collect();
            self.results_total = self.results.len();
            self.searching = false;
            self.search_time = Some(t.elapsed());
            self.rebuild_view();
            return;
        }
        if self.service_covers(&self.cwd) {
            self.run_service_query(generation);
            return;
        }
        if let Some(live) = self.current_index() {
            self.run_query(live, m, generation);
            return;
        }
        if let Some(b) = self.build.as_ref().filter(|b| index::in_scope(&self.cwd, &b.root)) {
            b.set_matcher(generation, Some(m)); // stream hits from the crawl in progress
            return;
        }
        if self.opening.as_deref().is_some_and(|scope| index::in_scope(&self.cwd, scope)) {
            return; // a saved index is being read; it runs the search when ready
        }
        // A saved index answers at once (then a crawl refreshes it); without
        // one, crawl here and stream hits.
        let scope = self.cwd.clone();
        self.opening = Some(scope.clone());
        let fallback = scope.clone();
        self.spawn_io_or(
            move || {
                let index = index::find_saved(&scope).and_then(|path| Index::load(&path));
                Some(Msg::IndexLoaded { scope, index })
            },
            move |_| Msg::IndexLoaded { scope: fallback, index: None },
        );
    }

    /// Poll the index service's status every few seconds (one small message
    /// over a local pipe; fails at once when the service isn't installed).
    fn watch_service(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            loop {
                if tx.send(Msg::Service(service::status().ok())).is_err() {
                    return;
                }
                ctx.request_repaint();
                thread::sleep(Duration::from_secs(5));
            }
        });
    }

    fn service_covers(&self, path: &str) -> bool {
        !self.service_failed && self.service.as_ref().is_some_and(|s| s.covers(path))
    }

    /// Search through the index service: no crawl, and the first screenful
    /// arrives sorted (by name) ahead of the rest. Rows carry their metadata.
    fn run_service_query(&mut self, generation: u64) {
        self.searching = true;
        let (scope, text, regex) = (self.cwd.clone(), self.query.clone(), self.regex_mode);
        let order = match (self.sort, self.descending) {
            (SortKey::Name, false) => service::NAME_ASC,
            (SortKey::Name, true) => service::NAME_DESC,
            _ => service::UNORDERED,
        };
        // Rows that come unordered are reported as not in the shown order,
        // so the result handler sorts them.
        let sorted = if order == service::UNORDERED { (self.sort, !self.descending) } else { (self.sort, self.descending) };
        let (tx, ctx, current) = (self.tx.clone(), self.ctx.clone(), self.generation.clone());
        thread::spawn(move || {
            let t = Instant::now();
            let send = |m: Msg| {
                let ok = current.load(Relaxed) == generation && tx.send(m).is_ok();
                ctx.request_repaint();
                ok
            };
            let ask = || service::query(&scope, &text, regex, MAX_RESULTS as u32, order, SCREENFUL, generation);
            let mut reply = ask();
            // A busy or momentarily refused connection gets one more try.
            if reply.is_err() && current.load(Relaxed) == generation {
                thread::sleep(Duration::from_millis(150));
                reply = ask();
            }
            match reply {
                Ok(service::Reply::Rows { rows, total, mut rest }) => {
                    if !send(Msg::QueryDone { generation, rows, total, elapsed: t.elapsed(), sorted }) {
                        return;
                    }
                    loop {
                        let (rows, done) = match rest.next() {
                            Ok(Some(rows)) => (rows, false),
                            _ => (Vec::new(), true),
                        };
                        if !send(Msg::ServiceMore { generation, rows, done }) || done {
                            return;
                        }
                    }
                }
                Ok(_) => {
                    send(Msg::ServiceFailed { generation, definitive: true });
                }
                Err(_) => {
                    send(Msg::ServiceFailed { generation, definitive: false });
                }
            }
        });
    }

    /// The best index for the open folder: an up-to-date one before a saved
    /// copy, then the narrowest. It becomes the most recently used.
    fn current_index(&mut self) -> Option<Arc<Live>> {
        let pos = (0..self.indexes.len()).filter(|&i| self.indexes[i].covers(&self.cwd)).min_by_key(|&i| {
            let live = &self.indexes[i];
            (live.loaded, std::cmp::Reverse(live.root.len()))
        })?;
        let live = self.indexes.remove(pos);
        self.indexes.insert(0, live.clone());
        Some(live)
    }

    /// Keep `live`, replacing indexes it subsumes and the least recently used.
    fn add_index(&mut self, live: Arc<Live>) {
        self.indexes.retain(|old| !index::in_scope(&old.root, &live.root));
        self.indexes.insert(0, live);
        self.indexes.truncate(MAX_INDEXES);
    }

    /// Crawl `root` into a new index. `stream`: also feed matches for the
    /// current search while crawling (when no other index answers it).
    fn start_build(&mut self, root: &str, stream: Option<(u64, Arc<Matcher>)>) {
        if let Some(old) = self.build.take() {
            old.cancel();
        }
        let build = Build::new(root);
        if let Some((generation, m)) = stream {
            build.set_matcher(generation, Some(m));
        }
        let (b, hits_tx, hits_ctx) = (build.clone(), self.tx.clone(), self.ctx.clone());
        let failed = build.clone();
        self.spawn_io_or(
            move || {
                let index = b.run(&|generation, rows| {
                    let _ = hits_tx.send(Msg::Hits { generation, rows });
                    hits_ctx.request_repaint();
                });
                Some(Msg::IndexReady { build: b, index })
            },
            move |_| Msg::IndexReady { build: failed, index: None },
        );
        self.build = Some(build);
    }

    /// Save `live` to disk in the background.
    fn save_index(&self, live: &Arc<Live>) {
        let live = live.clone();
        self.spawn_io(move || {
            index::write(&live.index).dirty = false;
            // A change from here on marks it dirty again for the next save.
            let saved = index::read(&live.index).save();
            saved.err().map(|e| {
                index::write(&live.index).dirty = true;
                Msg::Error(format!("Could not save the search index: {e}"))
            })
        });
    }

    fn run_query(&mut self, live: Arc<Live>, m: Arc<Matcher>, generation: u64) {
        self.searching = true;
        let (scope, current) = (self.cwd.clone(), self.generation.clone());
        let sorted = (self.sort, self.descending);
        self.spawn_io_or(
            move || {
                let t = Instant::now();
                let cancelled = || current.load(Relaxed) != generation;
                let found = index::read(&live.index).query(&m, &scope, MAX_RESULTS, &cancelled);
                let (mut rows, total) = found?;
                // Sorted here, in parallel, rather than on the UI thread.
                fsys::sort_rows_par(&mut rows, sorted.0, sorted.1);
                Some(Msg::QueryDone { generation, rows, total, elapsed: t.elapsed(), sorted })
            },
            move |_| Msg::QueryDone { generation, rows: Vec::new(), total: 0, elapsed: Duration::ZERO, sorted },
        );
    }

    // ------------------------------------------------------------- messages

    fn drain_messages(&mut self) {
        let current = self.generation.load(Relaxed);
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Listing { generation, result, elapsed, sorted } if generation == self.listing_gen => {
                    self.loading = false;
                    self.list_time = elapsed;
                    // On refresh, keep what was selected and focused.
                    let focus = self.focus_key();
                    let keep: HashSet<Box<str>> = self.listing.iter().filter(|r| r.sel).map(|r| r.name.clone()).collect();
                    match result {
                        Ok(mut rows) => {
                            for r in rows.iter_mut() {
                                r.sel = keep.contains(&r.name);
                            }
                            self.listing = rows;
                        }
                        Err(e) => {
                            self.listing.clear();
                            self.error = Some(e);
                        }
                    }
                    if self.cwd != RECENT && sorted != (self.sort, self.descending) {
                        fsys::sort_rows(&mut self.listing, self.sort, self.descending);
                    }
                    self.rebuild_view_keeping(focus);
                    let early = std::mem::take(&mut self.early_changes);
                    if !early.is_empty() {
                        self.apply_changes(early);
                    }
                    if self.cwd == RECENT && self.matcher.is_some() {
                        self.apply_query();
                    }
                }
                Msg::DirChanges { generation, changes } if generation == self.listing_gen => {
                    if self.loading {
                        self.early_changes.extend(changes);
                    } else {
                        self.apply_changes(changes);
                    }
                }
                Msg::Hits { generation, rows } if generation == current && self.in_results() => {
                    // A build rooted above cwd streams hits from outside it too.
                    let scoped = self.build.as_ref().is_some_and(|b| *b.root != *self.cwd);
                    for r in rows {
                        if scoped && !r.dir.as_deref().is_some_and(|d| index::in_scope(d, &self.cwd)) {
                            continue;
                        }
                        self.results_total += 1;
                        if self.results.len() < MAX_RESULTS {
                            if self.visible(&r) {
                                self.view.push(self.results.len() as u32);
                            }
                            self.results.push(r);
                        }
                    }
                }
                Msg::IndexReady { build, index } => {
                    if self.build.as_ref().is_some_and(|b| Arc::ptr_eq(b, &build)) {
                        self.build = None;
                        self.searching = false;
                        if let Some(idx) = index {
                            let (tx, ctx, root) = (self.tx.clone(), self.ctx.clone(), idx.root.clone());
                            let live = Live::new(idx, build.take_freshener(), move |change| {
                                let _ = tx.send(match change {
                                    Freshness::Changed => Msg::IndexChanged(root.clone()),
                                    Freshness::Stale => Msg::IndexStale(root.clone()),
                                });
                                ctx.request_repaint();
                            });
                            self.add_index(live.clone());
                            self.save_index(&live);
                            // Replace streamed (or saved-copy) results with a complete, sorted set.
                            if let (true, Some(m), true) = (self.in_results(), self.matcher.clone(), live.covers(&self.cwd)) {
                                self.run_query(live, m, current);
                            }
                        }
                    }
                }
                Msg::IndexLoaded { scope, index } => {
                    if self.opening.as_deref() == Some(scope.as_str()) {
                        self.opening = None;
                    }
                    let searching_here = self.in_results() && index::in_scope(&self.cwd, &scope);
                    match index {
                        Some(idx) => {
                            let root = idx.root.to_string();
                            let live = Live::new(idx, None, |_| {});
                            self.add_index(live.clone());
                            // The saved copy answers now; a crawl brings it up to date.
                            self.start_build(&root, None);
                            if let (true, Some(m)) = (searching_here, self.matcher.clone()) {
                                self.run_query(live, m, current);
                            }
                        }
                        None if searching_here && !self.indexes.iter().any(|l| l.covers(&self.cwd)) => {
                            let stream = self.matcher.clone().map(|m| (current, m));
                            let root = self.cwd.clone();
                            self.start_build(&root, stream);
                        }
                        None => {
                            if !self.build.as_ref().is_some_and(|b| index::in_scope(&self.cwd, &b.root)) {
                                self.searching = false;
                            }
                        }
                    }
                }
                Msg::IndexChanged(root) => {
                    if self.in_results() && self.indexes.iter().any(|l| l.root == root && l.covers(&self.cwd)) {
                        self.requery_at.get_or_insert_with(|| Instant::now() + REQUERY_DELAY);
                    }
                }
                Msg::IndexStale(root) => {
                    let old = self.indexes.iter().find(|l| l.root == root).is_some_and(|l| l.created.elapsed() > STALE_GRACE);
                    if old && self.build.is_none() {
                        self.start_build(&root, None);
                    }
                }
                Msg::Service(st) => {
                    self.service_failed = false;
                    self.service = st;
                }
                Msg::ServiceMore { generation, rows, done } if generation == current && self.in_results() => {
                    for r in rows {
                        if self.results.len() < MAX_RESULTS {
                            if self.visible(&r) {
                                self.view.push(self.results.len() as u32);
                            }
                            self.results.push(r);
                        }
                    }
                    if done {
                        let focus = self.focus_key();
                        fsys::sort_rows_par(&mut self.results, self.sort, self.descending);
                        self.rebuild_view_keeping(focus);
                    }
                }
                Msg::ServiceFailed { generation, definitive } if generation == current => {
                    if definitive {
                        self.service_failed = true;
                        self.start_search(generation);
                    } else {
                        self.searching = false;
                        self.error = Some("The index service didn't answer; search again in a moment.".into());
                    }
                }
                Msg::QueryDone { generation, rows, total, elapsed, sorted } if generation == current => {
                    let focus = self.focus_key();
                    // A refreshed result set keeps what was selected.
                    let selected: HashSet<String> = self.results.iter().filter(|r| r.sel).map(|r| r.full_path(&self.cwd)).collect();
                    self.searching = false;
                    self.results = rows;
                    if !selected.is_empty() {
                        for r in self.results.iter_mut() {
                            r.sel = selected.contains(&r.full_path(&self.cwd));
                        }
                    }
                    self.results_total = total;
                    self.search_time = Some(elapsed);
                    if sorted != (self.sort, self.descending) {
                        fsys::sort_rows_par(&mut self.results, self.sort, self.descending);
                    }
                    self.rebuild_view_keeping(focus);
                }
                Msg::EmojiFont(bytes) => {
                    // Last resort for glyphs no other font has (emoji in file names).
                    let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                        .into_iter()
                        .map(|family| egui::InsertFontFamily { family, priority: egui::FontPriority::Lowest })
                        .collect();
                    self.ctx.add_font(egui::FontInsert::new("seguiemj.ttf", egui::FontData::from_owned(bytes), families));
                }
                Msg::DriveSpace { path, target, is_network, queried, ejectable, result } => {
                    self.drive_space_pending.remove(&path);
                    if self.volumes.letter(&path).map(|d| &d.target) == target.as_ref() {
                        self.drive_space.insert(path, DriveSpace { is_network, queried, ejectable, capacity: result, checked: Instant::now() });
                    }
                }
                Msg::Volumes(snapshot) => self.set_volumes(snapshot),
                Msg::LetterAssigned { result, letter } => self.letter_assigned(result, letter),
                Msg::DriveEjected(path) => {
                    self.rescan_drives();
                    self.drive_space.remove(&path);
                    self.notice = Some(format!("You can now safely remove {path}"));
                }
                Msg::ShellNewItems(items) => {
                    self.shell_new_loading = false;
                    self.shell_new_items = Some(items);
                }
                Msg::VirtualDropDone { dir, created, errors } => {
                    self.history.record(Entry::Create(created));
                    if dir == self.cwd {
                        self.load_listing();
                    }
                    if !errors.is_empty() {
                        self.error = Some(errors.join("\n"));
                    }
                }
                Msg::Extracted { created, open, errors } => {
                    self.notice = None;
                    self.history.record(Entry::Create(created));
                    if !errors.is_empty() {
                        self.error = Some(errors.join("\n"));
                    }
                    if let Some(path) = open {
                        self.navigate(path, true);
                    }
                }
                Msg::Recycled(paths) => self.history.record(Entry::Recycle(paths)),
                Msg::HistoryDone { entry, undo, result } => {
                    self.history.busy = false;
                    self.notice = None;
                    match result {
                        Ok(()) => {
                            if let Entry::Rename { from, to } = &entry {
                                // Keep recent files and the selection on the renamed item.
                                let (was, now) = if undo { (to, from) } else { (from, to) };
                                crate::recent::renamed(was, now);
                                if fsys::parent(now).is_some_and(|d| d.eq_ignore_ascii_case(&self.cwd)) {
                                    self.pending_select = now.rsplit('\\').next().map(Into::into);
                                }
                            }
                            if undo {
                                self.history.undone(entry);
                            } else {
                                self.history.redone(entry);
                            }
                        }
                        Err(e) => {
                            let verb = if undo { "Undo" } else { "Redo" };
                            self.error = Some(format!("{verb} of {} failed: {e}", entry.describe()));
                            self.history.put_back(entry, undo);
                        }
                    }
                }
                Msg::RecentChanged if self.cwd == RECENT => self.load_listing(),
                Msg::Navigate(p) => self.navigate(p, true),
                Msg::Created { dir, name } => {
                    self.history.record(Entry::Create(vec![fsys::join(&dir, &name)]));
                    if dir != self.cwd {
                        continue;
                    }
                    // Don't wait for the watcher: show it and start renaming now.
                    if self.listing.iter().all(|r| !r.name.eq_ignore_ascii_case(&name)) {
                        if let Some(st) = fileop::stat(&fsys::join(&dir, &name)) {
                            self.apply_changes(vec![Change::Upsert(Row {
                                name: name.as_str().into(),
                                dir: None,
                                size: 0,
                                mtime: st.mtime,
                                attrs: st.attrs,
                                sel: false,
                            })]);
                        }
                    }
                    self.pending_select = Some(name.into());
                    self.pending_rename = true;
                    self.rebuild_view();
                }
                Msg::Renamed { old_path, new_name } => {
                    if let Some(parent) = fsys::parent(&old_path) {
                        let new_path = fsys::join(&parent, &new_name);
                        crate::recent::renamed(&old_path, &new_path);
                        self.history.record(Entry::Rename { from: old_path.clone(), to: new_path });
                    }
                    for r in self.listing.iter_mut().chain(self.results.iter_mut()) {
                        if r.full_path(&self.cwd).eq_ignore_ascii_case(&old_path) {
                            r.name = new_name.as_str().into();
                        }
                    }
                    self.rebuild_view();
                }
                Msg::Error(e) => {
                    self.notice = None;
                    self.error = Some(e);
                }
                Msg::Connections(c) => self.net.connections = c,
                Msg::Shares { server, result } => {
                    self.net.shares.insert(server.to_lowercase(), Some(result));
                }
                Msg::NetDone { result, open } => {
                    self.net.busy = false;
                    self.rescan_drives();
                    self.refresh_connections();
                    match (result, open) {
                        (Ok(()), Some(path)) => self.navigate(path, true),
                        (Ok(()), None) if self.cwd != NETWORK => self.refresh(),
                        (Ok(()), None) => {}
                        (Err(e), _) => self.error = Some(e),
                    }
                }
                _ => {} // stale generation
            }
        }
    }

    // ------------------------------------------------------------- keyboard

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));

        if pressed(Modifiers::NONE, Key::F5) {
            self.refresh();
        }
        if pressed(Modifiers::NONE, Key::F1) {
            self.help_open = !self.help_open;
        }
        if pressed(Modifiers::CTRL, Key::H) {
            self.toggle_hidden();
        }
        if pressed(Modifiers::CTRL, Key::L) || pressed(Modifiers::ALT, Key::D) {
            self.path_editing = true;
        }
        if pressed(Modifiers::CTRL, Key::N) {
            self.open_window(&self.cwd.clone(), None);
        }
        if pressed(Modifiers::CTRL, Key::F) || pressed(Modifiers::NONE, Key::F3) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique("search_bar")));
        }
        let (mouse_back, mouse_fwd) = ctx.input(|i| (i.pointer.button_pressed(PointerButton::Extra1), i.pointer.button_pressed(PointerButton::Extra2)));
        if mouse_back {
            self.go_back();
        }
        if mouse_fwd {
            self.go_forward();
        }
        // Tabs. Shifted variants first: a Ctrl pattern also matches Ctrl+Shift.
        if pressed(Modifiers::CTRL, Key::T) {
            self.open_tab(self.cwd.clone());
        }
        if pressed(Modifiers::CTRL, Key::W) {
            self.close_tab(self.active_tab);
        }
        if pressed(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab) {
            self.cycle_tab(-1);
        }
        if pressed(Modifiers::CTRL, Key::Tab) {
            self.cycle_tab(1);
        }
        let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
        for (n, key) in digits.into_iter().enumerate() {
            if pressed(Modifiers::CTRL, key) {
                // Ctrl+9 is always the last tab, as in browsers.
                self.switch_tab(if n == 8 { self.tabs.len() - 1 } else { n });
            }
        }
        if typing || self.confirm_delete.is_some() || self.net.map.is_some() || self.letter_dialog.is_some() || self.jobs.iter().any(|j| j.phase() == Phase::AwaitingDecision) {
            return;
        }
        if self.cwd == NETWORK {
            if pressed(Modifiers::ALT, Key::ArrowLeft) || pressed(Modifiers::NONE, Key::Backspace) {
                self.go_back();
            }
            if pressed(Modifiers::ALT, Key::ArrowRight) {
                self.go_forward();
            }
            return;
        }

        // Type-to-find: printable keys build a query; Backspace edits it
        // while it's live, Escape drops it.
        let live = self.typeahead.as_ref().is_some_and(|t| t.last.elapsed() < TYPEAHEAD_RESET);
        if !live {
            self.typeahead = None;
        }
        let typed: String = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect()
        });
        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
        if !typed.is_empty() && !(typed == " " && !live) {
            let mut text = self.typeahead.take().map(|t| t.text).unwrap_or_default();
            text.push_str(&typed);
            self.type_to_find(text);
            return;
        }
        if live {
            if pressed(Modifiers::NONE, Key::Backspace) {
                let mut text = self.typeahead.take().map(|t| t.text).unwrap_or_default();
                text.pop();
                if !text.is_empty() {
                    self.type_to_find(text);
                }
                return;
            }
            if pressed(Modifiers::NONE, Key::Escape) {
                self.typeahead = None;
                return;
            }
        }

        if pressed(Modifiers::CTRL | Modifiers::SHIFT, Key::Z) || pressed(Modifiers::CTRL, Key::Y) {
            self.redo();
        }
        if pressed(Modifiers::CTRL, Key::Z) {
            self.undo();
        }
        if pressed(Modifiers::NONE, Key::Escape) {
            if !self.query.is_empty() {
                self.query.clear();
                self.apply_query();
            } else {
                self.clear_sel();
            }
        }
        if pressed(Modifiers::ALT, Key::ArrowLeft) {
            self.go_back();
        }
        if pressed(Modifiers::ALT, Key::ArrowRight) {
            self.go_forward();
        }
        if pressed(Modifiers::ALT, Key::ArrowUp) || pressed(Modifiers::NONE, Key::Backspace) {
            self.go_up();
        }
        if pressed(Modifiers::ALT, Key::Enter) {
            self.open_properties();
            return;
        }
        if pressed(Modifiers::NONE, Key::Enter) {
            if let Some(i) = self.focus {
                self.open(i);
                return;
            }
        }
        if pressed(Modifiers::CTRL, Key::S) {
            if let Some(i) = self.focus.filter(|&i| self.row(i).is_some_and(|r| !r.is_dir())) {
                self.run_as(i);
            }
            return;
        }
        if pressed(Modifiers::CTRL, Key::C) {
            self.clip_selection(false);
        }
        if pressed(Modifiers::CTRL, Key::X) {
            self.clip_selection(true);
        }
        if pressed(Modifiers::CTRL, Key::V) {
            self.paste(None);
        }
        if pressed(Modifiers::CTRL | Modifiers::SHIFT, Key::N) {
            self.new_folder();
        }
        if pressed(Modifiers::CTRL | Modifiers::ALT, Key::N) {
            self.new_file();
        }
        if pressed(Modifiers::NONE, Key::F2) {
            if let Some(i) = self.focus.filter(|&i| self.row(i).is_some_and(|r| r.sel)) {
                self.begin_rename(i);
            }
        }
        let permanent = pressed(Modifiers::SHIFT, Key::Delete);
        if !crate::archive::is_location(&self.cwd) && (permanent || pressed(Modifiers::NONE, Key::Delete)) {
            let paths = self.selected_paths();
            if !paths.is_empty() {
                self.confirm_delete = Some((paths, permanent));
            }
        }
        let n = self.view.len();
        if n == 0 {
            return;
        }
        if pressed(Modifiers::CTRL, Key::A) {
            for vi in 0..n {
                self.set_sel(vi, true);
            }
        }

        let page = ((self.view_h / ROW_H) as usize).max(1);
        let shift = ctx.input(|i| i.modifiers.shift);
        let mods = if shift { Modifiers::SHIFT } else { Modifiers::NONE };
        let cur = self.focus;
        let moved = if pressed(mods, Key::ArrowDown) {
            Some(cur.map_or(0, |i| (i + 1).min(n - 1)))
        } else if pressed(mods, Key::ArrowUp) {
            Some(cur.map_or(0, |i| i.saturating_sub(1)))
        } else if pressed(mods, Key::PageDown) {
            Some(cur.map_or(0, |i| (i + page).min(n - 1)))
        } else if pressed(mods, Key::PageUp) {
            Some(cur.map_or(0, |i| i.saturating_sub(page)))
        } else if pressed(mods, Key::Home) {
            Some(0)
        } else if pressed(mods, Key::End) {
            Some(n - 1)
        } else {
            None
        };
        if let Some(i) = moved {
            if shift {
                let anchor = self.anchor.unwrap_or(i);
                self.clear_sel();
                self.select_range(anchor, i);
                self.anchor = Some(anchor);
                self.focus = Some(i);
            } else {
                self.select_only(i);
            }
            self.scroll_to = Some(i);
        }
    }

    // ------------------------------------------------------------------- UI

    /// One row: navigation, the path, search, and a menu. Everything else
    /// lives on the keyboard and the right-click menus (F1 lists the keys).
    fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(3.0);
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
            let can_go_up = if archive::is_location(&self.cwd) { archive::parent_location(&self.cwd).is_some() } else { fsys::parent(&self.cwd).is_some() };
            if nav(ui, can_go_up, "⬆", "Up (Alt+↑)") {
                self.go_up();
            }
            let writable = self.cwd != RECENT && self.cwd != NETWORK && !archive::is_location(&self.cwd) && !fsys::is_server_root(&self.cwd);
            let mut new_action = None;
            ui.add_enabled_ui(writable, |ui| {
                ui.menu_button("➕", |ui| self.new_menu(ui, &mut new_action))
                    .response
                    .on_hover_text("New folder or file (Ctrl+Shift+N, Ctrl+Alt+N)");
            });
            self.run_action(new_action, ui);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("☰", |ui| self.main_menu(ui));

                let hint = match (self.regex_mode, self.recursive) {
                    (false, true) => "Search",
                    (false, false) => "Filter this folder",
                    (true, true) => "Search (regex)",
                    (true, false) => "Filter this folder (regex)",
                };
                let mut search = TextEdit::singleline(&mut self.query).id(egui::Id::unique("search_bar")).hint_text(hint).desired_width(200.0);
                if self.query_error.is_some() {
                    search = search.text_color(RED);
                }
                let resp = ui.add(search);
                let tip = self.query_error.clone().unwrap_or_else(|| "Keywords, *.ext wildcards, type:ext (Ctrl+F)".into());
                let resp = resp.on_hover_text(tip);
                if resp.changed() {
                    self.query_dirty = Some(Instant::now());
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.query.clear();
                    self.apply_query();
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && self.query_dirty.is_some() {
                    self.apply_query();
                }

                self.path_bar(ui);
            });
        });
        ui.add_space(3.0);
    }

    /// The ☰ menu: view options and windows.
    fn main_menu(&mut self, ui: &mut Ui) {
        if ui.add(Button::new("New tab").shortcut_text("Ctrl+T")).clicked() {
            self.open_tab(self.cwd.clone());
            ui.close();
        }
        if ui.add(Button::new("New window").shortcut_text("Ctrl+N")).clicked() {
            self.open_window(&self.cwd.clone(), None);
            ui.close();
        }
        ui.separator();
        if ui.add(Button::selectable(self.show_hidden, "Show hidden files").shortcut_text("Ctrl+H")).clicked() {
            self.toggle_hidden();
        }
        let sub = ui.add(Button::selectable(self.recursive, "Search subfolders")).on_hover_text("Search below this folder (builds an in-memory index)");
        let rx = ui.add(Button::selectable(self.regex_mode, "Regular expressions"));
        if sub.clicked() {
            self.recursive = !self.recursive;
        }
        if rx.clicked() {
            self.regex_mode = !self.regex_mode;
        }
        if sub.clicked() || rx.clicked() {
            self.apply_query();
        }
        if ui
            .add(Button::selectable(self.report_network_space, "Network drive capacity"))
            .on_hover_text("Show free space for mapped network drives (queries can be slow)")
            .clicked()
        {
            self.report_network_space = !self.report_network_space;
        }
        ui.separator();
        if ui.add(Button::new("Keyboard shortcuts").shortcut_text("F1")).clicked() {
            self.help_open = true;
            ui.close();
        }
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_view();
    }

    /// Breadcrumbs for the open location: click one to go there, middle-click
    /// for a new tab, drop items on one to move them there. Clicking the empty
    /// part (or Ctrl+L) switches to typing a path.
    fn path_bar(&mut self, ui: &mut Ui) {
        let path_id = egui::Id::unique("path_bar");
        let focused = ui.memory(|m| m.has_focus(path_id));
        if !focused && !self.path_editing && self.path_text != self.cwd {
            self.path_text = self.cwd.clone();
        }
        if focused || self.path_editing {
            let resp = ui.add(TextEdit::singleline(&mut self.path_text).id(path_id).desired_width(ui.available_width()));
            if std::mem::take(&mut self.path_editing) {
                resp.request_focus();
                let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.path_text.chars().count()));
                let mut state = TextEdit::load_state(ui.ctx(), path_id).unwrap_or_default();
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), path_id);
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.submit_path();
            }
            return;
        }

        let height = ui.spacing().interact_size.y;
        let (rect, bar) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
        let visuals = ui.visuals().clone();
        ui.painter().rect(rect, 2.0, visuals.extreme_bg_color, visuals.widgets.inactive.bg_stroke, egui::StrokeKind::Inside);
        let font = egui::TextStyle::Button.resolve(ui.style());
        let crumbs = breadcrumbs(&self.cwd);
        let galleys: Vec<_> = crumbs.iter().map(|(label, _)| ui.painter().layout_no_wrap(label.clone(), font.clone(), visuals.text_color())).collect();
        let sep = ui.painter().layout_no_wrap("›".into(), font.clone(), visuals.weak_text_color());
        let (pad, gap) = (6.0, sep.size().x + 4.0);
        // Show as many trailing segments as fit; earlier ones collapse into "…".
        let ellipsis = ui.painter().layout_no_wrap("… ›".into(), font.clone(), visuals.weak_text_color());
        let mut first = crumbs.len().saturating_sub(1);
        let mut width = galleys.last().map_or(0.0, |g| g.size().x + 2.0 * pad);
        while first > 0 {
            let w = galleys[first - 1].size().x + 2.0 * pad + gap;
            let reserve = if first > 1 { ellipsis.size().x + 8.0 } else { 0.0 };
            if width + w + reserve > rect.width() - 8.0 {
                break;
            }
            width += w;
            first -= 1;
        }
        let painter = ui.painter_at(rect);
        let mut x = rect.left() + 2.0;
        if first > 0 {
            let w = ellipsis.size().x;
            painter.galley(egui::pos2(x + 4.0, rect.center().y - ellipsis.size().y / 2.0), ellipsis, visuals.weak_text_color());
            x += 8.0 + w;
        }
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let (mut go, mut tab) = (None, None);
        for (i, ((_, location), galley)) in crumbs.iter().zip(galleys).enumerate().skip(first) {
            let r = Rect::from_min_size(egui::pos2(x, rect.top() + 1.0), egui::vec2(galley.size().x + 2.0 * pad, height - 2.0)).intersect(rect);
            let resp = ui.interact(r, ui.scope_id().with(("crumb", i)), Sense::click()).on_hover_text(location.as_str());
            let droppable = accepts_files(location);
            let drop_here = self.drag.is_some() && droppable && pointer.is_some_and(|p| r.contains(p));
            if drop_here {
                self.drop_target = Some(location.clone());
            }
            if droppable {
                self.zones.push((r, location.clone()));
            }
            if drop_here || resp.hovered() {
                painter.rect_filled(r, 2.0, visuals.widgets.hovered.weak_bg_fill);
            }
            if drop_here {
                painter.rect_stroke(r, 2.0, Stroke::new(1.5, visuals.selection.stroke.color), egui::StrokeKind::Inside);
            }
            let color = if i + 1 == crumbs.len() { visuals.strong_text_color() } else { visuals.text_color() };
            painter.galley(egui::pos2(r.left() + pad, rect.center().y - galley.size().y / 2.0), galley, color);
            if resp.clicked() && *location != self.cwd {
                go = Some(location.clone());
            } else if resp.middle_clicked() {
                tab = Some(location.clone());
            }
            x = r.right();
            if i + 1 < crumbs.len() {
                painter.galley(egui::pos2(x + 2.0, rect.center().y - sep.size().y / 2.0), sep.clone(), visuals.weak_text_color());
                x += gap;
            }
        }
        if bar.clicked() {
            self.path_editing = true;
        }
        bar.on_hover_cursor(egui::CursorIcon::Text);
        if let Some(location) = go {
            self.navigate(location, true);
        } else if let Some(location) = tab {
            self.open_tab(location);
        }
    }

    fn request_drive_space(&mut self, path: &str, report_network: bool, known_network: bool) {
        if self.drive_space_pending.contains(path)
            || self.drive_space.get(path).is_some_and(|space| {
                space.checked.elapsed() < Duration::from_secs(60)
                    && (!space.is_network || !report_network || space.queried)
            })
        {
            return;
        }
        self.drive_space_pending.insert(path.to_string());
        let target = self.volumes.letter(path).map(|d| d.target.clone());
        let (path, failed, failed_target) = (path.to_string(), path.to_string(), target.clone());
        self.spawn_io_or(
            move || {
                let is_network = known_network || fileop::is_network(&path);
                let queried = !is_network || report_network;
                let ejectable = !is_network && fileop::ejectable(&path);
                let result = queried.then(|| win::drive_space(&path)).flatten();
                Some(Msg::DriveSpace { path, target, is_network, queried, ejectable, result })
            },
            move |_| Msg::DriveSpace {
                path: failed,
                target: failed_target,
                is_network: known_network,
                queried: true,
                ejectable: false,
                result: None,
            },
        );
    }

    /// The mapped connection shown for drive `root`. What the letter points
    /// to in this session decides: a local drive on a letter that is also
    /// remembered for a disconnected mapping (Windows gave a USB drive that
    /// letter) is not a network drive.
    fn mapping_of(&self, root: &str) -> Option<&net::Connection> {
        let on_letter = |c: &&net::Connection| c.local.as_deref().is_some_and(|l| root.starts_with(l));
        match self.volumes.letter(root).map(|d| d.network.is_some()) {
            Some(false) => None,
            Some(true) => self.net.connections.iter().find(on_letter),
            None => self.net.connections.iter().filter(|c| c.connected).find(on_letter),
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let drives = self.drives.clone();
        let hidden = self.volumes.hidden.clone();
        for drive in &drives {
            let known_network = self.mapping_of(drive).is_some() || self.volumes.letter(drive).is_some_and(|d| d.network.is_some());
            self.request_drive_space(drive, self.report_network_space, known_network);
        }
        let mut target = None;
        let mut tab_target = None;
        let mut action = None;
        let dragging = self.drag.is_some();
        let mut hover = None;
        let mut zones = Vec::new();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut entry = |ui: &mut Ui, label: String, path: &String, tip: &str, cwd: &str| -> egui::Response {
                let droppable = accepts_files(path);
                let hovered_drop = dragging && droppable && self.drop_target.as_deref() == Some(path.as_str());
                let resp = ui.selectable_label(cwd == path || hovered_drop, label).on_hover_text(tip);
                if droppable {
                    zones.push((resp.rect.intersect(ui.clip_rect()), path.clone()));
                }
                if dragging && droppable && resp.contains_pointer() {
                    hover = Some(path.clone());
                }
                if resp.clicked() {
                    target = Some(path.clone());
                } else if resp.middle_clicked() {
                    tab_target = Some(path.clone());
                }
                resp
            };
            ui.add_space(4.0);
            ui.weak("Quick access");
            entry(ui, "◷ Recent".into(), &RECENT.to_string(), "Recently opened files", &self.cwd);
            for (label, path) in &self.quick {
                entry(ui, format!("🗀 {label}"), path, path, &self.cwd);
            }
            ui.add_space(8.0);
            ui.weak("Drives");
            for d in &drives {
                let info = self.volumes.letter(d);
                let remote = self.mapping_of(d).map(|c| c.remote.clone()).or_else(|| info.and_then(|i| i.network.clone()));
                let is_network = remote.is_some() || self.drive_space.get(d).is_some_and(|space| space.is_network);
                let include_space = !is_network || self.report_network_space;
                let (label, tip) = match &remote {
                    Some(r) => {
                        let share = r.rsplit('\\').next().unwrap_or("");
                        (format!("🖧 {d} {share}"), r.clone())
                    }
                    None => (format!("🖴 {d}"), d.clone()),
                };
                // A remembered mapping that can't reconnect while this drive holds its letter.
                let clash = info.filter(|i| i.volume.is_some()).and_then(|_| {
                    self.net.connections.iter().find(|c| !c.connected && c.local.as_deref().is_some_and(|l| d.starts_with(l)))
                });
                ui.horizontal(|ui| {
                    let response = entry(ui, label, d, &tip, &self.cwd);
                    let ejectable = remote.is_none() && self.drive_space.get(d).is_some_and(|space| space.ejectable);
                    let why = clash.map(|c| {
                        format!("{d} is also remembered for the network drive {}, which can't reconnect while this drive uses the letter.", c.remote)
                    });
                    let system = std::env::var("SystemDrive").is_ok_and(|s| d.to_ascii_uppercase().starts_with(&s.to_ascii_uppercase()));
                    let change = info.and_then(|i| i.volume.clone()).filter(|_| !system && (ejectable || clash.is_some())).map(|volume| Action::ChangeLetter {
                        volume,
                        name: d.trim_end_matches('\\').to_string(),
                        current: d.chars().next(),
                        why: why.clone(),
                    });
                    response.context_menu(|ui| {
                        if ui.button("Open in new tab").clicked() {
                            action = Some(Action::OpenInNewTab(d.clone()));
                            ui.close();
                        }
                        if ejectable && ui.button("Safely eject").clicked() {
                            action = Some(Action::EjectDrive(d.clone()));
                            ui.close();
                        }
                        if let Some(change) = &change
                            && ui.button("Change drive letter…").clicked()
                        {
                            action = Some(change.clone());
                            ui.close();
                        }
                    });
                    if let (Some(why), Some(change)) = (&why, &change)
                        && ui.small_button("⚠").on_hover_text(format!("{why}\nClick to give this drive another letter.")).clicked()
                    {
                        action = Some(change.clone());
                    }
                    if ejectable && ui.small_button("⏏").on_hover_text(format!("Safely eject {d}")).clicked() {
                        action = Some(Action::EjectDrive(d.clone()));
                    }
                    if include_space
                        && let Some((free, total)) = self.drive_space.get(d).and_then(|space| space.capacity.as_ref())
                        && *total > 0
                    {
                        let used_fraction = (total.saturating_sub(*free) as f64 / *total as f64) as f32;
                        let (rect, response) = ui.allocate_exact_size(egui::vec2(36.0, ROW_H), Sense::hover());
                        let line = Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 2.0));
                        let visuals = ui.visuals();
                        ui.painter().rect_filled(line, 0.0, visuals.widgets.inactive.bg_fill);
                        let fill = Rect::from_min_max(line.min, egui::pos2(line.left() + line.width() * used_fraction, line.bottom()));
                        ui.painter().rect_filled(fill, 0.0, visuals.selection.bg_fill);
                        response.on_hover_text(format!(
                            "{} free of {}",
                            fsys::format_size(*free),
                            fsys::format_size(*total)
                        ));
                    }
                });
            }
            // Drives no letter reaches here: shadowed by a network drive, or never given one.
            for h in &hidden {
                let label = match h.letter {
                    Some(l) => format!("⚠ {} ({l}: is taken)", h.name()),
                    None => format!("⚠ {} (no letter)", h.name()),
                };
                let why = h.reason();
                let response = ui.selectable_label(false, label).on_hover_text(format!("{why}.\nClick to give it a free drive letter."));
                if response.clicked() {
                    action = Some(Action::ChangeLetter { volume: h.volume.clone(), name: h.name(), current: h.letter, why: Some(format!("{why}.")) });
                }
            }
            ui.add_space(8.0);
            entry(ui, "🖧 Network".into(), &NETWORK.to_string(), "Shares, mapped drives and network scan", &self.cwd);
            for s in &self.servers {
                entry(ui, format!("    {}", s.trim_start_matches('\\')), s, s, &self.cwd);
            }
        });
        self.zones.extend(zones);
        if hover.is_some() {
            self.drop_target = hover;
        }
        if action.is_some() {
            self.run_action(action, ui);
        }
        if let Some(t) = target {
            self.navigate(t, true);
        }
        if let Some(t) = tab_target.filter(|t| t != NETWORK) {
            self.open_tab(t);
        }
    }

    fn tab_location(&self, i: usize) -> String {
        if i == self.active_tab { self.cwd.clone() } else { self.tabs.get(i).map(|t| t.location.clone()).unwrap_or_default() }
    }

    /// Move tab `from` to position `to`, keeping the same tab active.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        self.sync_active_tab();
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        let a = self.active_tab;
        self.active_tab = if a == from {
            to
        } else if from < a && a <= to {
            a - 1
        } else if to <= a && a < from {
            a + 1
        } else {
            a
        };
    }

    /// Tabs: click to switch, drag to reorder or tear off, middle-click to
    /// close. While items are dragged (from here or another app), a tab is a
    /// drop target and opens after a moment's hover.
    fn tab_strip(&mut self, ui: &mut Ui) {
        let (mut switch, mut close, mut close_others) = (None, None, None);
        let (mut drag_start, mut spring, mut drop_on) = (None, None, None);
        let mut rects = Vec::with_capacity(self.tabs.len());
        let mut zones = Vec::new();
        // Where something being carried is: an item of ours, or files from another app.
        let carried = if self.drag.is_some() {
            ui.input(|i| i.pointer.hover_pos())
        } else {
            self.drops.as_ref().and_then(|d| d.hover()).filter(|h| h.payload != oledrop::Payload::Tab).map(|h| h.pos)
        };
        let stroke = Stroke::new(1.5, ui.visuals().selection.stroke.color);
        ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let active = i == self.active_tab;
                    let location = if active { &self.cwd } else { &tab.location };
                    let dragged = self.tab_drag.as_ref().is_some_and(|d| d.index == i);
                    let group = ui.horizontal(|ui| {
                        let resp = ui
                            .selectable_label(active || dragged, tab_label(location))
                            .on_hover_text(location.as_str())
                            .interact(Sense::drag());
                        if resp.clicked() {
                            switch = Some(i);
                        } else if resp.middle_clicked() {
                            close = Some(i);
                        } else if resp.drag_started_by(PointerButton::Primary) {
                            drag_start = Some(i);
                        }
                        resp.context_menu(|ui| {
                            if ui.add(Button::new("Close tab").shortcut_text("Ctrl+W")).clicked() {
                                close = Some(i);
                                ui.close();
                            }
                            if ui.button("Close other tabs").clicked() {
                                close_others = Some(i);
                                ui.close();
                            }
                        });
                        // Only the current tab shows its close button; middle-click closes any.
                        if active && ui.add(Button::new("✕").frame(false).small()).on_hover_text("Close tab (Ctrl+W)").clicked() {
                            close = Some(i);
                        }
                    });
                    let r = group.response.rect;
                    rects.push(r);
                    let droppable = accepts_files(location);
                    if droppable {
                        zones.push((r.intersect(ui.clip_rect()), location.clone()));
                    }
                    if carried.is_some_and(|p| r.contains(p)) {
                        spring = Some(i);
                        if droppable {
                            drop_on = Some(location.clone());
                            ui.painter().rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
                        }
                    }
                    ui.add_space(4.0);
                }
            });
        });
        self.tab_rects = rects;
        self.zones.extend(zones);
        if self.drag.is_some() && drop_on.is_some() {
            self.drop_target = drop_on;
        }
        // Spring-loading: rest a dragged item on a tab to open it and drop inside.
        match spring.filter(|&i| i != self.active_tab) {
            Some(i) => match self.tab_spring {
                Some((j, since)) if j == i && since.elapsed() >= TAB_SPRING => {
                    self.tab_spring = None;
                    switch = Some(i);
                }
                Some((j, since)) if j == i => ui.ctx().request_repaint_after(TAB_SPRING.saturating_sub(since.elapsed())),
                _ => {
                    self.tab_spring = Some((i, Instant::now()));
                    ui.ctx().request_repaint_after(TAB_SPRING);
                }
            },
            None => self.tab_spring = None,
        }
        if let Some(i) = drag_start {
            self.tab_drag = Some(TabDrag { index: i });
        }
        match (switch, close, close_others) {
            (Some(i), _, _) => self.switch_tab(i),
            (_, Some(i), _) => self.close_tab(i),
            (_, _, Some(i)) => self.close_other_tabs(i),
            _ => {}
        }
    }

    /// Follow a dragged tab: reorder within the strip; released away from the
    /// strip, it opens in a new window there; leaving the window hands it to
    /// Windows, so another FEx window can take it.
    fn update_tab_drag(&mut self, ctx: &egui::Context) {
        let Some(index) = self.tab_drag.as_ref().map(|d| d.index).filter(|&i| i < self.tabs.len()) else {
            self.tab_drag = None;
            return;
        };
        let (pos, down) = ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
        let Some(p) = pos else { return };
        if !ctx.content_rect().contains(p) && down {
            self.tab_drag = None;
            self.drag_tab_out(index);
            return;
        }
        let strip = self.tab_rects.iter().copied().reduce(|a, b| a.union(b));
        let near_strip = strip.is_some_and(|s| (p.y - s.center().y).abs() < s.height() / 2.0 + 30.0);
        if !down {
            self.tab_drag = None;
            if !near_strip && self.tabs.len() > 1 {
                let origin = ctx.input(|i| i.viewport().inner_rect.map(|r| r.min)).unwrap_or_default();
                let location = self.tab_location(index);
                if self.open_window(&location, Some([origin.x + p.x - 80.0, origin.y + p.y - 12.0])) {
                    self.close_tab(index);
                }
            }
            return;
        }
        if near_strip {
            if let Some(j) = self.tab_rects.iter().position(|r| r.x_range().contains(p.x)) {
                let r = self.tab_rects[j];
                // Past the neighbour's middle, so the swap doesn't flip back.
                if (j > index && p.x > r.center().x) || (j < index && p.x < r.center().x) {
                    self.move_tab(index, j);
                    self.tab_drag = Some(TabDrag { index: j });
                }
            }
            return;
        }
        let text = format!("🗐 {} — release to open in a new window", tab_label(&self.tab_location(index)));
        egui::Area::new(egui::Id::unique("tab_drag_label"))
            .order(egui::Order::Tooltip)
            .fixed_pos(p + egui::vec2(16.0, 12.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
            });
    }

    /// The tab left the window: another FEx window may take it; released
    /// elsewhere, it becomes a window of its own.
    fn drag_tab_out(&mut self, index: usize) {
        let Some(hwnd) = self.hwnd else { return };
        let location = self.tab_location(index);
        let result = oledrag::drag_tab(hwnd, &location);
        self.release_pointer = true;
        self.ctx.request_repaint();
        match result {
            Ok(oledrag::TabDrop::Taken) if self.tabs.len() > 1 => self.close_tab(index),
            Ok(oledrag::TabDrop::Taken) => self.ctx.send_viewport_cmd(ViewportCommand::Close),
            Ok(oledrag::TabDrop::Released) => {
                let (x, y) = win::cursor_pos();
                if self.tabs.len() > 1 && !win::window_contains(hwnd, x, y) {
                    let ppp = self.ctx.pixels_per_point();
                    if self.open_window(&location, Some([x as f32 / ppp - 80.0, y as f32 / ppp - 12.0])) {
                        self.close_tab(index);
                    }
                }
            }
            Ok(oledrag::TabDrop::Cancelled) => {}
            Err(e) => self.error = Some(e),
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if self.cwd == NETWORK {
                let mapped = self.net.connections.iter().filter(|c| c.local.is_some()).count();
                ui.label(format!("{mapped} mapped drives · {} remembered servers", self.servers.len()));
            } else if self.in_results() {
                let shown = self.view.len();
                if self.results_total > shown {
                    ui.label(format!("{shown} shown of {} matches", self.results_total));
                } else {
                    ui.label(format!("{} matches", self.results_total));
                }
            } else if self.cwd == RECENT {
                ui.label(format!("{} recent files", self.view.len()));
            } else {
                let resp = if self.loading { ui.weak("Loading…") } else { ui.label(format!("{} items", self.view.len())) };
                resp.on_hover_text(format!("Listed in {:.1} ms", self.list_time.as_secs_f64() * 1e3));
            }
            let sel = self.selected();
            if !sel.is_empty() {
                let bytes: u64 = sel.iter().filter(|r| !r.is_dir()).map(|r| r.size).sum();
                ui.weak(format!("· {} selected, {}", sel.len(), fsys::format_size(bytes)));
            }
            if let Some(e) = &self.error {
                ui.colored_label(RED, format!("· {e}"));
            } else if let Some(n) = &self.notice {
                ui.weak(format!("· {n}"));
            }

            // Search details stay out of the way: one short word, the rest on hover.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                crate::update::status_widget(ui, &self.updater);
                if let Some(s) = self.service.as_ref().filter(|s| !self.service_failed && s.covers(&self.cwd)) {
                    let paused = if s.paused.is_empty() { String::new() } else { format!(" (paused: {})", s.paused) };
                    ui.weak("Index service").on_hover_text(format!("{} entries · {}{paused}\n{}", s.entries, s.activity, s.reserve));
                } else if let Some(b) = &self.build {
                    ui.weak(format!("Indexing… {} entries", b.entries.load(Relaxed))).on_hover_text(format!(
                        "Indexing {}: {} folders in {:.1}s",
                        b.root,
                        b.dirs_done.load(Relaxed),
                        b.started.elapsed().as_secs_f32()
                    ));
                } else if let Some(live) = self.indexes.iter().find(|l| l.covers(&self.cwd)).or(self.indexes.first()) {
                    // Never wait on the index here: it may be busy applying changes.
                    let Some(idx) = index::try_read(&live.index) else {
                        ui.weak("Updating index…");
                        return;
                    };
                    let age = idx.built_at.elapsed().as_secs();
                    let age = if age < 60 { format!("{age}s") } else { format!("{}m", age / 60) };
                    let problems = match (idx.errors, idx.worker_errors) {
                        (0, 0) => String::new(),
                        (unreadable, 0) => format!("\n{unreadable} folders unreadable"),
                        (0, workers) => format!("\n{workers} index workers failed"),
                        (unreadable, workers) => format!("\n{unreadable} folders unreadable, {workers} index workers failed"),
                    };
                    let searched = match (self.in_results(), self.search_time) {
                        (true, Some(t)) => format!("Searched in {:.1} ms\n", t.as_secs_f64() * 1e3),
                        _ => String::new(),
                    };
                    let word = if self.searching { "Searching…" } else { "Indexed" };
                    let kept = if idx.loaded { "saved copy, loaded" } else { "kept up to date since it was built" };
                    ui.weak(word).on_hover_text(format!(
                        "{searched}Index of {}: {} entries, built in {:.1}s, {age} ago ({kept}){problems}\nF5 rebuilds it",
                        idx.root,
                        idx.entries,
                        idx.build_time.as_secs_f32()
                    ));
                }
            });
        });
    }

    fn jobs_panel(&mut self, ui: &mut Ui) {
        let mut dismiss = Vec::new();
        for (n, job) in self.jobs.iter().enumerate() {
            ui.horizontal(|ui| {
                let verb = if job.kind == Kind::Move { ("Moving", "Moved") } else { ("Copying", "Copied") };
                let dest = &job.dest;
                let files_total = job.files_total.load(Relaxed);
                let files_done = job.files_done.load(Relaxed);
                let (bytes_total, bytes_done) = (job.bytes_total.load(Relaxed), job.bytes_done.load(Relaxed));
                let errors = job.error_count();
                match job.phase() {
                    Phase::Planning => {
                        ui.label(format!("Preparing: {} {} item(s) to {dest}…", verb.0.to_lowercase(), job.sources.len()));
                    }
                    Phase::AwaitingDecision => {
                        ui.label(format!("{} files already exist in {dest}", job.conflicts.load(Relaxed)));
                    }
                    Phase::Transferring => {
                        let elapsed = job.started.elapsed().as_secs_f64().max(0.001);
                        let rate = bytes_done as f64 / elapsed;
                        let frac = if bytes_total > 0 {
                            bytes_done as f32 / bytes_total as f32
                        } else if files_total > 0 {
                            files_done as f32 / files_total as f32
                        } else {
                            0.0
                        };
                        let eta = if rate > 0.0 && bytes_total > bytes_done {
                            let s = ((bytes_total - bytes_done) as f64 / rate) as u64;
                            format!(" · {}:{:02} left", s / 60, s % 60)
                        } else {
                            String::new()
                        };
                        ui.add(egui::ProgressBar::new(frac).desired_width(160.0).show_percentage());
                        ui.label(format!(
                            "{} {files_done}/{files_total} files · {} of {} · {}/s{eta} · {} threads → {dest}",
                            verb.0,
                            fsys::format_size(bytes_done),
                            fsys::format_size(bytes_total),
                            fsys::format_size(rate as u64),
                            job.threads.load(Relaxed),
                        ));
                    }
                    Phase::Done | Phase::Cancelled => {
                        let took = job.elapsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).unwrap_or_default().as_secs_f32();
                        let what = if job.phase() == Phase::Cancelled { "Cancelled" } else { verb.1 };
                        let items = if files_done == 1 { "1 item".to_string() } else { format!("{files_done} items") };
                        let bytes = if bytes_done > 0 { format!(" ({})", fsys::format_size(bytes_done)) } else { String::new() };
                        ui.label(format!("{what} {items}{bytes} in {took:.1}s → {dest}"));
                    }
                }
                if errors > 0 {
                    if ui.link(RichText::new(format!("{errors} error(s)")).color(RED)).clicked() {
                        self.show_errors = Some(job.clone());
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if job.finished() {
                        if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                            dismiss.push(n);
                        }
                    } else if ui.small_button("Cancel").clicked() {
                        job.cancel();
                    }
                });
            });
        }
        // Clean, finished jobs fade out on their own after a few seconds.
        for (n, job) in self.jobs.iter().enumerate() {
            let old = job.elapsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some_and(|e| job.started.elapsed() > e + Duration::from_secs(8));
            if job.phase() == Phase::Done && job.error_count() == 0 && old && !dismiss.contains(&n) {
                dismiss.push(n);
            }
        }
        dismiss.sort_unstable();
        for n in dismiss.into_iter().rev() {
            self.jobs.remove(n);
        }
    }

    fn table(&mut self, ui: &mut Ui) {
        let results = self.in_results();
        let folders = results || self.cwd == RECENT;
        let width = ui.available_width();
        let (date_w, size_w, type_w) = (140.0, 90.0, if folders { 0.0 } else { 120.0 });
        let flex = (width - date_w - size_w - type_w).max(120.0);
        let (name_w, folder_w) = if folders { (flex * 0.45, flex * 0.55) } else { (flex, 0.0) };

        // Column layout: (label, sort key, x, width, right-aligned)
        let mut cols: Vec<(&str, Option<SortKey>, f32, f32, bool)> = Vec::new();
        let mut x = 0.0;
        let mut add = |label, key, w, right| {
            cols.push((label, key, x, w, right));
            x += w;
        };
        add("Name", Some(SortKey::Name), name_w, false);
        if folders {
            add("Folder", None, folder_w, false);
        }
        add("Date modified", Some(SortKey::Modified), date_w, false);
        if !folders {
            add("Type", Some(SortKey::Type), type_w, false);
        }
        add("Size", Some(SortKey::Size), size_w, true);

        let font = FontId::proportional(13.5);
        let visuals = ui.visuals().clone();
        let mods = ui.input(|i| i.modifiers);
        let mut action = None;

        // Header
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
            if resp.clicked() {
                if let Some(k) = key {
                    action = Some(Action::Sort(k));
                }
            }
        }
        ui.separator();

        if self.view.is_empty() {
            let msg = if self.loading || self.searching || self.build.is_some() {
                "Working…"
            } else if self.error.is_some() && !results {
                "Folder could not be read"
            } else if self.cwd == RECENT {
                "No recently opened files"
            } else if self.matcher.is_some() {
                "No matches"
            } else {
                "This folder is empty"
            };
            let rest = ui.available_rect_before_wrap();
            let resp = ui.interact(rest, ui.scope_id().with("empty"), Sense::click());
            ui.painter().text(rest.center_top() + egui::vec2(0.0, 30.0), Align2::CENTER_CENTER, msg, font.clone(), visuals.weak_text_color());
            // A share that refused us usually just needs credentials.
            if let (true, false, Some(target)) = (self.error.is_some(), self.loading, net::auth_target(&self.cwd)) {
                let r = Rect::from_center_size(rest.center_top() + egui::vec2(0.0, 66.0), egui::vec2(230.0, 26.0));
                if ui.put(r, Button::new("Connect with credentials…")).clicked() {
                    self.connect_share(target, true, Some(self.cwd.clone()));
                }
            }
            self.background_menu(&resp, &mut action);
            self.run_action(action, ui);
            return;
        }

        let mut area = ScrollArea::vertical().auto_shrink(false);
        if let Some(i) = self.scroll_to.take() {
            let top = i as f32 * ROW_H;
            let mut y = self.scroll_y;
            // Before the first frame has measured the list, show the row at the top.
            if top < y || self.view_h <= 0.0 {
                y = top;
            } else if top + ROW_H > y + self.view_h {
                y = top + ROW_H - self.view_h;
            }
            area = area.vertical_scroll_offset(y);
        } else if let Some(y) = self.scroll_set.take() {
            area = area.vertical_scroll_offset(y.max(0.0));
        }

        let check_cut = !self.cut_marks.is_empty();
        let typeahead_hit = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET && !t.positions.is_empty());
        let mut renaming = self.renaming.take();
        let mut rename_done: Option<bool> = None; // Some(commit?)
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let mut hover_drop: Option<String> = None;
        let mut row_zones = Vec::new();

        ui.spacing_mut().item_spacing.y = 0.0;
        let out = area.show_rows(ui, ROW_H, self.view.len() + BLANK_ROWS, |ui, range| {
            for vi in range {
                if vi >= self.view.len() {
                    // Trailing blank row: behaves like the empty space below.
                    let (_, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click_and_drag());
                    if resp.clicked() {
                        action = Some(Action::ClearSelection);
                    } else if resp.drag_started_by(PointerButton::Primary) {
                        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(resp.rect.left_top());
                        action = Some(Action::StartBand { origin, additive: mods.ctrl });
                    }
                    self.background_menu(&resp, &mut action);
                    continue;
                }
                let Some(row) = self.row(vi) else { continue };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click_and_drag());
                let selected = row.sel;
                let full = row.full_path(&self.cwd);
                let is_drop = self.drag.is_some() && row.is_dir() && !selected && resp.contains_pointer() && !archive::is_location(&full);
                if row.is_dir() && accepts_files(&full) {
                    row_zones.push((rect.intersect(ui.clip_rect()), full.clone()));
                }
                if is_drop {
                    hover_drop = Some(full.clone());
                }
                let painter = ui.painter();
                if selected {
                    painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
                } else if is_drop || (self.drag.is_none() && self.band.is_none() && resp.hovered()) {
                    painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                }
                if is_drop {
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.5, visuals.selection.stroke.color), egui::StrokeKind::Inside);
                }
                if self.focus == Some(vi) && !selected {
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, visuals.weak_text_color()), egui::StrokeKind::Inside);
                }
                let dim = check_cut && self.cut_marks.contains(&full.to_lowercase());
                let mut color = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
                let mut weak = if selected { color } else { visuals.weak_text_color() };
                if dim || row.is_hidden() {
                    color = color.gamma_multiply(0.55);
                    weak = weak.gamma_multiply(0.55);
                }
                let icon = if row.attrs & win::ATTR_REPARSE != 0 {
                    "↗"
                } else if row.is_dir() {
                    "🗀"
                } else {
                    "🗋"
                };

                let editing = renaming.as_ref().is_some_and(|r| r.path == full);
                let found = typeahead_hit.filter(|t| t.path == full);
                for &(label, _, cx, w, right) in &cols {
                    let r = Rect::from_min_size(egui::pos2(rect.left() + cx, rect.top()), egui::vec2(w, ROW_H));
                    if editing && label == "Name" {
                        continue;
                    }
                    if let (Some(t), "Name") = (found, label) {
                        highlighted_cell(painter, r, &format!("{icon}  "), &row.name, &t.positions, &font, color, &visuals);
                        continue;
                    }
                    let (text, c) = match label {
                        "Name" => (format!("{icon}  {}", row.name), color),
                        "Folder" => (row.dir.as_deref().unwrap_or("").to_string(), weak),
                        "Date modified" => (win::format_time(row.mtime), weak),
                        "Type" => (row.type_name(), weak),
                        _ => (if row.is_dir() { String::new() } else { fsys::format_size(row.size) }, weak),
                    };
                    cell(painter, r, &text, &font, c, right);
                }
                if let (true, Some(rn)) = (editing, renaming.as_mut()) {
                    let r = Rect::from_min_size(egui::pos2(rect.left() + 24.0, rect.top() + 1.0), egui::vec2(name_w - 28.0, ROW_H - 2.0));
                    let id = egui::Id::unique("rename_box");
                    let edit = ui.put(r, TextEdit::singleline(&mut rn.text).id(id).font(font.clone()));
                    if !rn.focused {
                        edit.request_focus();
                        // Preselect the name without its extension, like Explorer.
                        let stem = match rn.text.rfind('.') {
                            Some(i) if i > 0 && !row.is_dir() => rn.text[..i].chars().count(),
                            _ => rn.text.chars().count(),
                        };
                        let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(stem));
                        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        state.cursor.set_char_range(Some(range));
                        state.store(ui.ctx(), id);
                        rn.focused = true;
                    } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                        rename_done = Some(false);
                    } else if edit.lost_focus() {
                        rename_done = Some(true);
                    }
                }

                // Interaction
                if resp.double_clicked() {
                    action = Some(Action::Open(vi));
                } else if resp.middle_clicked() && row.is_dir() {
                    action = Some(Action::OpenInNewTab(full.clone()));
                } else if resp.clicked() {
                    action = Some(Action::Click { vi, ctrl: mods.ctrl, shift: mods.shift });
                } else if resp.secondary_clicked() {
                    action = Some(Action::Secondary(vi));
                } else if resp.drag_started_by(PointerButton::Primary) {
                    let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(rect.left_top());
                    // Dragging a name moves items; dragging elsewhere draws a selection box.
                    action = Some(if origin.x < rect.left() + name_w {
                        Action::StartDrag(vi)
                    } else {
                        Action::StartBand { origin, additive: mods.ctrl }
                    });
                }
                let is_dir = row.is_dir();
                let archive_file = !is_dir && !crate::archive::is_location(&self.cwd) && crate::archive::is_archive_file(&full);
                let archive_location = crate::archive::is_location(&self.cwd);
                resp.context_menu(|ui| {
                    let mut item = |ui: &mut Ui, label: &str, shortcut: &str, a: Action| {
                        if ui.add(Button::new(label).shortcut_text(shortcut)).clicked() {
                            action = Some(a);
                            ui.close();
                        }
                    };
                    if archive_file {
                        item(ui, "Open archive", "Enter", Action::OpenArchive(vi));
                        item(ui, "Extract here", "", Action::ExtractArchive(vi));
                    } else {
                        item(ui, if is_dir { "Open folder" } else { "Open" }, "Enter", Action::Open(vi));
                    }
                    if is_dir {
                        item(ui, "Open in new tab", "Middle-click", Action::OpenInNewTab(full.clone()));
                        item(ui, "Open in new window", "", Action::OpenInNewWindow(full.clone()));
                    }
                    if folders {
                        item(ui, "Open file location", "", Action::OpenLocation(vi));
                    }
                    if archive_location {
                        item(ui, "Extract next to the archive", "", Action::ExtractSelected);
                        item(ui, "Copy archive path", "", Action::CopyPath(vi));
                    } else {
                        item(ui, "Create symbolic link", "", Action::CreateLink(vi));
                        if !is_dir {
                            item(ui, "Run as administrator", "Ctrl+S", Action::RunAs(vi));
                        }
                        ui.separator();
                        item(ui, "Cut", "Ctrl+X", Action::Clip { cut: true });
                        item(ui, "Copy", "Ctrl+C", Action::Clip { cut: false });
                        if is_dir && clip::has_files() {
                            item(ui, "Paste into folder", "", Action::Paste(Some(full.clone())));
                        }
                        item(ui, "Copy path", "", Action::CopyPath(vi));
                        ui.separator();
                        item(ui, "Rename", "F2", Action::Rename(vi));
                        item(ui, "Delete", "Del", Action::Delete { permanent: false });
                        ui.separator();
                        item(ui, "Properties", "Alt+Enter", Action::Properties);
                    }
                });
            }
            // Empty space below the last row: click clears, drag draws a selection box.
            let rest = ui.available_rect_before_wrap();
            if rest.height() > 0.0 {
                let resp = ui.allocate_rect(rest, Sense::click_and_drag());
                if resp.clicked() {
                    action = Some(Action::ClearSelection);
                } else if resp.drag_started_by(PointerButton::Primary) {
                    let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(rest.left_top());
                    action = Some(Action::StartBand { origin, additive: mods.ctrl });
                }
                self.background_menu(&resp, &mut action);
            }
        });
        self.scroll_y = out.state.clamped_offset().y;
        self.view_h = out.inner_rect.height();
        self.rows_rect = out.inner_rect;
        self.zones.extend(row_zones);
        if hover_drop.is_some() {
            self.drop_target = hover_drop;
        }

        match rename_done {
            Some(true) => {
                if let Some(r) = renaming.take() {
                    self.commit_rename(r);
                }
            }
            Some(false) => renaming = None,
            None => {}
        }
        self.renaming = renaming;

        self.update_band(ui, pointer);
        self.typeahead_overlay(ui);
        self.run_action(action, ui);
    }

    /// What's been typed, bottom-right of the list (red when nothing matches).
    fn typeahead_overlay(&self, ui: &Ui) {
        let Some(t) = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET) else { return };
        let v = ui.visuals();
        let color = if t.positions.is_empty() { RED } else { v.strong_text_color() };
        let galley = ui.painter().layout_no_wrap(format!("🔍 {}", t.text), FontId::proportional(14.0), color);
        let size = galley.size() + egui::vec2(16.0, 8.0);
        let rect = Rect::from_min_size(self.rows_rect.right_bottom() - size - egui::vec2(12.0, 10.0), size);
        ui.painter().rect(rect, 4.0, v.extreme_bg_color, Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Inside);
        ui.painter().galley(rect.min + egui::vec2(8.0, 4.0), galley, color);
    }

    /// The New menu: a folder, then the file types registered with ShellNew
    /// (loaded on first open).
    fn new_menu(&self, ui: &mut Ui, action: &mut Option<Action>) {
        if ui.add(Button::new("Folder").shortcut_text("Ctrl+Shift+N")).clicked() {
            *action = Some(Action::NewFolder);
            ui.close();
        }
        if ui.add(Button::new("Text file").shortcut_text("Ctrl+Alt+N")).clicked() {
            *action = Some(Action::NewFile);
            ui.close();
        }
        ui.separator();
        match self.shell_new_items.as_ref() {
            Some(items) if items.is_empty() => {
                ui.weak("No registered templates");
            }
            Some(items) => {
                for (index, item) in items.iter().enumerate() {
                    if ui.button(format!("{} ({})", item.label, item.extension)).clicked() {
                        *action = Some(Action::NewItem(index));
                        ui.close();
                    }
                }
            }
            None => {
                if !self.shell_new_loading {
                    *action = Some(Action::LoadShellNew);
                }
                ui.spinner();
            }
        }
    }

    fn background_menu(&self, resp: &egui::Response, action: &mut Option<Action>) {
        let writable = self.cwd != RECENT && !archive::is_location(&self.cwd);
        resp.context_menu(|ui| {
            let undo = self.history.next_undo().filter(|_| !self.history.busy);
            if ui.add_enabled(undo.is_some(), Button::new(undo.map_or("Undo".into(), |e| format!("Undo {}", e.describe()))).shortcut_text("Ctrl+Z")).clicked() {
                *action = Some(Action::Undo);
                ui.close();
            }
            if let Some(redo) = self.history.next_redo().filter(|_| !self.history.busy) {
                if ui.add(Button::new(format!("Redo {}", redo.describe())).shortcut_text("Ctrl+Y")).clicked() {
                    *action = Some(Action::Redo);
                    ui.close();
                }
            }
            ui.separator();
            if ui.add_enabled(clip::has_files() && writable, Button::new("Paste").shortcut_text("Ctrl+V")).clicked() {
                *action = Some(Action::Paste(None));
                ui.close();
            }
            ui.add_enabled_ui(writable, |ui| ui.menu_button("New", |ui| self.new_menu(ui, action)));
            ui.separator();
            if ui.add_enabled(writable, Button::new("Properties")).clicked() {
                *action = Some(Action::FolderProperties);
                ui.close();
            }
        });
    }

    /// Rubber-band selection: update the selected range and auto-scroll
    /// when the pointer leaves the list.
    fn update_band(&mut self, ui: &Ui, pointer: Option<Pos2>) {
        let Some(band) = &self.band else { return };
        let (Some(p), true) = (pointer, ui.input(|i| i.pointer.primary_down())) else {
            self.band = None;
            return;
        };
        let rr = self.rows_rect;
        let to_content = |y: f32| y - rr.top() + self.scroll_y;
        let (y0, y1) = {
            let cur = to_content(p.y.clamp(rr.top(), rr.bottom()));
            (band.origin_y.min(cur), band.origin_y.max(cur))
        };
        let first = (y0 / ROW_H).floor().max(0.0) as usize;
        let last = ((y1 / ROW_H).floor() as usize).min(self.view.len().saturating_sub(1));
        let base = band.base.clone();
        for vi in 0..self.view.len() {
            let inside = vi >= first && vi <= last && y1 > y0;
            self.set_sel(vi, base.get(vi).copied().unwrap_or(false) || inside);
        }
        let band = self.band.as_ref().unwrap();
        let screen = Rect::from_two_pos(
            egui::pos2(band.origin_x, band.origin_y - self.scroll_y + rr.top()),
            egui::pos2(p.x, p.y.clamp(rr.top(), rr.bottom())),
        )
        .intersect(rr);
        let sel = ui.visuals().selection.stroke.color;
        ui.painter().rect(screen, 0.0, sel.gamma_multiply(0.15), Stroke::new(1.0, sel), egui::StrokeKind::Inside);
        self.autoscroll(p);
    }

    fn autoscroll(&mut self, p: Pos2) {
        let rr = self.rows_rect;
        let dy = if p.y < rr.top() + 4.0 {
            -(rr.top() + 4.0 - p.y).min(60.0)
        } else if p.y > rr.bottom() - 4.0 {
            (p.y - rr.bottom() + 4.0).min(60.0)
        } else {
            0.0
        };
        if dy != 0.0 {
            self.scroll_set = Some(self.scroll_y + dy * 0.5);
            self.ctx.request_repaint();
        }
    }

    fn run_action(&mut self, action: Option<Action>, ui: &Ui) {
        let Some(action) = action else { return };
        match action {
            Action::Click { vi, ctrl, shift } => {
                if shift {
                    let anchor = self.anchor.unwrap_or(vi);
                    if !ctrl {
                        self.clear_sel();
                    }
                    self.select_range(anchor, vi);
                    self.anchor = Some(anchor);
                    self.focus = Some(vi);
                } else if ctrl {
                    let on = !self.row(vi).is_some_and(|r| r.sel);
                    self.set_sel(vi, on);
                    self.focus = Some(vi);
                    self.anchor = Some(vi);
                } else {
                    self.select_only(vi);
                }
            }
            Action::Secondary(vi) => {
                if !self.row(vi).is_some_and(|r| r.sel) {
                    self.select_only(vi);
                }
            }
            Action::Open(i) => self.open(i),
            Action::OpenInNewTab(path) => self.open_tab(path),
            Action::OpenInNewWindow(path) => {
                self.open_window(&path, None);
            }
            Action::OpenArchive(i) => self.open(i),
            Action::ExtractArchive(i) => {
                if let Some(row) = self.row(i) {
                    let path = row.full_path(&self.cwd);
                    self.notice = Some(format!("Extracting {}…", row.name));
                    self.spawn_io(move || Some(match archive::extract_archive(&path) {
                        Ok(destination) => Msg::Extracted { created: vec![destination.clone()], open: Some(destination), errors: Vec::new() },
                        Err(error) => Msg::Error(error),
                    }));
                }
            }
            Action::ExtractSelected => {
                let paths = self.selected_paths();
                let Some(dest) = archive::parse_location(&self.cwd).and_then(|(a, _)| fsys::parent(a)) else { return };
                if !paths.is_empty() {
                    self.extract_to(paths, dest);
                }
            }
            Action::CreateLink(i) => {
                if let Some(row) = self.row(i).cloned() {
                    let target = row.full_path(&self.cwd);
                    let name = row.name.to_string();
                    let dir = self.cwd.clone();
                    let is_dir = row.is_dir();
                    self.spawn_io(move || {
                        let link_name = if is_dir {
                            format!("{name} - Link")
                        } else {
                            match name.rfind('.').filter(|&index| index > 0) {
                                Some(index) => format!("{} - Link{}", &name[..index], &name[index..]),
                                None => format!("{name} - Link"),
                            }
                        };
                        let name = ops::unique_name(&dir, &link_name, false);
                        Some(match fileop::create_symbolic_link(&fsys::join(&dir, &name), &target, is_dir) {
                            Ok(()) => Msg::Created { dir, name },
                            Err(code) => Msg::Error(format!("Could not create link: {}", win::error_text(code))),
                        })
                    });
                }
            }
            Action::EjectDrive(path) => self.eject(path),
            Action::ChangeLetter { volume, name, current, why } => self.open_letter_dialog(volume, name, current, why),
            Action::RunAs(i) => {
                self.select_only(i);
                self.run_as(i);
            }
            Action::OpenLocation(i) => {
                if let Some(dir) = self.row(i).and_then(|r| r.dir.clone()) {
                    let name = self.row(i).map(|r| r.name.clone());
                    self.navigate(dir.to_string(), true);
                    self.pending_select = name;
                }
            }
            Action::CopyPath(i) => {
                let paths = self.selected_paths();
                let text = if paths.len() > 1 { paths.join("\r\n") } else { self.row(i).map(|r| r.full_path(&self.cwd)).unwrap_or_default() };
                ui.ctx().copy_text(text);
            }
            Action::Sort(k) => self.set_sort(k),
            Action::Clip { cut } => self.clip_selection(cut),
            Action::Paste(into) => self.paste(into),
            Action::Rename(vi) => {
                self.select_only(vi);
                self.begin_rename(vi);
            }
            Action::Delete { permanent } => {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    self.confirm_delete = Some((paths, permanent));
                }
            }
            Action::Properties => self.open_properties(),
            Action::FolderProperties => {
                self.clear_sel();
                self.open_properties();
            }
            Action::NewFolder => self.new_folder(),
            Action::NewFile => self.new_file(),
            Action::NewItem(index) => self.create_shell_new_item(index),
            Action::LoadShellNew => self.load_shell_new_items(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::ClearSelection => self.clear_sel(),
            Action::StartBand { origin, additive } => {
                let base = if additive {
                    (0..self.view.len()).map(|vi| self.row(vi).is_some_and(|r| r.sel)).collect()
                } else {
                    self.clear_sel();
                    Vec::new()
                };
                let origin_y = origin.y - self.rows_rect.top() + self.scroll_y;
                self.band = Some(Band { origin_x: origin.x, origin_y, base });
            }
            Action::StartDrag(vi) => {
                if !self.row(vi).is_some_and(|r| r.sel) {
                    self.select_only(vi);
                }
                self.drag = Some(ItemDrag { paths: self.selected_paths(), same_volume: None });
            }
        }
    }

    /// Floating label while dragging, and the drop itself on release.
    fn update_drag(&mut self, ctx: &egui::Context) {
        let Some(drag) = &mut self.drag else { return };
        let (pos, released, mods) = ctx.input(|i| (i.pointer.interact_pos(), i.pointer.primary_released(), i.modifiers));
        let target = self.drop_target.clone();
        let from_archive = drag.paths.first().is_some_and(|p| archive::is_location(p));
        let same = match (&target, &drag.same_volume) {
            _ if from_archive => false,
            (Some(t), Some((cached, same))) if cached == t => *same,
            (Some(t), _) => {
                let same = fileop::same_volume(&drag.paths[0], t);
                drag.same_volume = Some((t.clone(), same));
                same
            }
            _ => false,
        };
        let kind = Self::drop_kind(mods, same);
        if released {
            let paths = std::mem::take(&mut drag.paths);
            self.drag = None;
            match target {
                Some(t) if from_archive => self.extract_to(paths, t),
                Some(t) => {
                    self.start_job(kind, paths, t);
                }
                None => {}
            }
            return;
        }
        // Leaving the window hands the drag to Windows, so it can end in
        // Explorer, the desktop, another app or another FEx window.
        if let (Some(p), Some(hwnd), false) = (pos, self.hwnd, from_archive) {
            if !ctx.content_rect().contains(p) {
                let paths = std::mem::take(&mut drag.paths);
                self.drag = None;
                if let Err(e) = oledrag::drag_files(hwnd, &paths) {
                    self.error = Some(e);
                }
                self.release_pointer = true;
                self.ctx.request_repaint();
                return;
            }
        }
        if let Some(p) = pos {
            let n = drag.paths.len();
            let what = if n == 1 { drag.paths[0].rsplit('\\').next().unwrap_or("").to_string() } else { format!("{n} items") };
            let text = match &target {
                Some(t) => {
                    let folder = t.trim_end_matches('\\').rsplit('\\').next().unwrap_or(t);
                    let verb = if from_archive { "Extract" } else if kind == Kind::Move { "Move" } else { "Copy" };
                    format!("{verb} {what} to {folder}")
                }
                None => what,
            };
            egui::Area::new(egui::Id::unique("drag_label"))
                .order(egui::Order::Tooltip)
                .fixed_pos(p + egui::vec2(16.0, 12.0))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
                });
            self.autoscroll(p);
        }
    }

    /// Files dragged in from Explorer, Outlook, browsers and other apps
    /// (see oledrop.rs). They always land in the open folder.
    fn handle_os_drop(&mut self, ctx: &egui::Context) {
        let Some(inbox) = self.drops.clone() else { return };
        let target = accepts_files(&self.cwd).then_some(self.cwd.as_str());
        inbox.set_layout(target, std::mem::take(&mut self.zones), ctx.pixels_per_point());
        if let Some(hover) = inbox.hover() {
            let what = if hover.count == 1 { "1 item".to_string() } else { format!("{} items", hover.count) };
            let (text, outline) = match (&hover.payload, &hover.dest) {
                (oledrop::Payload::Tab, _) => ("Open as a tab in this window".to_string(), Some(ctx.content_rect())),
                (_, None) => ("Can't drop here: open a folder first".to_string(), None),
                (payload, Some(dest)) => {
                    let verb = if *payload == oledrop::Payload::Virtual { "Save" } else if hover.moving { "Move" } else { "Copy" };
                    // The folder under the pointer, or else the list of the open folder.
                    let zone = inbox.zone_at(hover.pos).unwrap_or(self.rows_rect);
                    (format!("{verb} {what} to {}", tab_label(dest)), Some(zone))
                }
            };
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
            let sel = ctx.global_style().visuals.selection.stroke.color;
            if let Some(r) = outline {
                painter.rect(r, 2.0, sel.gamma_multiply(0.12), Stroke::new(2.0, sel), egui::StrokeKind::Inside);
            }
            egui::Area::new(egui::Id::unique("os_drop_label"))
                .order(egui::Order::Tooltip)
                .fixed_pos(hover.pos + egui::vec2(16.0, 12.0))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
                });
        }
        for dropped in inbox.take() {
            match dropped {
                oledrop::Dropped::Files { paths, dest, ctrl, shift } if !paths.is_empty() => {
                    let same = fileop::same_volume(&paths[0], &dest);
                    let mods = Modifiers { ctrl, shift, ..Modifiers::NONE };
                    self.start_job(Self::drop_kind(mods, same), paths, dest);
                }
                oledrop::Dropped::Files { .. } => {}
                oledrop::Dropped::Virtual { files, dest } => self.save_virtual_drops(dest, files),
                oledrop::Dropped::Tab(location) => {
                    self.open_tab(location);
                    self.ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            }
        }
    }

    /// Move files that only existed inside the drag source from their staging
    /// files into place. A top-level name that is taken gets a numbered one;
    /// anything dragged inside a folder follows its folder. Every file is
    /// marked as coming from the Internet zone: its origin is unknown.
    fn save_virtual_drops(&self, directory: String, files: Vec<oledrop::VirtualFile>) {
        let failed_dir = directory.clone();
        self.spawn_io_or(
            move || {
                let mut errors = Vec::new();
                let mut tops: HashMap<String, String> = HashMap::new(); // lowercase dropped name → name used
                let mut created = Vec::new();
                for file in files {
                    // Checked before anything else: an unsafe name must never
                    // become a path, folder or not.
                    let staged = match file.data {
                        Ok(staged) => staged,
                        Err(e) => {
                            errors.push(format!("{}: {e}", file.name));
                            continue;
                        }
                    };
                    let (top, rest) = file.name.split_once('\\').map_or((file.name.as_str(), None), |(t, r)| (t, Some(r)));
                    let chosen = tops.entry(top.to_lowercase()).or_insert_with(|| {
                        let name = ops::unique_name(&directory, top, false);
                        created.push(fsys::join(&directory, &name));
                        name
                    });
                    let top_path = fsys::join(&directory, chosen);
                    let path = rest.map_or_else(|| top_path.clone(), |r| fsys::join(&top_path, r));
                    let Some(staged) = staged else {
                        if file.is_dir {
                            if let Err(e) = std::fs::create_dir_all(&path) {
                                errors.push(format!("{path}: {e}"));
                            }
                        }
                        continue;
                    };
                    if let Some(parent) = std::path::Path::new(&path).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Never replaces an existing file.
                    if let Err(e) = fileop::rename(&staged.to_string_lossy(), &path, false) {
                        let _ = std::fs::remove_file(&staged);
                        errors.push(format!("{path}: {}", win::error_text(e)));
                        continue;
                    }
                    let _ = fileop::set_attributes(&path, 0x80); // drop the staging file's Hidden
                    win::set_zone(std::path::Path::new(&path), win::INTERNET_ZONE);
                    if let Some(time) = win::system_time(file.mtime) {
                        let _ = std::fs::OpenOptions::new().write(true).open(&path).and_then(|f| f.set_modified(time));
                    }
                }
                created.retain(|p| fileop::stat(p).is_some());
                Some(Msg::VirtualDropDone { dir: directory, created, errors })
            },
            move |e| Msg::VirtualDropDone { dir: failed_dir, created: Vec::new(), errors: vec![e] },
        );
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.help_open {
            let mut open = true;
            egui::Window::new("Keyboard shortcuts")
                .id(egui::Id::unique("help_window"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new("shortcuts").num_columns(2).spacing([24.0, 4.0]).striped(false).show(ui, |ui| {
                        for (keys, what) in SHORTCUTS {
                            if keys.is_empty() {
                                ui.add_space(6.0);
                                ui.end_row();
                                continue;
                            }
                            ui.strong(*keys);
                            ui.label(*what);
                            ui.end_row();
                        }
                    });
                });
            self.help_open = open && !ctx.input(|i| i.key_pressed(Key::Escape));
        }

        // Conflict decision for the first job that needs one.
        if let Some(job) = self.jobs.iter().find(|j| j.phase() == Phase::AwaitingDecision).cloned() {
            egui::Modal::new(egui::Id::unique("conflict")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading("Files already exist");
                ui.label(format!("{} of the files being {} already exist in\n{}", job.conflicts.load(Relaxed), if job.kind == Kind::Move { "moved" } else { "copied" }, job.dest));
                ui.add_space(8.0);
                let choices = [
                    (Conflict::Replace, "Replace all", "Overwrite every existing file"),
                    (Conflict::Update, "Update", "Only transfer files whose size or date differ (rsync-style quick check)"),
                    (Conflict::Skip, "Skip all", "Leave existing files untouched"),
                    (Conflict::KeepBoth, "Keep both", "Add a number to the incoming file's name"),
                ];
                for (c, label, tip) in choices {
                    if ui.add_sized([200.0, 24.0], Button::new(label)).on_hover_text(tip).clicked() {
                        job.resolve(c);
                    }
                }
                ui.add_space(4.0);
                if ui.button("Cancel").clicked() {
                    job.cancel();
                }
            });
        }

        if let Some((paths, permanent)) = self.confirm_delete.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("confirm_delete")).show(ctx, |ui| {
                ui.set_width(380.0);
                let what = if paths.len() == 1 {
                    format!("'{}'", paths[0].rsplit('\\').next().unwrap_or(""))
                } else {
                    format!("these {} items", paths.len())
                };
                if permanent {
                    ui.heading("Delete permanently?");
                    ui.label(format!("Permanently delete {what}? This can't be undone."));
                } else {
                    ui.heading("Move to Recycle Bin?");
                    ui.label(format!("Move {what} to the Recycle Bin?"));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let go = ui.button(RichText::new("Delete").color(RED)).clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                    if go {
                        let p = paths.clone();
                        self.spawn_io(move || {
                            win::com_init();
                            match fileop::recycle(&p, permanent) {
                                // Network deletes skip the Recycle Bin, so they can't be undone.
                                Ok(()) if !permanent => Some(Msg::Recycled(p.into_iter().filter(|x| !fileop::is_network(x)).collect())),
                                Ok(()) => None,
                                Err(e) => Some(Msg::Error(e)),
                            }
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_delete = None;
            }
        }

        if let Some(job) = self.show_errors.clone() {
            let mut open = true;
            egui::Window::new("Transfer errors").open(&mut open).default_width(560.0).show(ctx, |ui| {
                let errors = job.errors.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only the visible lines are laid out, however many there are.
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
                ScrollArea::vertical().max_height(320.0).show_rows(ui, row_h, errors.len(), |ui, range| {
                    for e in &errors[range] {
                        ui.label(e);
                    }
                });
            });
            if !open {
                self.show_errors = None;
            }
        }

        if let Some(p) = &mut self.props {
            if !p.show(ctx) {
                self.props = None;
            } else if p.busy() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.descending = !self.descending;
        } else {
            self.sort = key;
            self.descending = key == SortKey::Modified || key == SortKey::Size;
        }
        self.resort();
    }
}

/// Use Windows' own UI font, with Segoe UI Symbol as glyph fallback for icons
/// and arrows. Fonts that are missing are skipped; egui's built-in fonts
/// remain the fallback. Segoe UI Emoji (13 MB) is added after startup.
fn install_system_fonts(ctx: &egui::Context) {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = Vec::new();
    for file in ["segoeui.ttf", "seguisym.ttf"] {
        if let Ok(bytes) = std::fs::read(format!(r"{dir}\Fonts\{file}")) {
            fonts.font_data.insert(file.into(), Arc::new(egui::FontData::from_owned(bytes)));
            loaded.push(file.to_string());
        }
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family.clone()).or_default();
        if family == egui::FontFamily::Proportional {
            list.splice(0..0, loaded.iter().cloned());
        } else {
            list.extend(loaded.iter().filter(|f| *f != "segoeui.ttf").cloned());
        }
    }
    ctx.set_fonts(fonts);
}

/// A plain, still look. egui redraws only on input, but every animated frame
/// is another full redraw, so: no animations, no shadows (extra geometry on
/// every popup), and solid scroll bars that don't fade in and out on hover.
fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.animation_time = 0.0;
        s.visuals.window_shadow = egui::Shadow::NONE;
        s.visuals.popup_shadow = egui::Shadow::NONE;
        s.spacing.scroll = egui::style::ScrollStyle::solid();
    });
}

/// Name cell with type-to-find matches emphasized.
#[allow(clippy::too_many_arguments)]
fn highlighted_cell(painter: &egui::Painter, rect: Rect, prefix: &str, name: &str, positions: &[usize], font: &FontId, color: Color32, v: &egui::Visuals) {
    use egui::text::{LayoutJob, TextFormat};
    let plain = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hit = TextFormat {
        font_id: font.clone(),
        color: v.strong_text_color(),
        background: v.warn_fg_color.gamma_multiply(0.35),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.append(prefix, 0.0, plain.clone());
    let mut run = String::new();
    let mut run_hit = false;
    for (i, c) in name.chars().enumerate() {
        let h = positions.contains(&i);
        if h != run_hit && !run.is_empty() {
            job.append(&run, 0.0, if run_hit { hit.clone() } else { plain.clone() });
            run.clear();
        }
        run_hit = h;
        run.push(c);
    }
    job.append(&run, 0.0, if run_hit { hit } else { plain });
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    let galley = painter.layout_job(job);
    let pos = egui::pos2(clip.left(), rect.center().y - galley.size().y / 2.0);
    painter.with_clip_rect(clip).galley(pos, galley, color);
}

/// Case-insensitive name equality, without allocating for ASCII names.
fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || (!(a.is_ascii() && b.is_ascii()) && a.to_lowercase() == b.to_lowercase())
}

/// Whether files can be dropped into `location` (not a pseudo-location,
/// archive or server root).
fn accepts_files(location: &str) -> bool {
    location != RECENT && location != NETWORK && !archive::is_location(location) && !fsys::is_server_root(location)
}

/// Path-bar segments for a location: (label, location to open).
fn breadcrumbs(location: &str) -> Vec<(String, String)> {
    let single = || vec![(location.to_string(), location.to_string())];
    if location == RECENT || location == NETWORK {
        return single();
    }
    if let Some((archive_path, inner)) = archive::parse_location(location) {
        let mut out = breadcrumbs(archive_path);
        if let Some(last) = out.last_mut() {
            last.1 = archive::root_location(archive_path);
        }
        let mut acc = String::new();
        for part in inner.split(['/', '\\']).filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            out.push((part.to_string(), archive::location(archive_path, &acc)));
        }
        return out;
    }
    let (mut acc, rest) = match location.strip_prefix(r"\\") {
        Some(rest) => {
            let (server, rest) = rest.split_once('\\').unwrap_or((rest, ""));
            (format!(r"\\{server}"), rest)
        }
        None => {
            let (drive, rest) = location.split_once('\\').unwrap_or((location, ""));
            (format!("{drive}\\"), rest)
        }
    };
    let mut out = vec![(acc.trim_end_matches('\\').to_string(), acc.clone())];
    for part in rest.split('\\').filter(|p| !p.is_empty()) {
        acc = fsys::join(&acc, part);
        out.push((part.to_string(), acc.clone()));
    }
    if out[0].0.is_empty() {
        return single();
    }
    out
}

/// Short tab title: the folder's own name, the drive, or the archive member.
fn tab_label(location: &str) -> String {
    if let Some((archive, inner)) = archive::parse_location(location) {
        let name = if inner.is_empty() { archive } else { inner };
        return name.rsplit(['\\', '/']).next().unwrap_or(name).to_string();
    }
    match location.trim_end_matches('\\').rsplit('\\').next() {
        Some(name) if !name.is_empty() && !name.ends_with(':') => name.to_string(),
        _ => location.to_string(),
    }
}

/// Paint one table cell, clipped to its column.
fn cell(painter: &egui::Painter, rect: Rect, text: &str, font: &FontId, color: Color32, right: bool) {
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    let p = painter.with_clip_rect(clip);
    if right {
        p.text(egui::pos2(clip.right(), rect.center().y), Align2::RIGHT_CENTER, text, font.clone(), color);
    } else {
        p.text(egui::pos2(clip.left(), rect.center().y), Align2::LEFT_CENTER, text, font.clone(), color);
    }
}

impl eframe::App for FexApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.session().save();
        for live in &self.indexes {
            let idx = index::read(&live.index);
            if idx.dirty && !idx.loaded {
                let _ = idx.save();
            }
        }
    }

    /// Clipboard shortcuts are delivered as plain key events (see main.rs) so
    /// Ctrl+C/X/V can act on files. While a text field has focus, turn them
    /// back into the text clipboard events egui's TextEdit expects.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // An OLE drag ran its own message loop and ate the button release.
        if std::mem::take(&mut self.release_pointer) {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            raw.events.push(egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
            raw.events.push(egui::Event::PointerGone);
        }
        if !ctx.egui_wants_keyboard_input() {
            return;
        }
        for ev in raw.events.iter_mut() {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                if modifiers.command_only() {
                    match key {
                        Key::C => *ev = egui::Event::Copy,
                        Key::X => *ev = egui::Event::Cut,
                        Key::V => {
                            if let Some(text) = clip::get_text() {
                                *ev = egui::Event::Paste(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.drain_messages();
        if let Some(t) = self.query_dirty {
            let waited = t.elapsed();
            if waited >= QUERY_DEBOUNCE {
                self.apply_query();
            } else {
                ui.ctx().request_repaint_after(QUERY_DEBOUNCE - waited);
            }
        }
        // Cut marks disappear once something else is put on the clipboard.
        if !self.cut_marks.is_empty() && clip::sequence() != self.cut_seq {
            self.cut_marks.clear();
        }
        if let Some((job, seq)) = &self.cut_job {
            if job.finished() {
                if job.phase() == Phase::Done && job.error_count() == 0 {
                    clip::clear_if_unchanged(*seq);
                }
                self.cut_marks.clear();
                self.cut_job = None;
            }
        }

        let ctx = ui.ctx().clone();
        if self.renaming.is_none() {
            self.handle_keys(&ctx);
        }
        self.drop_target = None;
        self.zones.clear();
        // Remember the normal window placement for next time.
        let (outer, inner, maximized, minimized) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized.unwrap_or(false), v.minimized.unwrap_or(false))
        });
        if !minimized {
            self.maximized = maximized;
            if let (false, Some(outer), Some(inner)) = (maximized, outer, inner) {
                self.placement = Some(([outer.min.x, outer.min.y], [inner.width(), inner.height()]));
            }
        }

        self.finish_history_jobs();
        if self.requery_at.is_some_and(|t| Instant::now() >= t) {
            self.requery_at = None;
            if let (true, Some(m)) = (self.in_results(), self.matcher.clone()) {
                if let Some(live) = self.current_index() {
                    self.run_query(live, m, self.generation.load(Relaxed));
                }
            }
        } else if let Some(t) = self.requery_at {
            ctx.request_repaint_after(t.saturating_duration_since(Instant::now()));
        }
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.last_save = Instant::now();
            let dirty: Vec<_> = self.indexes.iter().filter(|l| index::try_read(&l.index).is_none_or(|i| i.dirty)).cloned().collect();
            for live in &dirty {
                self.save_index(live);
            }
        }
        if self.elevated {
            egui::Panel::top("elevated").show(ui, |ui| {
                ui.colored_label(
                    RED,
                    "⚠ FEx is running as administrator: nothing you do here will ask for UAC approval, and \
                     Windows blocks dragging files in from Explorer and other normal programs. \
                     Unless you need that, close it and start FEx normally.",
                );
            });
        }
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.tabs.len() > 1 {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_strip(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if !self.jobs.is_empty() {
            egui::Panel::bottom("jobs").show(ui, |ui| {
                ui.add_space(2.0);
                self.jobs_panel(ui);
                ui.add_space(2.0);
            });
        }
        egui::Panel::left("sidebar").default_size(170.0).resizable(true).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.cwd == NETWORK {
                self.network_page(ui);
            } else {
                self.table(ui);
            }
        });

        self.update_drag(&ctx);
        self.update_tab_drag(&ctx);
        self.handle_os_drop(&ctx);
        self.dialogs(&ctx);
        self.map_dialog(&ctx);
        self.letter_dialog(&ctx);

        if self.build.is_some() || self.jobs.iter().any(|j| !j.finished()) {
            // Progress counters live in atomics; tick the UI.
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if !self.jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1)); // auto-dismiss timer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("fex-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("folder")).unwrap();
        for f in ["a.txt", "b.txt", "video.mp4"] {
            fs::write(dir.join(f), f).unwrap();
        }
        dir.to_string_lossy().into_owned()
    }

    /// Drain messages until `done` holds (or fail after 5 s).
    fn pump(app: &mut FexApp, done: impl Fn(&FexApp) -> bool) {
        for _ in 0..500 {
            app.drain_messages();
            if done(app) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn focused_name(app: &FexApp) -> Option<String> {
        app.focus.and_then(|i| app.row(i)).map(|r| r.name.to_string())
    }

    /// Regression: moving the focused (last) file out of the folder made
    /// rebuild_view index the shrunk listing through the stale view.
    #[test]
    fn focused_file_moved_away() {
        let dir = scratch("moved");
        let mut app = FexApp::with_context(egui::Context::default(), Some(dir.clone()));
        pump(&mut app, |a| !a.loading);
        assert_eq!(app.view.len(), 4);
        let vi = app.find_name("video.mp4").unwrap();
        app.select_only(vi);

        fs::rename(format!(r"{dir}\video.mp4"), format!(r"{dir}\folder\video.mp4")).unwrap();
        pump(&mut app, |a| a.find_name("video.mp4").is_none());
        assert_eq!(app.view.len(), 3);
        assert_eq!(focused_name(&app), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_to_find() {
        let dir = std::env::temp_dir().join(format!("fex-app-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in ["Annual report.pdf", "Q3 report.pdf", "dad.txt", "data.csv", "notes.txt"] {
            fs::write(dir.join(f), "x").unwrap();
        }
        let mut app = FexApp::with_context(egui::Context::default(), Some(dir.to_string_lossy().into_owned()));
        pump(&mut app, |a| !a.loading);

        app.type_to_find("rep".into()); // word start; earlier in the name wins
        assert_eq!(focused_name(&app).as_deref(), Some("Q3 report.pdf"));
        app.type_to_find("anrep".into()); // fuzzy
        assert_eq!(focused_name(&app).as_deref(), Some("Annual report.pdf"));
        app.type_to_find("d".into());
        assert_eq!(focused_name(&app).as_deref(), Some("dad.txt"));
        app.type_to_find("dd".into()); // no "dd…" name: cycle like Explorer
        assert_eq!(focused_name(&app).as_deref(), Some("data.csv"));
        app.type_to_find("ddd".into());
        assert_eq!(focused_name(&app).as_deref(), Some("dad.txt"));
        app.type_to_find("dat".into()); // after Backspace-style edits
        assert_eq!(focused_name(&app).as_deref(), Some("data.csv"));
        assert_eq!(app.selected().len(), 1);
        app.type_to_find("zzz".into());
        assert!(app.typeahead.as_ref().unwrap().positions.is_empty()); // shown in red, selection kept
        assert_eq!(focused_name(&app).as_deref(), Some("data.csv"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn breadcrumbs_cover_every_kind_of_location() {
        let crumbs = |l: &str| breadcrumbs(l).into_iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>();
        assert_eq!(crumbs(r"C:\"), [r"C:=C:\"]);
        assert_eq!(crumbs(r"C:\Users\me"), [r"C:=C:\", r"Users=C:\Users", r"me=C:\Users\me"]);
        assert_eq!(crumbs(r"\\srv\share\x"), [r"\\srv=\\srv", r"share=\\srv\share", r"x=\\srv\share\x"]);
        assert_eq!(
            crumbs(r"zip:D:\a.zip|f/g"),
            [r"D:=D:\", r"a.zip=zip:D:\a.zip|", r"f=zip:D:\a.zip|f", r"g=zip:D:\a.zip|f/g"]
        );
        assert_eq!(crumbs(RECENT), ["Recent=Recent"]);
    }

    #[test]
    fn moving_tabs_keeps_the_active_one() {
        let dir = scratch("tabs");
        let mut app = FexApp::with_context(egui::Context::default(), Some(dir.clone()));
        app.open_tab(format!(r"{dir}\folder"));
        app.open_tab(dir.clone());
        app.switch_tab(1); // tabs: dir, dir\folder (active), dir
        app.move_tab(1, 2);
        assert_eq!(app.active_tab, 2);
        assert_eq!(app.cwd, format!(r"{dir}\folder"));
        app.move_tab(0, 2); // the active tab shifts left
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.tab_location(app.active_tab), format!(r"{dir}\folder"));
        assert_eq!(app.session().tabs.len(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn focus_follows_its_file_when_rows_shift() {
        let dir = scratch("shift");
        let mut app = FexApp::with_context(egui::Context::default(), Some(dir.clone()));
        pump(&mut app, |a| !a.loading);
        let vi = app.find_name("b.txt").unwrap();
        app.select_only(vi);

        fs::remove_file(format!(r"{dir}\a.txt")).unwrap(); // shifts b.txt up one row
        pump(&mut app, |a| a.find_name("a.txt").is_none());
        assert_eq!(focused_name(&app).as_deref(), Some("b.txt"));
        assert_eq!(app.selected().len(), 1);

        // Refresh after the folder shrank, focused on the last row.
        let vi = app.find_name("video.mp4").unwrap();
        app.select_only(vi);
        fs::remove_file(format!(r"{dir}\video.mp4")).unwrap();
        app.watcher = None; // force the refresh path rather than the watcher path
        app.refresh();
        pump(&mut app, |a| !a.loading);
        assert_eq!(app.view.len(), 2);
        assert_eq!(focused_name(&app), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
