//! FEx user interface on Linux (X11), feature for feature the Windows UI
//! (`src/app.rs`) on the Linux engine. Same principles: all filesystem work
//! happens on background threads; the UI thread only drains a message
//! channel and paints the visible rows.
//!
//! Panels and dialogs are drawn in `app_ui.rs`, the network page in
//! `netview.rs`.

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, Modifiers, PointerButton, Pos2, Rect};

use super::appearance::Appearance;
use super::fsys::{self, Row, SortKey};
use super::history::{Entry, History, Step};
use super::index::{Build, Index, Matcher};
use super::ops::{self, Job, Kind, Phase};
use super::props::{self, Props};
use super::session::Session;
use super::sys;
use super::watch::{self, Change, Watcher};
use super::{archive, clip, fileop, net, proto, recent, templates, trash, xdnd};
use crate::typeahead;

#[path = "app_ui.rs"]
mod ui;
#[path = "netview.rs"]
mod netview;

/// Search results materialized for display; the total count is still exact.
const MAX_RESULTS: usize = 250_000;
/// Rows in the first chunk of a daemon reply: more than a tall window shows.
const SCREENFUL: u32 = 200;
const QUERY_DEBOUNCE: Duration = Duration::from_millis(30);
const MAX_INDEXES: usize = 4;
const TYPEAHEAD_RESET: Duration = Duration::from_millis(1200);
/// Hovering a dragged item on a tab this long switches to it.
const TAB_SPRING: Duration = Duration::from_millis(600);
const ROW_H: f32 = 22.0;
/// Blank rows kept after the last item, so a full folder still has empty
/// space to right-click (New, Paste) or start a selection box from.
const BLANK_ROWS: usize = 3;
const RED: egui::Color32 = egui::Color32::from_rgb(230, 80, 80);
/// How often volumes are re-listed even without a mount change (GVFS
/// shares come and go without one), and free space is re-read.
const VOLUME_POLL: Duration = Duration::from_secs(5);
const SPACE_REFRESH: Duration = Duration::from_secs(60);
/// Pseudo-locations (real paths always start with `/`).
const RECENT: &[u8] = b"Recent";
const NETWORK: &[u8] = b"Network";
/// The F1 sheet; an empty row separates groups.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Open"),
    ("Ctrl+S", "Run as root (programs)"),
    ("Alt+← / Alt+→", "Back / forward"),
    ("Alt+↑, Backspace", "Up"),
    ("Ctrl+L", "Type a path"),
    ("Ctrl+F, F3", "Search"),
    ("F5", "Refresh, rebuild the search index"),
    ("", ""),
    ("Ctrl+C / Ctrl+X / Ctrl+V", "Copy / cut / paste"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
    ("F2", "Rename"),
    ("Del, Shift+Del", "Move to trash, delete permanently"),
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

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A location FEx can list and change: not Recent, Network or an archive.
fn is_real(loc: &[u8]) -> bool {
    loc.starts_with(b"/") && !archive::is_location(loc)
}

/// Whether files can be dropped into `location`.
fn accepts_files(loc: &[u8]) -> bool {
    is_real(loc)
}

fn parent_of(loc: &[u8]) -> Option<Vec<u8>> {
    if archive::is_location(loc) {
        archive::parent_location(loc)
    } else if is_real(loc) {
        fsys::parent(loc)
    } else {
        None
    }
}

/// A volume in the sidebar.
#[derive(Clone, PartialEq, Eq)]
struct Volume {
    label: String,
    point: Vec<u8>,
    source: Vec<u8>,
    tip: String,
    /// Served over the network: queried only off the UI thread, and its
    /// free space only when asked for (a dead server can stall a query).
    network: bool,
    /// A GVFS share (connected through the desktop or the Network page).
    gvfs: bool,
    /// Can be safely removed (USB stick, card, optical, external disk).
    ejectable: bool,
}

/// Local mounts and network shares, as the sidebar shows them. GVFS asks
/// its daemon and ejectability reads sysfs, so this runs on the volume
/// watcher's thread.
fn list_volumes(with_gvfs: bool) -> Vec<Volume> {
    let mut out: Vec<Volume> = sys::volumes().into_iter().map(|m| {
        let ejectable = with_gvfs && fileop::ejectable(&m);
        let mut v = volume_of_mount(m);
        v.ejectable = ejectable;
        v
    }).collect();
    if with_gvfs {
        out.extend(sys::gvfs_mounts().into_iter().map(|(label, point)| Volume {
            label: format!("🖧 {label}"),
            tip: lossy(&point),
            point,
            source: Vec::new(),
            network: true,
            gvfs: true,
            ejectable: false,
        }));
    }
    out
}

/// A mount as the sidebar shows it: local volumes by folder name and type,
/// network shares by folder name and server.
fn volume_of_mount(m: sys::Mount) -> Volume {
    let network = sys::is_network_fs(&m.fstype);
    let base = if m.point == b"/" { "File system".to_string() } else { lossy(fsys::base_name(&m.point)) };
    let source = lossy(&m.source);
    let (label, tip) = if network {
        let server = sys::server_of(&m.source).map(lossy);
        let label = match server {
            Some(server) => format!("🖧 {base}  ({server})"),
            None => format!("🖧 {base}  ({})", m.fstype),
        };
        (label, format!("{source} on {} ({})", lossy(&m.point), m.fstype))
    } else {
        (format!("🖴 {base}  ({})", m.fstype), format!("{} · {source}", lossy(&m.point)))
    };
    Volume { label, point: m.point, source: m.source, tip, network, gvfs: false, ejectable: false }
}

/// Free space of one volume, and when it was read.
struct Space {
    capacity: Option<(u64, u64)>,
    checked: Instant,
}

enum Msg {
    /// A folder's listing, and the watcher started just before it was read.
    Listing { generation: u64, result: Result<Vec<Row>, String>, took: Duration, watcher: Option<Watcher> },
    /// Files read from the clipboard, to paste into `dest`.
    Paste { files: Vec<Vec<u8>>, cut: bool, dest: Vec<u8>, seq: u32 },
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
    /// The daemon couldn't answer this query. `definitive`: it said it can't
    /// (not covered, still building, refused), so search in-process; else it
    /// didn't answer twice running, so report it rather than start a crawl.
    DaemonFailed { generation: u64, definitive: bool },
    /// The sidebar's volumes changed (a mount came or went).
    Volumes(Vec<Volume>),
    /// Free and total bytes of a volume (`None`: couldn't be read).
    Space { point: Vec<u8>, capacity: Option<(u64, u64)> },
    DriveEjected(String),
    Templates(Vec<templates::Item>),
    RecentChanged,
    Navigate(Vec<u8>),
    Created { dir: Vec<u8>, name: Vec<u8> },
    Renamed { old_path: Vec<u8>, new_name: Vec<u8> },
    /// Items moved to the trash (undoable), and what couldn't be.
    Trashed { done: Vec<Vec<u8>>, errors: Vec<String> },
    /// Archive contents written to disk; optionally open a folder afterwards.
    Extracted { created: Vec<Vec<u8>>, open: Option<Vec<u8>>, errors: Vec<String> },
    /// An undo (or redo) step that didn't need a transfer job finished.
    HistoryDone { entry: Entry, undo: bool, result: Result<(), String> },
    Error(String),
    Shares { server: String, result: Result<Vec<String>, String> },
    /// A connect or disconnect finished.
    NetDone(netview::NetResult),
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
/// sizes and dates in batches. Ends when the search is replaced.
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
        batch.sort_unstable_by(|a, b| a.dir.as_deref().cmp(&b.dir.as_deref()));
        fsys::fill_meta(&mut batch);
        let filled = batch.into_iter().map(|r| (r.id, r.size, r.mtime)).collect();
        if tx.send(Msg::Meta { generation, filled }).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// Keys typed into the file list to jump to a name.
struct TypeAhead {
    text: String,
    last: Instant,
    /// Highlighted characters of the matched row.
    path: Vec<u8>,
    positions: Vec<usize>,
}

enum Action {
    Click { vi: usize, ctrl: bool, shift: bool },
    Secondary(usize),
    Open(usize),
    OpenInNewTab(Vec<u8>),
    OpenInNewWindow(Vec<u8>),
    ExtractArchive(usize),
    /// Extract the selected archive members next to the archive.
    ExtractSelected,
    CreateLink(usize),
    EjectDrive(Vec<u8>),
    RunAs(usize),
    OpenLocation(usize),
    CopyPath(usize),
    Sort(SortKey),
    Clip { cut: bool },
    Paste(Option<Vec<u8>>),
    Rename(usize),
    Delete { permanent: bool },
    Properties,
    /// Properties of the open folder itself.
    FolderProperties,
    NewFolder,
    /// An empty text file (Ctrl+Alt+N).
    NewFile,
    /// Create a file from the template at this index.
    NewItem(usize),
    LoadTemplates,
    Undo,
    Redo,
    ClearSelection,
    Refresh,
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
    paths: Vec<Vec<u8>>,
    same_volume: Option<(Vec<u8>, bool)>,
}

struct Rename {
    path: Vec<u8>,
    original: String,
    text: String,
    focused: bool,
}

#[derive(Clone)]
struct Tab {
    location: Vec<u8>,
    back: Vec<Vec<u8>>,
    forward: Vec<Vec<u8>>,
}

/// What a running transfer job means for the undo history when it finishes.
enum Pending {
    Record,
    Undo(Entry),
    Redo,
}

pub struct App {
    ctx: egui::Context,
    /// Signed updates from GitHub releases (checked at start, then daily).
    updater: crate::update::Updater,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    cwd: Vec<u8>,
    path_text: String,
    /// The path bar shows the editable text instead of breadcrumbs.
    path_editing: bool,
    back: Vec<Vec<u8>>,
    forward: Vec<Vec<u8>>,
    tabs: Vec<Tab>,
    active_tab: usize,

    rows: Vec<Row>,
    /// Indices into the shown source (listing or results) that pass the
    /// hidden-file filter (and the filter query), in display order.
    view: Vec<usize>,
    loading: bool,
    error: Option<String>,
    /// Status-bar message that isn't an error ("You can now remove ...").
    notice: Option<String>,
    listed_in: Duration,
    listing_gen: u64,
    watcher: Option<Watcher>,
    /// Changes that arrived while the listing was still loading.
    early_changes: Vec<Change>,
    sort: SortKey,
    descending: bool,
    show_hidden: bool,

    query: String,
    regex: bool,
    /// Search below the folder (indexed); else filter the open folder.
    recursive: bool,
    query_dirty: Option<Instant>,
    query_gen: Arc<AtomicU64>,
    matcher: Option<Arc<Matcher>>,
    results: Option<Results>,
    search_error: Option<String>,
    searching: bool,
    indexes: Vec<Arc<Index>>,
    build: Option<Arc<Build>>,
    /// The system index daemon, when one is running.
    daemon: Option<proto::Status>,
    /// It failed a query; use the in-process index until its next status.
    daemon_failed: bool,
    /// It didn't answer a query at all: don't ask it again before this,
    /// whatever its status says (each try would cost the user a timeout).
    daemon_retry_at: Option<Instant>,

    focus: Option<usize>,
    anchor: Option<usize>,
    /// Full path to select when it appears in the listing.
    pending_select: Option<Vec<u8>>,
    pending_rename: bool,
    scroll_to: Option<usize>,
    scroll_set: Option<f32>,
    scroll_y: f32,
    view_h: f32,
    rows_rect: Rect,
    typeahead: Option<TypeAhead>,

    band: Option<Band>,
    drag: Option<ItemDrag>,
    drop_target: Option<Vec<u8>>,
    /// Folders under parts of the window this frame, for drops from other apps.
    zones: Vec<(Rect, Vec<u8>)>,
    renaming: Option<Rename>,
    confirm_delete: Option<(Vec<Vec<u8>>, bool)>,
    props: Option<Props>,

    jobs: Vec<Arc<Job>>,
    show_errors: Option<Arc<Job>>,
    /// Paths currently cut to the clipboard (drawn dimmed).
    cut_marks: HashSet<Vec<u8>>,
    cut_seq: u32,
    /// Cut-paste job whose completion should clear the clipboard.
    cut_job: Option<(Arc<Job>, u32)>,
    history: History,
    history_jobs: Vec<(Arc<Job>, Pending)>,

    tab_drag: Option<usize>,
    /// Where each tab was drawn last frame.
    tab_rects: Vec<Rect>,
    /// A dragged item has rested on this tab since then; it opens after a moment.
    tab_spring: Option<(usize, Instant)>,
    /// Normal (not maximized) window placement in points: outer top-left, inner size.
    placement: Option<([f32; 2], [f32; 2])>,
    maximized: bool,

    places: Vec<(String, Vec<u8>)>,
    volumes: Vec<Volume>,
    space: HashMap<Vec<u8>, Space>,
    /// Volumes with a free-space query in flight (one that hangs on a dead
    /// server stays here, so it is never asked again in parallel).
    space_pending: HashSet<Vec<u8>>,
    /// Show free space of network volumes too (off by default, as on Windows).
    report_network_space: bool,
    help_open: bool,
    templates: Option<Vec<templates::Item>>,
    templates_loading: bool,
    /// Remembered servers (`smb://name`), persisted in ~/.config/fex/servers.txt.
    servers: Vec<String>,
    net: netview::NetState,
    /// FEx runs as root: nothing it does will ask for a password.
    elevated: bool,
    /// The window title last sent, so it is only set when it changes.
    title: String,
    font: Option<String>,
    /// When recent frames panicked (see `recover`).
    panics: Vec<Instant>,
    /// Theme, font, size and which parts of the window are shown.
    look: Appearance,
    /// The Appearance window is open; `look_font` is its font field.
    look_open: bool,
    look_font: String,
    /// The window was created see-through (frosted glass at start).
    transparent: bool,
    /// Blur behind the window as last requested from the compositor.
    blur: Option<bool>,
}

impl App {
    /// `restore`: reopen the session's tabs (no location was given).
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<String>, session: &Session, restore: bool, look: Appearance) -> App {
        look.install(&cc.egui_ctx);
        let (tx, rx) = channel();
        let home = sys::home();
        let tabs = if restore { session.tabs.clone() } else { Vec::new() };
        let cwd = match start {
            Some(s) if s == "Recent" || s == "Network" || archive::is_location(s.as_bytes()) => s.into_bytes(),
            Some(s) => fsys::resolve_input(&s, &std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or(home.clone())),
            None => tabs.get(session.active).cloned().unwrap_or_else(|| home.clone()),
        };
        let updater = {
            let ctx = cc.egui_ctx.clone();
            crate::update::Updater::start(move || ctx.request_repaint())
        };
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            updater,
            tx,
            rx,
            cwd: Vec::new(),
            path_text: String::new(),
            path_editing: false,
            back: Vec::new(),
            forward: Vec::new(),
            tabs: vec![Tab { location: cwd.clone(), back: Vec::new(), forward: Vec::new() }],
            active_tab: 0,
            rows: Vec::new(),
            view: Vec::new(),
            loading: false,
            error: None,
            notice: None,
            listed_in: Duration::ZERO,
            listing_gen: 0,
            watcher: None,
            early_changes: Vec::new(),
            sort: SortKey::Name,
            descending: false,
            show_hidden: false,
            query: String::new(),
            regex: false,
            recursive: true,
            query_dirty: None,
            query_gen: Arc::new(AtomicU64::new(0)),
            matcher: None,
            results: None,
            search_error: None,
            searching: false,
            indexes: Vec::new(),
            build: None,
            daemon: None,
            daemon_failed: false,
            daemon_retry_at: None,
            focus: None,
            anchor: None,
            pending_select: None,
            pending_rename: false,
            scroll_to: None,
            scroll_set: None,
            scroll_y: 0.0,
            view_h: 0.0,
            rows_rect: Rect::NOTHING,
            typeahead: None,
            band: None,
            drag: None,
            drop_target: None,
            zones: Vec::new(),
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
            tab_drag: None,
            tab_rects: Vec::new(),
            tab_spring: None,
            placement: None,
            maximized: false,
            places: sys::quick_access(),
            // Mounts now (reading mountinfo is instant); GVFS shares and
            // ejectability arrive from the volume watcher.
            volumes: list_volumes(false),
            space: HashMap::new(),
            space_pending: HashSet::new(),
            report_network_space: session.network_space,
            help_open: false,
            templates: None,
            templates_loading: false,
            servers: net::load_servers(),
            net: netview::NetState::default(),
            elevated: sys::uid() == 0,
            title: String::new(),
            font: None,
            panics: Vec::new(),
            look_font: look.font.clone(),
            transparent: look.wants_transparency(),
            look,
            look_open: false,
            blur: None,
        };
        app.show_hidden = session.show_hidden;
        app.recursive = session.recursive;
        app.regex = session.regex;
        app.sort = session.sort;
        app.descending = session.descending;
        if tabs.len() > 1 {
            app.active_tab = session.active.min(tabs.len() - 1);
            app.tabs = tabs.into_iter().map(|location| Tab { location, back: Vec::new(), forward: Vec::new() }).collect();
            app.tabs[app.active_tab].location = cwd.clone();
        }
        ui::load_fonts(&app.ctx, app.tx.clone(), app.look.font.clone());
        app.watch_daemon();
        app.watch_volumes();
        thread::spawn(archive::clean_cache);
        thread::spawn(ui::install_desktop_entry);
        app.navigate(cwd, false);
        app
    }

    fn session(&self) -> Session {
        Session {
            tabs: self.tabs.iter().enumerate().map(|(i, t)| if i == self.active_tab { self.cwd.clone() } else { t.location.clone() }).collect(),
            active: self.active_tab,
            sort: self.sort,
            descending: self.descending,
            show_hidden: self.show_hidden,
            recursive: self.recursive,
            regex: self.regex,
            network_space: self.report_network_space,
            pos: self.placement.map(|(pos, _)| pos),
            size: self.placement.map(|(_, size)| size),
            maximized: self.maximized,
        }
    }

    /// Poll the index daemon's status every few seconds (cheap: one small
    /// message over a local socket).
    fn watch_daemon(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            // Only a change in what the window shows earns a repaint; the
            // rest (counts in tooltips) is picked up by the next frame.
            let mut shown: Option<(bool, Vec<u8>, String, u32)> = None;
            let mut first = true;
            loop {
                let st = proto::status().ok();
                let now = st.as_ref().map(|s| (s.ready, s.root.clone(), s.paused.clone(), s.watched));
                let changed = first || now != shown;
                first = false;
                shown = now;
                if tx.send(Msg::Daemon(st)).is_err() {
                    return;
                }
                if changed {
                    ctx.request_repaint();
                }
                thread::sleep(Duration::from_secs(5));
            }
        });
    }

    /// Re-list volumes when the mount table changes (USB sticks, network
    /// shares, `mount`), and every few seconds for GVFS shares, which don't
    /// change it. Only differences reach the UI.
    fn watch_volumes(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let mut last = self.volumes.clone();
        thread::Builder::new()
            .name("volumes".into())
            .spawn(move || {
                let mut check = move || {
                    let now = catch_unwind(|| list_volumes(true)).unwrap_or_else(|_| last.clone());
                    if now != last {
                        last = now.clone();
                        if tx.send(Msg::Volumes(now)).is_err() {
                            return false;
                        }
                        ctx.request_repaint();
                    }
                    true
                };
                if check() {
                    sys::watch_mounts(VOLUME_POLL, check);
                }
            })
            .ok();
    }

    /// Read a volume's free space on a worker, at most once a minute.
    fn request_space(&mut self, point: &[u8]) {
        if self.space_pending.contains(point) || self.space.get(point).is_some_and(|s| s.checked.elapsed() < SPACE_REFRESH) {
            return;
        }
        self.space_pending.insert(point.to_vec());
        let (point, tx, ctx) = (point.to_vec(), self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let capacity = sys::space(&point);
            let _ = tx.send(Msg::Space { point, capacity });
            ctx.request_repaint();
        });
    }

    fn daemon_covers(&self, path: &[u8]) -> bool {
        is_real(path)
            && !self.daemon_failed
            && self.daemon_retry_at.is_none_or(|t| Instant::now() >= t)
            && self.daemon.as_ref().is_some_and(|d| d.ready && fsys::in_scope(path, &d.root))
    }

    /// Run blocking work off the UI thread; a returned message is
    /// delivered. A panic is reported as an error.
    fn spawn_io(&self, f: impl FnOnce() -> Option<Msg> + Send + 'static) {
        self.spawn_io_or(f, Msg::Error);
    }

    /// `spawn_io`, where a panic delivers `on_panic(error)` instead, so state
    /// waiting for the answer (busy flags, spinners) is released.
    fn spawn_io_or(&self, f: impl FnOnce() -> Option<Msg> + Send + 'static, on_panic: impl FnOnce(String) -> Msg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let msg = catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
                let text = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
                Some(on_panic(format!("Internal error: {text}")))
            });
            if let Some(msg) = msg {
                let _ = tx.send(msg);
                ctx.request_repaint();
            }
        });
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

    fn selected(&self) -> Vec<&Row> {
        let src = self.source();
        self.view.iter().filter_map(|&i| src.get(i)).filter(|r| r.sel).collect()
    }

    fn selected_paths(&self) -> Vec<Vec<u8>> {
        self.selected().iter().map(|r| r.full_path(&self.cwd)).collect()
    }

    fn set_sel(&mut self, vi: usize, on: bool) {
        if let Some(&i) = self.view.get(vi)
            && let Some(r) = self.source_mut().get_mut(i)
        {
            r.sel = on;
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

    /// Recompute `view` after the source changed, restoring focus to `keep`.
    fn rebuild_view(&mut self, keep: Option<Vec<u8>>) {
        let hidden = self.show_hidden;
        // Without subfolder search, the query filters the open folder.
        let filter = (!self.in_results()).then(|| self.matcher.clone()).flatten();
        self.view = self
            .source()
            .iter()
            .enumerate()
            .filter(|(_, r)| (hidden || !r.is_hidden()) && filter.as_ref().is_none_or(|m| m.is_match_entry(&r.name, r.is_dir())))
            .map(|(i, _)| i)
            .collect();
        self.focus = keep.and_then(|k| (0..self.view.len()).find(|&vi| self.row(vi).is_some_and(|r| r.path_is(&self.cwd, &k))));
        self.anchor = self.focus;
        if let Some(want) = self.pending_select.clone()
            && let Some(vi) = (0..self.view.len()).find(|&vi| self.row(vi).is_some_and(|r| r.path_is(&self.cwd, &want)))
        {
            self.pending_select = None;
            self.select_only(vi);
            self.scroll_to = Some(vi);
            if std::mem::take(&mut self.pending_rename) {
                self.begin_rename(vi);
            }
        }
    }

    fn refilter(&mut self) {
        let keep = self.focus_path();
        self.rebuild_view(keep);
    }

    fn resort(&mut self) {
        let keep = self.focus_path();
        let (key, desc) = (self.sort, self.descending);
        if self.cwd != RECENT {
            fsys::sort_rows_par(&mut self.rows, key, desc);
        }
        if let Some(r) = self.results.as_mut() {
            fsys::sort_rows_par(&mut r.rows, key, desc);
            r.reindex();
        }
        self.rebuild_view(keep);
        self.scroll_to = self.focus;
    }

    fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.descending = !self.descending;
        } else {
            self.sort = key;
            self.descending = matches!(key, SortKey::Modified | SortKey::Size);
        }
        self.resort();
    }

    // ------------------------------------------------------------ tabs

    fn sync_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.location = self.cwd.clone();
            tab.back.clone_from(&self.back);
            tab.forward.clone_from(&self.forward);
        }
    }

    /// Open `location` in a new tab after the current one and switch to it.
    fn open_tab(&mut self, location: Vec<u8>) {
        self.sync_active_tab();
        self.active_tab += 1;
        self.tabs.insert(self.active_tab, Tab { location: location.clone(), back: Vec::new(), forward: Vec::new() });
        self.back.clear();
        self.forward.clear();
        self.navigate(location, false);
    }

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

    fn tab_location(&self, i: usize) -> Vec<u8> {
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

    /// Open `location` in a new FEx window the size of this one, with its
    /// top-left at `at` (points) if given. True if it started.
    fn open_window(&mut self, location: &[u8], at: Option<[f32; 2]>) -> bool {
        let spawned = std::env::current_exe().and_then(|exe| {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg(sys::os(location)).stdin(std::process::Stdio::null());
            if let Some([x, y]) = at {
                cmd.arg("--at").arg(format!("{x:.0},{y:.0}"));
            }
            if let Some((_, [w, h])) = self.placement {
                cmd.arg("--size").arg(format!("{w:.0},{h:.0}"));
            }
            cmd.spawn()
        });
        match spawned {
            Ok(child) => {
                thread::spawn(move || {
                    let mut child = child;
                    child.wait()
                });
                true
            }
            Err(error) => {
                self.error = Some(format!("Could not open a new FEx window: {error}"));
                false
            }
        }
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, path: Vec<u8>, record: bool) {
        if record && path != self.cwd {
            self.back.push(std::mem::take(&mut self.cwd));
            self.forward.clear();
        }
        self.cwd = path;
        self.path_text = lossy(&self.cwd);
        self.path_editing = false;
        self.notice = None;
        self.sync_active_tab();
        self.rows.clear();
        self.view.clear();
        self.focus = None;
        self.anchor = None;
        self.renaming = None;
        self.typeahead = None;
        self.scroll_to = Some(0);
        if !self.query.is_empty() {
            self.query.clear();
            self.clear_search();
        }
        if self.cwd == NETWORK {
            self.listing_gen += 1; // drop any listing still in flight
            self.watcher = None;
            self.loading = false;
            self.error = None;
            self.open_network_page();
        } else {
            self.load_listing();
        }
    }

    fn load_listing(&mut self) {
        self.listing_gen += 1;
        let generation = self.listing_gen;
        self.loading = true;
        self.error = None;
        self.early_changes.clear();
        self.watcher = None;
        let (path, tx, ctx) = (self.cwd.clone(), self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let t = Instant::now();
            // Watch from the start, so nothing between listing and watching
            // is missed. Here, not on the UI thread: adding a watch resolves
            // the path, which can hang on an unreachable network mount.
            let watcher = is_real(&path).then(|| {
                let (tx, ctx) = (tx.clone(), ctx.clone());
                watch::watch(path.clone(), move |changes| {
                    let _ = tx.send(Msg::Changes { generation, changes });
                    ctx.request_repaint();
                })
            }).flatten();
            // A panic here would leave the folder "Loading…" forever.
            let result = catch_unwind(AssertUnwindSafe(|| {
                if path == RECENT {
                    Ok(recent::rows())
                } else if archive::is_location(&path) {
                    archive::rows(&path)
                } else {
                    fsys::list_dir(&path)
                }
            }))
            .unwrap_or_else(|_| Err("Internal error while listing this folder".into()));
            let _ = tx.send(Msg::Listing { generation, result, took: t.elapsed(), watcher });
            ctx.request_repaint();
        });
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
        if let Some(p) = parent_of(&self.cwd) {
            // Land on the folder (or archive) we came from.
            let child = match archive::parse_location(&self.cwd) {
                Some((a, b"")) => a.to_vec(),
                Some(_) => self.cwd.clone(),
                None => self.cwd.clone(),
            };
            self.navigate(p, true);
            self.pending_select = Some(child);
        }
    }

    fn refresh(&mut self) {
        if self.cwd == NETWORK {
            self.open_network_page();
            return;
        }
        if self.in_results() {
            // F5 in search results rebuilds the index.
            self.indexes.retain(|i| !i.covers(&self.cwd));
            self.start_search();
        } else {
            let keep = self.focus_path();
            self.pending_select = keep;
            self.load_listing();
        }
    }

    /// Resolve a typed path off-thread (network paths can stall).
    fn submit_path(&mut self) {
        let text = self.path_text.trim().to_string();
        self.path_editing = false;
        if text.eq_ignore_ascii_case("Recent") || text.eq_ignore_ascii_case("Network") {
            let loc = if text.eq_ignore_ascii_case("Recent") { RECENT } else { NETWORK };
            return self.navigate(loc.to_vec(), true);
        }
        if archive::is_location(text.as_bytes()) {
            return self.navigate(text.into_bytes(), true);
        }
        // A share address goes to the Network page's connect flow.
        if let Some(url) = net::to_url(&text) {
            return self.connect_share(url, None);
        }
        let base = if is_real(&self.cwd) { self.cwd.clone() } else { sys::home() };
        let path = fsys::resolve_input(&text, &base);
        self.spawn_io(move || {
            Some(match fileop::stat(&path) {
                Some(_) if sys::is_dir(&path) => Msg::Navigate(path),
                Some(_) => match sys::open_with_default(&path) {
                    Ok(()) => {
                        recent::record(&path);
                        Msg::RecentChanged
                    }
                    Err(e) => Msg::Error(e),
                },
                None => Msg::Error(format!("Cannot find '{}'", lossy(&path))),
            })
        });
    }

    fn open(&mut self, vi: usize) {
        let Some(row) = self.row(vi) else { return };
        let path = row.full_path(&self.cwd);
        if row.is_dir() {
            self.navigate(path, true);
        } else if archive::is_location(&self.cwd) {
            self.spawn_io(move || match archive::extract_member(&path) {
                Ok(file) => sys::open_with_default(&file).err().map(Msg::Error),
                Err(error) => Some(Msg::Error(error)),
            });
        } else if archive::is_archive_file(&path) {
            self.navigate(archive::root_location(&path), true);
        } else {
            self.spawn_io(move || match sys::open_with_default(&path) {
                Ok(()) => {
                    recent::record(&path);
                    Some(Msg::RecentChanged)
                }
                Err(e) => Some(Msg::Error(e)),
            });
        }
    }

    /// Run a program as root through polkit (`pkexec`), the counterpart of
    /// Windows' "Run as administrator". Only for executable files.
    fn run_as(&mut self, vi: usize) {
        if !is_real(&self.cwd) && self.cwd != RECENT {
            return;
        }
        let Some(row) = self.row(vi).filter(|r| !r.is_dir()) else { return };
        let path = row.full_path(&self.cwd);
        self.spawn_io(move || {
            if !sys::access(&path).2 {
                return Some(Msg::Error(format!("{} is not a program (it isn't executable)", lossy(fsys::base_name(&path)))));
            }
            let child = std::process::Command::new("pkexec").arg(sys::os(&path)).stdin(std::process::Stdio::null()).spawn();
            match child {
                Ok(mut c) => {
                    recent::record(&path);
                    thread::spawn(move || c.wait());
                    Some(Msg::RecentChanged)
                }
                Err(e) => Some(Msg::Error(format!("Could not run pkexec: {e}"))),
            }
        });
    }

    // ------------------------------------------------------- file operations

    fn start_job(&mut self, kind: Kind, sources: Vec<Vec<u8>>, dest: Vec<u8>) -> Option<Arc<Job>> {
        if sources.is_empty() {
            return None;
        }
        if !accepts_files(&dest) || sources.iter().any(|s| archive::is_location(s)) {
            self.error = Some("Choose a real folder to paste into".into());
            return None;
        }
        let ctx = self.ctx.clone();
        let job = ops::start(kind, sources, dest, Arc::new(move || ctx.request_repaint()));
        self.jobs.push(job.clone());
        self.history_jobs.push((job.clone(), Pending::Record));
        Some(job)
    }

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
                    Step::Rename { from, to } => fileop::rename(&from, &to, false).map_err(|e| format!("{}: {}", lossy(&from), sys::error_text(e))),
                    Step::Trash(paths) => {
                        let (_, errors) = trash::trash(&paths);
                        if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
                    }
                    Step::Restore(paths) => trash::restore(&paths),
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
            let created = job.created.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            if job.kind == Kind::Move {
                for (from, to) in &created {
                    recent::renamed(from, to);
                }
            }
            let created = Entry::Transfer { kind: job.kind, pairs: created };
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
        if archive::is_location(&self.cwd) {
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        match clip::set_files(&paths, cut) {
            Ok(seq) => {
                self.cut_marks = if cut { paths.into_iter().collect() } else { HashSet::new() };
                self.cut_seq = seq;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn paste(&mut self, into: Option<Vec<u8>>) {
        let dest = into.unwrap_or_else(|| self.cwd.clone());
        if !accepts_files(&dest) {
            return;
        }
        // Asking the clipboard's owner can take a while (or time out):
        // off the UI thread.
        self.spawn_io(move || {
            let (files, cut) = clip::get_files()?;
            Some(Msg::Paste { files, cut, dest, seq: clip::sequence() })
        });
    }

    fn paste_files(&mut self, files: Vec<Vec<u8>>, cut: bool, dest: Vec<u8>, seq: u32) {
        if let Some(job) = self.start_job(if cut { Kind::Move } else { Kind::Copy }, files, dest)
            && cut
        {
            self.cut_job = Some((job, seq));
        }
    }

    /// Drag/drop semantics match the file managers': Ctrl copies, Shift
    /// moves, and otherwise the same filesystem moves while another copies.
    fn drop_kind(mods: Modifiers, same_volume: bool) -> Kind {
        if mods.ctrl {
            Kind::Copy
        } else if mods.shift || same_volume {
            Kind::Move
        } else {
            Kind::Copy
        }
    }

    fn begin_rename(&mut self, vi: usize) {
        if !is_real(&self.cwd) && self.cwd != RECENT && !self.in_results() {
            return;
        }
        if let Some(r) = self.row(vi) {
            let name = r.display().into_owned();
            self.renaming = Some(Rename { path: r.full_path(&self.cwd), original: name.clone(), text: name, focused: false });
        }
    }

    fn commit_rename(&mut self, r: Rename) {
        let new = r.text.trim().to_string();
        if new.is_empty() || new == r.original {
            return;
        }
        if new.contains('/') || new == "." || new == ".." {
            self.error = Some("A name can't contain / or be . or ..".into());
            return;
        }
        let dir = fsys::parent(&r.path).unwrap_or_default();
        let dst = fsys::join(&dir, new.as_bytes());
        self.pending_select = Some(dst.clone());
        self.spawn_io(move || {
            Some(match fileop::rename(&r.path, &dst, false) {
                Ok(()) => Msg::Renamed { old_path: r.path, new_name: new.into_bytes() },
                Err(libc::EEXIST) => Msg::Error(format!("Rename failed: '{new}' already exists")),
                Err(e) => Msg::Error(format!("Rename failed: {}", sys::error_text(e))),
            })
        });
    }

    fn new_folder(&mut self) {
        if !is_real(&self.cwd) {
            return;
        }
        let dir = self.cwd.clone();
        self.spawn_io(move || {
            let name = ops::unique_name(&dir, b"New folder", false);
            Some(match fileop::create_dir(&fsys::join(&dir, &name)) {
                Ok(()) => Msg::Created { dir, name },
                Err(e) => Msg::Error(format!("Could not create folder: {}", sys::error_text(e))),
            })
        });
    }

    /// A new empty text file, then rename it.
    fn new_file(&mut self) {
        if !is_real(&self.cwd) {
            return;
        }
        let dir = self.cwd.clone();
        self.spawn_io(move || Some(match templates::create(&dir, &templates::Item::text_file()) {
            Ok(name) => Msg::Created { dir, name },
            Err(error) => Msg::Error(error),
        }));
    }

    fn load_templates(&mut self) {
        if self.templates_loading || self.templates.is_some() {
            return;
        }
        self.templates_loading = true;
        self.spawn_io_or(|| Some(Msg::Templates(templates::load())), |_| Msg::Templates(Vec::new()));
    }

    fn create_from_template(&mut self, index: usize) {
        if !is_real(&self.cwd) {
            return;
        }
        let Some(item) = self.templates.as_ref().and_then(|items| items.get(index)).cloned() else { return };
        let dir = self.cwd.clone();
        self.spawn_io(move || {
            Some(match templates::create(&dir, &item) {
                Ok(name) => Msg::Created { dir, name },
                Err(error) => Msg::Error(error),
            })
        });
    }

    /// Extract archive members (`zip:` locations) into the folder `dest`.
    fn extract_to(&mut self, members: Vec<Vec<u8>>, dest: Vec<u8>) {
        if !accepts_files(&dest) {
            self.error = Some("Choose a real folder to extract into".into());
            return;
        }
        self.notice = Some(format!("Extracting {} item(s)…", members.len()));
        self.spawn_io(move || {
            let (created, errors) = archive::extract_members(&members, &dest);
            Some(Msg::Extracted { created, open: None, errors })
        });
    }

    /// Prepare a volume for removal. FEx first lets go of everything it
    /// holds open there (the folder watcher, a running index), since any
    /// open file makes the unmount fail.
    fn eject(&mut self, point: Vec<u8>) {
        let Some(vol) = self.volumes.iter().find(|v| v.point == point).cloned() else { return };
        let on_drive = |p: &[u8]| fsys::in_scope(archive::parse_location(p).map_or(p, |(a, _)| a), &point);
        if self.jobs.iter().any(|j| !j.finished() && j.touches(&point)) {
            self.error = Some(format!("Wait for transfers involving {} to finish before ejecting it", lossy(&point)));
            return;
        }
        if on_drive(&self.cwd) {
            self.navigate(sys::home(), true);
        }
        if self.build.as_ref().is_some_and(|b| on_drive(&b.root))
            && let Some(b) = self.build.take()
        {
            b.cancel();
        }
        self.indexes.retain(|i| !on_drive(&i.root));
        self.props = None;
        self.error = None;
        let name = lossy(fsys::base_name(&vol.point));
        self.notice = Some(format!("Preparing {name} for removal…"));
        self.spawn_io(move || {
            Some(match fileop::eject(&vol.point, &vol.source) {
                Ok(()) => Msg::DriveEjected(name),
                Err(e) => Msg::Error(format!("Could not eject {name}: {e}")),
            })
        });
    }

    fn open_properties(&mut self) {
        if archive::is_location(&self.cwd) || self.cwd == NETWORK {
            return;
        }
        let mut items: Vec<props::Item> = self
            .selected()
            .iter()
            .map(|r| props::Item { path: r.full_path(&self.cwd), name: r.display().into_owned(), flags: r.flags, size: r.size })
            .collect();
        if items.is_empty() {
            if let Some(r) = self.focus.and_then(|vi| self.row(vi)) {
                items.push(props::Item { path: r.full_path(&self.cwd), name: r.display().into_owned(), flags: r.flags, size: r.size });
            } else if is_real(&self.cwd) {
                return self.open_folder_properties();
            }
        }
        if !items.is_empty() {
            self.props = Some(Props::new(items, self.ctx.clone()));
        }
    }

    fn open_folder_properties(&mut self) {
        if !is_real(&self.cwd) {
            return;
        }
        let item = props::Item { path: self.cwd.clone(), name: lossy(fsys::base_name(&self.cwd)), flags: sys::DIR, size: 0 };
        self.props = Some(Props::new(vec![item], self.ctx.clone()));
    }

    /// Fold watcher notifications into the listing: new entries are inserted
    /// in sort order, existing ones only get their metadata updated in place
    /// (no re-sorting), and deleted ones are removed.
    fn apply_changes(&mut self, changes: Vec<Change>) {
        if changes.iter().any(|c| matches!(c, Change::Rescan)) {
            self.pending_select = self.focus_path();
            return self.load_listing();
        }
        let resolved: Vec<(Change, Option<usize>)> = {
            let table: Option<HashMap<&[u8], usize>> = (changes.len() > 32).then(|| self.rows.iter().enumerate().map(|(i, r)| (&*r.name, i)).collect());
            let find = |name: &[u8]| match &table {
                Some(t) => t.get(name).copied(),
                None => self.rows.iter().position(|r| &*r.name == name),
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
                        let r = &mut self.rows[i];
                        r.size = row.size;
                        r.mtime = row.mtime;
                        r.flags = row.flags;
                        removed.remove(&i);
                    }
                    None if !added.iter().any(|a| a.name == row.name) => added.push(row),
                    None => {}
                },
                Change::Removed(name) => {
                    if let Some(i) = found {
                        removed.insert(i);
                    }
                    added.retain(|a| a.name != name);
                }
                Change::Rescan => {}
            }
        }
        if removed.is_empty() && added.is_empty() {
            return; // metadata only: rows were updated in place
        }
        let keep = self.focus_path();
        if !removed.is_empty() {
            let mut i = 0;
            self.rows.retain(|_| {
                i += 1;
                !removed.contains(&(i - 1))
            });
        }
        if added.len() > 64 {
            // Many at once (an extraction, a big copy landing): one sort
            // beats shifting the listing once per new row.
            self.rows.extend(added);
            if self.cwd != RECENT {
                fsys::sort_rows_par(&mut self.rows, self.sort, self.descending);
            }
        } else {
            for row in added {
                let pos = self.rows.partition_point(|r| fsys::cmp_rows(r, &row, self.sort, self.descending).is_lt());
                self.rows.insert(pos, row);
            }
        }
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
        let had = self.results.take().is_some();
        if had || !self.recursive {
            self.refilter();
        }
    }

    fn start_search(&mut self) {
        self.query_dirty = None;
        let m = match Matcher::new(&self.query, self.regex) {
            Ok(Some(m)) => Arc::new(m),
            Ok(None) => return self.clear_search(),
            Err(e) => {
                self.search_error = Some(e.lines().last().unwrap_or("invalid pattern").trim().to_string());
                return;
            }
        };
        self.search_error = None;
        self.matcher = Some(m.clone());
        // Filter mode, and places without an index: filter the listing.
        if !self.recursive || !is_real(&self.cwd) {
            self.query_gen.fetch_add(1, Relaxed);
            self.results = None;
            self.searching = false;
            self.scroll_to = Some(0);
            return self.refilter();
        }
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

    /// Ask the daemon: its index is always ready and current. The first
    /// screenful arrives sorted and is shown at once; the rest streams in.
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
            let ask = || proto::query(&scope, &text, regex, MAX_RESULTS as u32, order, SCREENFUL, generation);
            let mut reply = ask();
            if reply.is_err() && current.load(Relaxed) == generation {
                thread::sleep(Duration::from_millis(150));
                reply = ask();
            }
            match reply {
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
                Ok(_) => {
                    send(Msg::DaemonFailed { generation, definitive: true });
                }
                Err(_) => {
                    send(Msg::DaemonFailed { generation, definitive: false });
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

    /// Give arriving rows ids and queue them for metadata. Streamed rows are
    /// appended as they come and the whole set is sorted once, in parallel,
    /// when `sort` is set: for the first batch and when the last arrived.
    fn add_result_rows(&mut self, mut rows: Vec<Row>, sort: bool) {
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
        if !sort {
            let hidden = self.show_hidden;
            let start = r.rows.len();
            r.pos.resize(start + rows.len(), 0);
            for (i, row) in rows.iter().enumerate() {
                r.pos[row.id as usize] = (start + i) as u32;
            }
            self.view.extend((start..start + rows.len()).filter(|&i| hidden || !rows[i - start].is_hidden()));
            r.rows.extend(rows);
            return;
        }
        r.rows.extend(rows);
        let keep = self.focus_path();
        let (key, desc) = (self.sort, self.descending);
        if let Some(r) = self.results.as_mut() {
            fsys::sort_rows_par(&mut r.rows, key, desc);
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

    // ------------------------------------------------------------ messages

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Listing { generation, result, took, watcher } if generation == self.listing_gen => {
                    self.loading = false;
                    self.watcher = watcher;
                    self.listed_in = took;
                    // On refresh, keep what was selected.
                    let keep: HashSet<Box<[u8]>> = self.rows.iter().filter(|r| r.sel).map(|r| r.name.clone()).collect();
                    match result {
                        Ok(mut rows) => {
                            for r in rows.iter_mut() {
                                r.sel = keep.contains(&r.name);
                            }
                            if self.cwd != RECENT {
                                fsys::sort_rows_par(&mut rows, self.sort, self.descending);
                            }
                            self.rows = rows;
                        }
                        Err(e) => {
                            self.rows.clear();
                            self.error = Some(e);
                        }
                    }
                    if !self.in_results() {
                        let keep = self.pending_select.clone().or_else(|| self.focus_path());
                        self.rebuild_view(keep);
                        if self.scroll_to.is_none() {
                            self.scroll_to = self.focus;
                        }
                    }
                    let early = std::mem::take(&mut self.early_changes);
                    if !early.is_empty() {
                        self.apply_changes(early);
                    }
                }
                Msg::Changes { generation, changes } if generation == self.listing_gen => {
                    if self.loading {
                        self.early_changes.extend(changes);
                    } else {
                        self.apply_changes(changes);
                    }
                }
                Msg::Built(Some(index)) => {
                    let index = Arc::new(index);
                    self.build = None;
                    self.indexes.retain(|i| !index.covers(&i.root));
                    self.indexes.insert(0, index.clone());
                    self.indexes.truncate(MAX_INDEXES);
                    if let (Some(m), true, true) = (self.matcher.clone(), index.covers(&self.cwd), self.recursive) {
                        self.run_query(index, m);
                    }
                }
                Msg::Built(None) => {}
                Msg::Results { generation, rows, total, first, complete } if generation == self.query_gen.load(Relaxed) => {
                    self.searching = false;
                    let (fill_tx, fill_rx) = channel();
                    let (current, tx, ctx) = (self.query_gen.clone(), self.tx.clone(), self.ctx.clone());
                    thread::spawn(move || backfill(generation, current, fill_rx, tx, ctx));
                    // A refreshed result set keeps what was selected.
                    let selected: HashSet<Vec<u8>> = self.selected_paths().into_iter().collect();
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
                    let mut rows = rows;
                    if !selected.is_empty() {
                        for r in rows.iter_mut() {
                            r.sel = selected.contains(&r.full_path(&self.cwd));
                        }
                    }
                    self.add_result_rows(rows, true);
                    self.focus = None;
                    self.scroll_to = Some(0);
                    if complete {
                        self.apply_meta(Vec::new());
                    }
                }
                Msg::MoreRows { generation, rows, complete } if generation == self.query_gen.load(Relaxed) => {
                    self.add_result_rows(rows, complete);
                    if let (true, Some(r)) = (complete, self.results.as_mut()) {
                        r.all = Some(r.started.elapsed());
                    }
                    self.apply_meta(Vec::new());
                }
                Msg::Meta { generation, filled } if generation == self.query_gen.load(Relaxed) => self.apply_meta(filled),
                Msg::Fonts(name) => self.font = name,
                Msg::Paste { files, cut, dest, seq } => self.paste_files(files, cut, dest, seq),
                Msg::Daemon(st) => {
                    self.daemon_failed = false;
                    self.daemon = st;
                }
                Msg::DaemonFailed { generation, definitive } if generation == self.query_gen.load(Relaxed) => {
                    // Fall back to searching in-process until the daemon's
                    // next good status.
                    self.daemon_failed = true;
                    if !definitive {
                        self.daemon_retry_at = Some(Instant::now() + Duration::from_secs(60));
                        self.notice = Some("The index daemon isn't answering; searching this folder directly".into());
                    }
                    self.start_search();
                }
                Msg::Volumes(volumes) => {
                    self.space.retain(|p, _| volumes.iter().any(|v| v.point == *p));
                    self.volumes = volumes;
                }
                Msg::Space { point, capacity } => {
                    self.space_pending.remove(&point);
                    self.space.insert(point, Space { capacity, checked: Instant::now() });
                }
                Msg::DriveEjected(name) => self.notice = Some(format!("You can now safely remove {name}")),
                Msg::Templates(items) => {
                    self.templates_loading = false;
                    self.templates = Some(items);
                }
                Msg::RecentChanged if self.cwd == RECENT => self.load_listing(),
                Msg::RecentChanged => {}
                Msg::Navigate(p) => self.navigate(p, true),
                Msg::Created { dir, name } => {
                    let path = fsys::join(&dir, &name);
                    self.history.record(Entry::Create(vec![path.clone()]));
                    if dir != self.cwd {
                        continue;
                    }
                    // Don't wait for the watcher: show it and start renaming now.
                    if self.rows.iter().all(|r| *r.name != *name)
                        && let Some(st) = fileop::stat(&path)
                    {
                        self.apply_changes(vec![Change::Upsert(Row { name: name.into(), dir: None, size: 0, mtime: st.mtime, flags: st.flags, sel: false, id: 0 })]);
                    }
                    self.pending_select = Some(path);
                    self.pending_rename = true;
                    self.refilter();
                }
                Msg::Renamed { old_path, new_name } => {
                    if let Some(parent) = fsys::parent(&old_path) {
                        let new_path = fsys::join(&parent, &new_name);
                        recent::renamed(&old_path, &new_path);
                        self.history.record(Entry::Rename { from: old_path.clone(), to: new_path });
                    }
                    let cwd = self.cwd.clone();
                    let rows = self.rows.iter_mut().chain(self.results.iter_mut().flat_map(|r| r.rows.iter_mut()));
                    for r in rows {
                        if r.path_is(&cwd, &old_path) {
                            r.name = new_name.clone().into();
                        }
                    }
                    self.refilter();
                }
                Msg::Trashed { done, errors } => {
                    self.history.record(Entry::Trash(done));
                    if !errors.is_empty() {
                        self.error = Some(format!("{} — use Shift+Delete to delete permanently", errors.join("; ")));
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
                Msg::HistoryDone { entry, undo, result } => {
                    self.history.busy = false;
                    self.notice = None;
                    match result {
                        Ok(()) => {
                            if let Entry::Rename { from, to } = &entry {
                                // Keep recent files and the selection on the renamed item.
                                let (was, now) = if undo { (to, from) } else { (from, to) };
                                recent::renamed(was, now);
                                if fsys::parent(now).as_deref() == Some(self.cwd.as_slice()) {
                                    self.pending_select = Some(now.clone());
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
                Msg::Error(e) => {
                    self.notice = None;
                    self.error = Some(e);
                }
                Msg::Shares { server, result } => {
                    self.net.shares.insert(server, Some(result));
                }
                Msg::NetDone(result) => self.net_done(result),
                _ => {} // stale generation
            }
        }
    }

    // ------------------------------------------------------------ keyboard

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));

        if pressed(Modifiers::NONE, Key::F5) {
            self.refresh();
        }
        if pressed(Modifiers::NONE, Key::F1) {
            self.help_open = !self.help_open;
        }
        // Escape closes the shortcuts sheet before it does anything else.
        if self.help_open && pressed(Modifiers::NONE, Key::Escape) {
            self.help_open = false;
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
        let modal = self.confirm_delete.is_some() || self.net.dialog_open() || self.jobs.iter().any(|j| j.phase() == Phase::AwaitingDecision);
        if typing || modal {
            if typing && pressed(Modifiers::NONE, Key::Escape) {
                self.path_editing = false;
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
        let typed: String = ctx.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect());
        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
        if !typed.is_empty() && !(typed == " " && !live) && !ctx.input(|i| i.modifiers.ctrl || i.modifiers.alt) {
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
                self.clear_search();
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
        if pressed(Modifiers::ALT, Key::ArrowUp) || (!self.in_results() && pressed(Modifiers::NONE, Key::Backspace)) {
            self.go_up();
        }
        // Before plain Enter, which would also match Alt+Enter.
        if pressed(Modifiers::ALT, Key::Enter) {
            self.open_properties();
            return;
        }
        if pressed(Modifiers::NONE, Key::Enter)
            && let Some(i) = self.focus
        {
            self.open(i);
            return;
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
        if pressed(Modifiers::NONE, Key::F2)
            && let Some(i) = self.focus.filter(|&i| self.row(i).is_some_and(|r| r.sel))
        {
            self.begin_rename(i);
        }
        let permanent = pressed(Modifiers::SHIFT, Key::Delete);
        if !archive::is_location(&self.cwd) && (permanent || pressed(Modifiers::NONE, Key::Delete)) {
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

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refilter();
    }

    /// Jump to the best match for `text` (see `typeahead::score`). A single
    /// letter, or the same letter repeated ("ddd"), cycles through names
    /// starting with it, as on Windows.
    fn type_to_find(&mut self, text: String) {
        let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
        let first = text.chars().next().map(fold).unwrap_or(' ');
        let starts = |r: &Row| r.display().chars().next().map(fold) == Some(first);
        let repeated = text.chars().map(fold).all(|c| c == first);
        let n = self.view.len();
        let lower = text.to_lowercase();
        let whole_prefix = |r: &Row| r.display().to_lowercase().starts_with(&lower);
        let found: Option<(usize, Vec<usize>)> = if repeated && (text.chars().count() == 1 || !(0..n).any(|vi| self.row(vi).is_some_and(whole_prefix))) {
            let from = match self.focus {
                Some(f) if self.row(f).is_some_and(starts) => f + 1,
                _ => 0,
            };
            (0..n).map(|k| (from + k) % n.max(1)).find(|&vi| self.row(vi).is_some_and(starts)).map(|vi| (vi, vec![0]))
        } else {
            let mut best: Option<(i32, usize, Vec<usize>)> = None;
            for vi in 0..n {
                if let Some(m) = self.row(vi).and_then(|r| typeahead::score(&text, &r.display()))
                    && best.as_ref().is_none_or(|b| m.score > b.0)
                {
                    best = Some((m.score, vi, m.positions));
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
            None => (Vec::new(), Vec::new()),
        };
        self.typeahead = Some(TypeAhead { text, last: Instant::now(), path, positions });
        self.ctx.request_repaint_after(TYPEAHEAD_RESET);
    }

    fn run_action(&mut self, action: Option<Action>, ctx: &egui::Context) {
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
            Action::ExtractArchive(i) => {
                if let Some(row) = self.row(i) {
                    let path = row.full_path(&self.cwd);
                    self.notice = Some(format!("Extracting {}…", row.display()));
                    self.spawn_io(move || {
                        Some(match archive::extract_archive(&path) {
                            Ok(destination) => Msg::Extracted { created: vec![destination.clone()], open: Some(destination), errors: Vec::new() },
                            Err(error) => Msg::Error(error),
                        })
                    });
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
                    let dir = fsys::parent(&target).unwrap_or_else(|| self.cwd.clone());
                    let name = row.name.to_vec();
                    self.spawn_io(move || {
                        let ext = fsys::ext(&name);
                        let link_name = if row.is_dir() || ext.is_empty() {
                            [name.as_slice(), b" - Link"].concat()
                        } else {
                            [&name[..name.len() - ext.len() - 1], b" - Link.", ext].concat()
                        };
                        let name = ops::unique_name(&dir, &link_name, false);
                        // Relative to the folder it sits in, so it survives moving both together.
                        Some(match fileop::symlink(fsys::base_name(&target), &fsys::join(&dir, &name)) {
                            Ok(()) => Msg::Created { dir, name },
                            Err(code) => Msg::Error(format!("Could not create link: {}", sys::error_text(code))),
                        })
                    });
                }
            }
            Action::EjectDrive(path) => self.eject(path),
            Action::RunAs(i) => {
                self.select_only(i);
                self.run_as(i);
            }
            Action::OpenLocation(i) => {
                if let Some(row) = self.row(i) {
                    let path = row.full_path(&self.cwd);
                    if let Some(dir) = row.dir.as_ref().map(|d| d.to_vec()) {
                        self.navigate(dir, true);
                        self.pending_select = Some(path);
                    }
                }
            }
            Action::CopyPath(i) => {
                let paths = self.selected_paths();
                let text = if paths.len() > 1 {
                    paths.iter().map(|p| lossy(p)).collect::<Vec<_>>().join("\n")
                } else {
                    self.row(i).map(|r| lossy(&r.full_path(&self.cwd))).unwrap_or_default()
                };
                ctx.copy_text(text);
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
                self.open_folder_properties();
            }
            Action::NewFolder => self.new_folder(),
            Action::NewFile => self.new_file(),
            Action::NewItem(index) => self.create_from_template(index),
            Action::LoadTemplates => self.load_templates(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::ClearSelection => self.clear_sel(),
            Action::Refresh => self.refresh(),
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

    /// Time until the next finished job fades out of the jobs panel.
    fn next_dismissal(&self) -> Option<Duration> {
        self.jobs
            .iter()
            .filter(|j| j.phase() == Phase::Done && j.error_count() == 0)
            .filter_map(|j| {
                let took = (*j.elapsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner))?;
                Some((took + ui::JOB_LINGER).saturating_sub(j.started.elapsed()) + Duration::from_millis(50))
            })
            .min()
    }

    /// Start dragging files out to another program (see xdnd.rs).
    fn drag_out(&mut self, paths: Vec<Vec<u8>>) {
        self.spawn_io(move || match xdnd::drag_files(&paths) {
            Ok(_) => None,
            Err(e) => Some(Msg::Error(format!("Could not drag the files out: {e}"))),
        });
    }

    /// Files dragged in from other programs (winit's XDND drop target).
    /// They land on the folder under the pointer, else the open folder.
    fn handle_os_drop(&mut self, ctx: &egui::Context) {
        let (hovered, dropped) = ctx.input(|i| (i.raw.hovered_files.len(), i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| p.is_absolute()).collect::<Vec<_>>()));
        if hovered == 0 && dropped.is_empty() {
            return;
        }
        // During an XDND drag winit doesn't report the pointer; ask X.
        let (pos, mods) = match xdnd::pointer(ctx) {
            Some((p, m)) => (Some(p), m),
            None => (ctx.input(|i| i.pointer.latest_pos()), ctx.input(|i| i.modifiers)),
        };
        let zone = pos.and_then(|p| self.zones.iter().rev().find(|(r, _)| r.contains(p)).cloned());
        let dest = zone.as_ref().map(|z| z.1.clone()).or_else(|| accepts_files(&self.cwd).then(|| self.cwd.clone()));
        if hovered > 0 {
            let what = if hovered == 1 { "1 item".to_string() } else { format!("{hovered} items") };
            let (text, outline) = match &dest {
                None => ("Can't drop here: open a folder first".to_string(), None),
                Some(d) => (format!("Copy or move {what} to {}", ui::tab_label(d)), Some(zone.map_or(self.rows_rect, |z| z.0))),
            };
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
            let sel = ctx.global_style().visuals.selection.stroke.color;
            if let Some(r) = outline {
                painter.rect(r, 2.0, sel.gamma_multiply(0.12), egui::Stroke::new(2.0, sel), egui::StrokeKind::Inside);
            }
            if let Some(p) = pos {
                egui::Area::new(egui::Id::unique("os_drop_label")).order(egui::Order::Tooltip).fixed_pos(p + egui::vec2(16.0, 12.0)).interactable(false).show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
                });
            }
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if let (false, Some(dest)) = (dropped.is_empty(), dest) {
            let paths: Vec<Vec<u8>> = dropped.into_iter().map(|p| p.into_os_string().into_vec()).collect();
            let same = fileop::same_volume(&paths[0], &dest);
            self.start_job(Self::drop_kind(mods, same), paths, dest);
        }
    }
}

use std::os::unix::ffi::OsStringExt;

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.session().save();
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.transparent {
            self.look.clear_color(visuals)
        } else {
            // An opaque window can't show through; paint the tint solid.
            let mut c = visuals.panel_fill.to_normalized_gamma_f32();
            c[3] = 1.0;
            c
        }
    }

    /// Clipboard shortcuts are delivered as plain key events (see mod.rs) so
    /// Ctrl+C/X/V can act on files. While a text field has focus, turn them
    /// back into the text clipboard events egui's TextEdit expects.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !ctx.egui_wants_keyboard_input() {
            return;
        }
        for ev in raw.events.iter_mut() {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev
                && modifiers.command_only()
            {
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

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.update_blur(frame);
        // A bug must not take the window (and whatever the user was doing)
        // down with it: the frame is abandoned, the details go to the crash
        // log (the panic hook), and the window carries on.
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.frame(ui))) {
            self.recover(panic);
        }
    }
}

impl App {
    /// Ask the compositor for blur behind a see-through window when the
    /// setting changes (an X round trip, so off the UI thread, and once).
    fn update_blur(&mut self, frame: &eframe::Frame) {
        let want = self.transparent && self.look.theme == super::appearance::Theme::Glass && self.look.glass_blur;
        if self.blur == Some(want) {
            return;
        }
        self.blur = Some(want);
        if let Some(win) = super::appearance::x11_window(frame) {
            thread::spawn(move || {
                let _ = super::appearance::request_blur(win, want);
            });
        }
    }

    /// Put a changed look into effect and remember it.
    fn set_look(&mut self, look: Appearance) {
        if look == self.look {
            return;
        }
        let font_changed = look.font != self.look.font;
        let colours_changed = look.visuals() != self.look.visuals() || look.scale != self.look.scale;
        self.look = look;
        if colours_changed {
            self.look.install(&self.ctx);
        }
        if font_changed {
            ui::load_fonts(&self.ctx, self.tx.clone(), self.look.font.clone());
        }
        self.ctx.request_repaint();
    }

    fn save_look(&mut self) {
        if let Err(e) = self.look.save() {
            self.error = Some(e);
        }
    }

    /// After a panic in a frame: drop what was in progress, say so briefly,
    /// and, if it keeps happening, go back to a plain view of the home folder.
    fn recover(&mut self, panic: Box<dyn std::any::Any + Send>) {
        let text = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
        let now = Instant::now();
        self.panics.retain(|t| now.duration_since(*t) < Duration::from_secs(10));
        self.panics.push(now);
        self.drag = None;
        self.band = None;
        self.renaming = None;
        self.tab_drag = None;
        self.typeahead = None;
        self.props = None;
        self.scroll_to = Some(0);
        let mut msg = format!("Internal error: {text} — details in ~/.local/state/fex/crash.log");
        if self.panics.len() >= 3 {
            self.panics.clear();
            self.query.clear();
            self.clear_search();
            self.navigate(sys::home(), false);
            msg.push_str("; FEx reset the view");
        }
        self.error = Some(msg);
        self.ctx.request_repaint();
    }

    fn frame(&mut self, ui: &mut egui::Ui) {
        self.drain_messages();
        if let Some(t) = self.query_dirty {
            let waited = t.elapsed();
            if waited >= QUERY_DEBOUNCE {
                self.start_search();
            } else {
                ui.ctx().request_repaint_after(QUERY_DEBOUNCE - waited);
            }
        }
        // Cut marks disappear once something else is put on the clipboard.
        if !self.cut_marks.is_empty() && clip::sequence() != self.cut_seq {
            self.cut_marks.clear();
        }
        if let Some((job, seq)) = &self.cut_job
            && job.finished()
        {
            if job.phase() == Phase::Done && job.error_count() == 0 {
                clip::clear_if_unchanged(*seq);
            }
            self.cut_marks.clear();
            self.cut_job = None;
        }
        let ctx = ui.ctx().clone();
        if self.renaming.is_none() {
            self.handle_keys(&ctx);
        }
        self.drop_target = None;
        let zones = std::mem::take(&mut self.zones);
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
        self.draw(ui, &ctx);
        // Drops from other programs use the zones laid out last frame.
        let fresh = std::mem::replace(&mut self.zones, zones);
        self.handle_os_drop(&ctx);
        self.zones = fresh;

        if self.build.is_some() || self.jobs.iter().any(|j| !j.finished()) {
            // Progress counters live in atomics; tick the UI.
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some(wait) = self.next_dismissal() {
            ctx.request_repaint_after(wait); // auto-dismiss timer
        }
        // Only when it changes: each title command is a round trip to the X server.
        let title = format!("FEx — {}", lossy(&self.cwd));
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_volumes_like_windows_drives() {
        let m = |point: &str, fstype: &str, source: &str| sys::Mount { point: point.into(), fstype: fstype.into(), source: source.into() };
        let nas = volume_of_mount(m("/mnt/media", "cifs", "//nas/media"));
        assert_eq!(nas.label, "🖧 media  (nas)");
        assert_eq!(nas.tip, "//nas/media on /mnt/media (cifs)");
        assert!(nas.network && !nas.gvfs);
        let nfs = volume_of_mount(m("/home/shared", "nfs4", "files.lan:/export/shared"));
        assert_eq!(nfs.label, "🖧 shared  (files.lan)");
        let root = volume_of_mount(m("/", "ext4", "/dev/sda2"));
        assert_eq!(root.label, "🖴 File system  (ext4)");
        assert!(!root.network);
    }

    #[test]
    fn locations() {
        assert!(is_real(b"/home") && !is_real(RECENT) && !is_real(NETWORK) && !is_real(b"zip:/a.zip|"));
        assert_eq!(parent_of(b"zip:/d/a.zip|x").as_deref(), Some(&b"zip:/d/a.zip|"[..]));
        assert_eq!(parent_of(b"/d"), Some(b"/".to_vec()));
        assert_eq!(parent_of(RECENT), None);
    }
}
