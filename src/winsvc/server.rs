//! The index service: keeps each fixed drive's index ready and current,
//! and answers queries over a named pipe, filtered per client.
//!
//! * Start: each drive's saved index loads at once; the NTFS change journal
//!   (`usn.rs`) replays what changed while the service was stopped, so no
//!   crawl or verification is needed. Without a saved index, or if the
//!   journal no longer reaches back, the drive is crawled in the background
//!   (serving the saved copy meanwhile).
//! * Running: the index's own recursive watcher (`ReadDirectoryChangesW`)
//!   applies changes as they happen; indexes are saved with the journal
//!   position every few minutes and on stop.
//! * Priorities: crawler threads run in background mode with the CPU reserve
//!   (`throttle.rs`); queries and index updates run at normal priority.
//! * Visibility: identity comes from the pipe (the client's token, never the
//!   request). Each folder with matches is checked with `AccessCheck`
//!   against the folder's security descriptor, read once and cached across
//!   queries (`access.rs`), so a query opens no files; the cache follows the
//!   change journal. Where that shortcut isn't valid (a token without the
//!   traverse bypass, a drive without a journal), workers take on the token
//!   and open each folder instead. Hidden matches are left out of the count.
//!   Anything that can't be checked is hidden. SYSTEM clients see everything.
//! * Memory: the process runs at below-normal memory priority. When memory
//!   gets tight (`memory.rs`), background work pauses and the service gives
//!   back what it can: freed heap and its working set (the index pages
//!   back in on the next query); when critical, the descriptor cache too,
//!   read again in the background once memory is normal.
//! * A client's newer query cancels its older ones (search as you type).
//! * The pipe: created first-instance-only (nobody else can own the name),
//!   remote clients refused, any authenticated user may connect. A client
//!   gets 5 s to send its request.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, RevertToSelf, TokenUser, WinLocalSystemSid, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_IMPERSONATE, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, ImpersonateNamedPipeClient, PeekNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenThreadToken, SetProcessWorkingSetSize, SetThreadToken};

use super::access::{self, Descriptors, Sd};
use super::profile::{self, Profile};
use super::proto::{self, Status};
use super::throttle::{self, Throttle};
use super::usn::{self, CatchUp, Polled};
use crate::fsys::{self, Row, SortKey};
use crate::index::{self, Build, Everything, Freshness, Hooks, Index, Live, Matcher, Visibility};
use crate::memory::Level;
use crate::wire::{self, put_bytes, put_u32, put_u64, Reader};

pub struct Config {
    pub state_dir: PathBuf,
    /// Drive roots to index (`C:\`, ...).
    pub roots: Vec<String>,
    /// CPU share reserved under full load, overriding the profile's.
    pub reserve: Option<f64>,
    /// Available memory (fraction) below which the service gives memory back.
    pub memory_floor: f64,
    pub save_every: Duration,
}

const MAX_CLIENTS: usize = 32;
const CHUNK_ROWS: usize = 16 * 1024;
const MAX_FIRST: usize = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

static STOP: AtomicBool = AtomicBool::new(false);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Where the service logs: `%ProgramData%\FEx\service.log`, readable by
/// users (it holds timings and drive roots, no file names), unlike the
/// index folder beside it.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.parent().unwrap_or(state_dir).join("service.log")
}

pub fn log(msg: &str) {
    eprintln!("fex-service: {msg}");
    // Under the SCM there's no console: keep a log file.
    if let Some(path) = LOG_FILE.get() {
        let secs = now_ns() / 1_000_000_000;
        let line = format!("{secs} {msg}\r\n");
        // Keep it bounded: one previous generation.
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 1 << 20) {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
}

static LOG_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Ask the running service to stop (from the SCM's control handler).
/// Crawls in progress, so stopping can cancel them.
static CRAWLS: Mutex<Vec<std::sync::Weak<Build>>> = Mutex::new(Vec::new());

pub fn request_stop() {
    STOP.store(true, Relaxed);
    for b in CRAWLS.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
        if let Some(b) = b.upgrade() {
            b.cancel();
        }
    }
    // Wake the accept loop, which is blocked waiting for a client.
    let name = wide(proto::PIPE);
    let h = unsafe { CreateFileW(name.as_ptr(), 0, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(h) };
    }
}

/// Security attributes from an SDDL string, freed on drop.
struct Sddl {
    sa: SECURITY_ATTRIBUTES,
}

impl Sddl {
    fn new(sddl: &str) -> Option<Sddl> {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) };
        (ok != 0).then(|| Sddl { sa: SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 } })
    }
}

impl Drop for Sddl {
    fn drop(&mut self) {
        unsafe { LocalFree(self.sa.lpSecurityDescriptor) };
    }
}

/// The state folder holds every name on the machine: SYSTEM and
/// Administrators only (protected from inheriting anything looser).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sddl = Sddl::new("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)").ok_or_else(std::io::Error::last_os_error)?;
    let path: Vec<u16> = wide(&dir.to_string_lossy());
    if unsafe { CreateDirectoryW(path.as_ptr(), &sddl.sa) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

struct Drive {
    root: String,
    /// The descriptor cache follows this drive's change journal, so it can
    /// be trusted across queries.
    tracked: AtomicBool,
    live: RwLock<Option<Arc<Live>>>,
    /// The watcher lost changes: crawl again.
    stale: Arc<AtomicBool>,
    /// The index is current to the disk (crawled, or fully caught up from
    /// the journal). Until then it isn't saved: the previous saved index and
    /// position stay, so the next start can resume the catch-up.
    caught_up: AtomicBool,
    last_save: Mutex<Instant>,
}

struct Shared {
    drives: Vec<Drive>,
    /// The service's own folder (state and log): its changes don't count.
    own: Arc<[String]>,
    /// Folder security descriptors, shared by all queries.
    sds: Descriptors,
    /// Per client process, the number of its latest query: older ones stop.
    clients: Mutex<HashMap<u32, Arc<AtomicU64>>>,
    throttle: Arc<Throttle>,
    activity: Mutex<&'static str>,
    verified_ns: AtomicU64,
    dir: PathBuf,
}

impl Shared {
    fn set_activity(&self, a: &'static str) {
        *self.activity.lock().unwrap_or_else(PoisonError::into_inner) = a;
    }
}

/// Run the service until `request_stop`. `ready` runs once the pipe is up.
pub fn run(cfg: Config, ready: impl FnOnce()) {
    let log_file = log_path(&cfg.state_dir);
    if let Some(dir) = log_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = LOG_FILE.set(log_file);
    log(&format!("starting FEx {}", env!("CARGO_PKG_VERSION")));
    if let Err(e) = create_private_dir(&cfg.state_dir) {
        log(&format!("cannot create {}: {e}", cfg.state_dir.display()));
        return;
    }
    let profile_path = cfg.state_dir.join("profile");
    let profile = match Profile::load(&profile_path) {
        Some(p) if p.cores == crate::walk::cores() => p,
        _ => {
            let p = profile::measure(&cfg.roots, None);
            let _ = p.save(&profile_path);
            p
        }
    };
    log(&format!("profile: {}", profile.describe()));
    throttle::lower_memory_priority();
    let sh = Arc::new(Shared {
        drives: cfg
            .roots
            .iter()
            .map(|r| Drive { root: r.clone(), tracked: AtomicBool::new(false), caught_up: AtomicBool::new(false), live: RwLock::new(None), stale: Arc::new(AtomicBool::new(false)), last_save: Mutex::new(Instant::now()) })
            .collect(),
        throttle: Throttle::start(cfg.reserve.unwrap_or(profile.reserve), profile.cores, cfg.memory_floor),
        sds: Descriptors::default(),
        clients: Mutex::new(HashMap::new()),
        own: Arc::from([cfg.state_dir.parent().unwrap_or(&cfg.state_dir).to_string_lossy().into_owned()]),
        activity: Mutex::new("starting"),
        verified_ns: AtomicU64::new(0),
        dir: cfg.state_dir.clone(),
    });
    log(&format!("indexing {}", cfg.roots.join(" ")));
    {
        let sh = sh.clone();
        thread::Builder::new().name("fex-permissions".into()).spawn(move || follow_permissions(&sh)).ok();
    }
    {
        let sh = sh.clone();
        thread::Builder::new().name("fex-memory".into()).spawn(move || watch_memory(&sh)).ok();
    }
    let maint = {
        let sh = sh.clone();
        let save_every = cfg.save_every;
        let user_reserve = cfg.reserve.is_some();
        thread::Builder::new().name("fex-maintain".into()).spawn(move || maintain(&sh, save_every, user_reserve)).ok()
    };
    serve_pipe(&sh, ready);
    // Background threads paused on battery must still see the stop.
    sh.throttle.stop();
    if let Some(m) = maint {
        let _ = m.join();
    }
    for d in &sh.drives {
        save_drive(&sh, d, true);
    }
    log("stopped");
}

fn saved_paths(dir: &Path, root: &str) -> (PathBuf, PathBuf) {
    let idx = Index::saved_path(dir, root);
    let pos = idx.with_extension("usn");
    (idx, pos)
}

fn on_change(stale: Arc<AtomicBool>) -> impl Fn(Freshness) + Send + 'static {
    move |f| {
        if matches!(f, Freshness::Stale) {
            stale.store(true, Relaxed);
        }
    }
}

fn hooks(sh: &Arc<Shared>) -> Hooks {
    let t = sh.throttle.clone();
    Hooks { init: Box::new(throttle::background_thread), pace: Box::new(move || t.wait()), ignore: sh.own.clone() }
}

/// Crawl a drive in the background and swap the result in.
fn crawl(sh: &Arc<Shared>, d: &Drive) -> Option<usize> {
    let t = Instant::now();
    if STOP.load(Relaxed) {
        return None;
    }
    let build = Build::with_hooks(&d.root, hooks(sh));
    {
        let mut crawls = CRAWLS.lock().unwrap_or_else(PoisonError::into_inner);
        crawls.retain(|b| b.strong_count() > 0);
        crawls.push(Arc::downgrade(&build));
    }
    // Stop may have come between the check above and registering.
    if STOP.load(Relaxed) {
        return None;
    }
    let idx = build.run(&|_, _| {})?;
    let entries = idx.entries;
    log(&format!("crawled {}: {entries} entries in {:.1} s ({} unreadable)", d.root, t.elapsed().as_secs_f64(), idx.errors));
    d.stale.store(false, Relaxed);
    d.caught_up.store(true, Relaxed);
    let live = Live::new(idx, build.take_freshener(), on_change(d.stale.clone()));
    *d.live.write().unwrap_or_else(PoisonError::into_inner) = Some(live);
    save_drive(sh, d, false);
    Some(entries)
}

/// Save a drive's index with the journal position it is current to
/// (`only_dirty`: unless nothing changed since the last save). An index
/// whose watcher lost changes is not saved, and its old journal position is
/// dropped: the next start must crawl rather than trust it.
fn save_drive(sh: &Shared, d: &Drive, only_dirty: bool) {
    let Some(live) = d.live.read().unwrap_or_else(PoisonError::into_inner).clone() else { return };
    if !d.caught_up.load(Relaxed) {
        return; // keep the previous save (and position) to resume from
    }
    if d.stale.load(Relaxed) {
        let _ = std::fs::remove_file(saved_paths(&sh.dir, &d.root).1);
        log(&format!("{}: not saved (changes were lost); it will be crawled at the next start", d.root));
        return;
    }
    if only_dirty && !index::read(&live.index).dirty {
        return;
    }
    // The saved position must not run ahead of the index: take it first,
    // let the watcher receive everything logged before it (it reads every
    // 500 ms), then wait until it has applied all of it. Replaying a little
    // twice at the next start is harmless; skipping a change isn't.
    let pos = usn::position(&d.root);
    thread::sleep(Duration::from_millis(1500));
    let stopping = STOP.load(Relaxed);
    let deadline = Instant::now() + if stopping { Duration::from_secs(10) } else { Duration::from_secs(60) };
    let mut settled = live.settled();
    while !settled && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        settled = live.settled();
    }
    let (idx_path, pos_path) = saved_paths(&sh.dir, &d.root);
    let pos = match (settled, stopping) {
        (true, _) => pos,
        // Still busy: try again next round.
        (false, false) => {
            log(&format!("{}: still applying changes; saving later", d.root));
            *d.last_save.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
            return;
        }
        // Stopping while busy: save the index (it answers at once next
        // time) but not the position, so the next start crawls to be sure.
        (false, true) => {
            log(&format!("{}: saved while still applying changes; it will be crawled at the next start", d.root));
            None
        }
    };
    index::write(&live.index).dirty = false;
    // Serialized under the read lock (memory only); written and synced after.
    let bytes = index::read(&live.index).serialize();
    let saved = Index::write_saved(&sh.dir, &live.root, &bytes);
    drop(bytes);
    match saved {
        Ok(_) => {
            match pos {
                Some(p) => {
                    let _ = usn::save(&pos_path, p);
                }
                None => {
                    let _ = std::fs::remove_file(&pos_path);
                }
            }
            *d.last_save.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        }
        Err(e) => {
            index::write(&live.index).dirty = true;
            log(&format!("saving {}: {e}", idx_path.display()));
        }
    }
}

/// Load a drive's saved index and catch it up from the change journal.
/// Returns whether the drive still needs a crawl.
fn load_drive(sh: &Arc<Shared>, d: &Drive) -> bool {
    // Watch before loading, so nothing in between is missed.
    let fresh = index::Freshener::start_ignoring(d.root.as_str().into(), sh.own.clone());
    let (idx_path, pos_path) = saved_paths(&sh.dir, &d.root);
    let t = Instant::now();
    let loaded = Index::load(&idx_path).filter(|i| index::in_scope(&d.root, &i.root) && index::in_scope(&i.root, &d.root));
    let Some(idx) = loaded else { return true };
    log(&format!("loaded {}: {} entries in {:.0} ms", d.root, idx.entries, t.elapsed().as_secs_f64() * 1e3));
    let live = Live::new(idx, Some(fresh), on_change(d.stale.clone()));
    *d.live.write().unwrap_or_else(PoisonError::into_inner) = Some(live.clone());
    let t = Instant::now();
    match usn::load(&pos_path).map(|p| usn::changed_since(&d.root, p)) {
        Some(CatchUp::Dirs(dirs)) => {
            let n = dirs.len();
            let mut read = 0;
            // In batches, so a stop request isn't held up by a long catch-up.
            for batch in dirs.chunks(1024) {
                if STOP.load(Relaxed) {
                    log(&format!("{}: catch-up interrupted; it resumes at the next start", d.root));
                    return false;
                }
                read += index::refresh_dirs(&live.index, batch.to_vec());
            }
            d.caught_up.store(true, Relaxed);
            log(&format!("caught up {} from the change journal: {n} folders changed, {read} re-read in {:.0} ms", d.root, t.elapsed().as_secs_f64() * 1e3));
            false
        }
        Some(CatchUp::Lost) | None => {
            log(&format!("{}: changes since the last save can't be replayed; crawling in the background", d.root));
            true
        }
    }
}

fn maintain(sh: &Arc<Shared>, save_every: Duration, user_reserve: bool) {
    // Every drive's saved index first, all at once: a drive that needs a
    // long crawl mustn't keep the others offline.
    sh.set_activity("loading");
    let needs: Vec<bool> = thread::scope(|sc| {
        let handles: Vec<_> = sh.drives.iter().map(|d| sc.spawn(move || load_drive(sh, d))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(true)).collect()
    });
    // Then the crawls, one at a time (each uses every core it may).
    let mut crawled = 0u64;
    for (d, &need) in sh.drives.iter().zip(&needs) {
        if STOP.load(Relaxed) {
            return;
        }
        if need {
            sh.set_activity("crawling");
            crawled += crawl(sh, d).unwrap_or(0) as u64;
        }
    }
    sh.verified_ns.store(now_ns(), Relaxed);
    prefetch_descriptors(sh);
    sh.set_activity("idle");
    // The profile estimated the file count; a full crawl knows it.
    if crawled > 0 {
        let path = sh.dir.join("profile");
        if let Some(p) = Profile::load(&path).filter(|p| crawled.abs_diff(p.files) * 5 > p.files) {
            let p = p.with_files(crawled);
            let _ = p.save(&path);
            if !user_reserve {
                sh.throttle.set_reserve(p.reserve);
            }
            log(&format!("profile updated with the real file count: reserve {:.2}%", p.reserve * 100.0));
        }
    }
    while !STOP.load(Relaxed) {
        thread::sleep(Duration::from_secs(1));
        // Refill a cleared cache only once memory is normal (a shed under
        // critical memory clears it; queries meanwhile read what they need).
        if sh.throttle.memory_level() == Level::Normal && sh.sds.take_cleared() {
            prefetch_descriptors(sh);
            sh.set_activity("idle");
        }
        for d in &sh.drives {
            if STOP.load(Relaxed) {
                break;
            }
            if d.stale.load(Relaxed) {
                log(&format!("{}: the watcher lost changes; crawling again", d.root));
                sh.set_activity("crawling");
                crawl(sh, d);
                sh.verified_ns.store(now_ns(), Relaxed);
                sh.set_activity("idle");
                continue;
            }
            let live = d.live.read().unwrap_or_else(PoisonError::into_inner).clone();
            let dirty = live.as_ref().is_some_and(|l| index::try_read(&l.index).is_some_and(|i| i.dirty));
            if dirty && d.last_save.lock().unwrap_or_else(PoisonError::into_inner).elapsed() >= save_every {
                save_drive(sh, d, true);
            }
        }
    }
}

/// Shed memory when the level rises, and again every 30 s while it stays up
/// (what was freed may have been refilled since).
fn watch_memory(sh: &Shared) {
    let mut was = Level::Normal;
    let mut last = Instant::now();
    while !STOP.load(Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let level = sh.throttle.memory_level();
        if level > was || (level != Level::Normal && last.elapsed() >= Duration::from_secs(30)) {
            shed(sh, level);
            last = Instant::now();
        }
        was = level;
    }
}

/// Give memory back (background work already pauses while it's tight).
/// Tight: compact the heap (free blocks decommitted) and empty the working
/// set; pages stay on the standby list, so a query soon after takes soft
/// faults, not disk reads, unless something else needed the memory.
/// Critical: also drop the descriptor cache.
fn shed(sh: &Shared, level: Level) {
    let before = process_memory();
    let mut dropped = 0;
    if level == Level::Critical {
        dropped = sh.sds.folders();
        sh.sds.clear();
    }
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
    let after = process_memory();
    let mb = |b: u64| b as f64 / 1048576.0;
    log(&format!(
        "memory {} ({:.0}% available): working set {:.1} -> {:.1} MB, private {:.1} -> {:.1} MB{}",
        level.name(),
        sh.throttle.memory_available() * 100.0,
        mb(before.0),
        mb(after.0),
        mb(before.1),
        mb(after.1),
        if dropped > 0 { format!("; dropped {dropped} cached folder permissions") } else { String::new() }
    ));
}

/// (working set, private bytes) of this process.
fn process_memory() -> (u64, u64) {
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) } == 0 {
        return (0, 0);
    }
    (c.WorkingSetSize as u64, c.PrivateUsage as u64)
}

fn memory_state(sh: &Shared) -> String {
    let (ws, private) = process_memory();
    let mb = |b: u64| b as f64 / 1048576.0;
    format!(
        "{:.1} MB working set, {:.1} MB private, {} cached folder permissions · {} ({:.0}% of memory available)",
        mb(ws),
        mb(private),
        sh.sds.folders(),
        sh.throttle.memory_level().name(),
        sh.throttle.memory_available() * 100.0
    )
}

/// Read every indexed folder's security descriptor ahead of the first
/// query, on background threads.
fn prefetch_descriptors(sh: &Arc<Shared>) {
    sh.set_activity("reading permissions");
    let t = Instant::now();
    for d in &sh.drives {
        if !d.tracked.load(Relaxed) {
            continue;
        }
        let Some(live) = d.live.read().unwrap_or_else(PoisonError::into_inner).clone() else { continue };
        let folders = index::read(&live.index).folder_paths();
        let next = AtomicUsize::new(0);
        thread::scope(|sc| {
            for _ in 0..crate::walk::cores() {
                sc.spawn(|| {
                    throttle::background_thread();
                    while let Some(f) = folders.get(next.fetch_add(1, Relaxed)) {
                        if STOP.load(Relaxed) {
                            return;
                        }
                        sh.throttle.wait();
                        let _ = sh.sds.get(f);
                    }
                });
            }
        });
    }
    log(&format!("read permissions of {} folders ({} distinct) in {:.1} s", sh.sds.folders(), sh.sds.distinct(), t.elapsed().as_secs_f64()));
}

/// Keep the descriptor cache true to the disk: follow each drive's change
/// journal every second (folder records only). A folder created drops its
/// entry (its path may have named another folder); a folder deleted,
/// renamed (old or new path) or given a new ACL drops its whole subtree's
/// (subfolders move with it without records of their own). Only records
/// that can't be placed, or a lost journal, clear the cache, which is then
/// read again in the background.
fn follow_permissions(sh: &Arc<Shared>) {
    const SECURITY: u32 = 0x800;
    const CREATE: u32 = 0x100;
    const MOVED: u32 = 0x200 | 0x1000 | 0x2000; // delete, rename old/new name
    const NAMES: u32 = CREATE | MOVED;
    let mut tails: Vec<Option<usn::Tail>> = sh.drives.iter().map(|d| usn::Tail::start(&d.root)).collect();
    for (d, t) in sh.drives.iter().zip(&tails) {
        d.tracked.store(t.is_some(), Relaxed);
        if t.is_none() {
            log(&format!("{}: no change journal; permissions are checked folder by folder", d.root));
        }
    }
    while !STOP.load(Relaxed) {
        thread::sleep(Duration::from_secs(1));
        for tail in tails.iter_mut().flatten() {
            match tail.poll(SECURITY | NAMES, true) {
                Polled::Lost => sh.sds.clear(),
                Polled::Records(records) => {
                    let dirs = records;
                    let mut trees = Vec::new();
                    let mut created = Vec::new();
                    let mut unresolved = false;
                    for (r, parent) in &dirs {
                        let Some(p) = parent else {
                            // The parent is gone too; its own record covers it,
                            // but a rename into a vanished folder can't be placed.
                            unresolved |= r.reason & MOVED != 0;
                            continue;
                        };
                        let path = fsys::join(p, &r.name);
                        // A folder's own ACL change: its descriptor and its
                        // subtree's (inherited entries are rewritten, each
                        // with a record of its own, but forgetting the subtree
                        // costs no more than a range).
                        if r.reason & (MOVED | SECURITY) != 0 {
                            trees.push(path);
                        } else {
                            created.push(path);
                        }
                    }
                    if unresolved {
                        sh.sds.clear();
                    } else {
                        if !trees.is_empty() {
                            sh.sds.forget_trees(&trees);
                        }
                        for path in created {
                            sh.sds.forget(&path);
                        }
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------ pipe

fn serve_pipe(sh: &Arc<Shared>, ready: impl FnOnce()) {
    // SYSTEM and Administrators: full control; authenticated users: read and write.
    let Some(sddl) = Sddl::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)") else {
        log("cannot build the pipe's security descriptor");
        return;
    };
    let name = wide(proto::PIPE);
    let active = Arc::new(AtomicUsize::new(0));
    let mut first = true;
    let mut ready = Some(ready);
    while !STOP.load(Relaxed) {
        let flags = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                256 * 1024,
                64 * 1024,
                0,
                &sddl.sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            log(&format!("cannot create {}: {}", proto::PIPE, std::io::Error::last_os_error()));
            if first {
                // Someone else owns the name: give up, and let `run` finish
                // (maintenance stops too) instead of hanging in start-up.
                log("another process holds the index pipe; stopping");
                request_stop();
                return;
            }
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        if first {
            first = false;
            if let Some(r) = ready.take() {
                r();
            }
        }
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        // SAFETY: our pipe instance; File closes it.
        let pipe = unsafe { File::from_raw_handle(h) };
        if !connected || STOP.load(Relaxed) || active.load(Relaxed) >= MAX_CLIENTS {
            unsafe { DisconnectNamedPipe(h) };
            continue;
        }
        active.fetch_add(1, Relaxed);
        let (sh, counter) = (sh.clone(), active.clone());
        let spawned = thread::Builder::new().name("fex-client".into()).spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(&sh, &pipe)));
            unsafe {
                FlushFileBuffers(pipe.as_raw_handle());
                DisconnectNamedPipe(pipe.as_raw_handle());
            }
            counter.fetch_sub(1, Relaxed);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Relaxed);
        }
    }
}

/// Wait (polling, so a silent client can't hold a thread forever) until
/// `n` bytes are available, then read them.
fn read_exact_within(pipe: &File, n: usize, deadline: Instant) -> std::io::Result<Vec<u8>> {
    loop {
        let mut avail = 0u32;
        let ok = unsafe { PeekNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if avail as usize >= n {
            let mut buf = vec![0u8; n];
            (&*pipe).read_exact(&mut buf)?;
            return Ok(buf);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "client too slow"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn read_request(pipe: &File) -> std::io::Result<Vec<u8>> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let len = u32::from_le_bytes(read_exact_within(pipe, 4, deadline)?.try_into().unwrap()) as usize;
    if len > proto::MAX_REQUEST {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "request too large"));
    }
    read_exact_within(pipe, len, deadline)
}

/// An access token, closed on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a token handle may be used from any thread.
unsafe impl Send for Token {}
unsafe impl Sync for Token {}

/// The pipe client's token: impersonate just long enough to copy it.
fn client_token(pipe: &File) -> Option<Token> {
    unsafe {
        if ImpersonateNamedPipeClient(pipe.as_raw_handle()) == 0 {
            return None;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let ok = OpenThreadToken(GetCurrentThread(), TOKEN_IMPERSONATE | TOKEN_QUERY, 1, &mut tok);
        if RevertToSelf() == 0 {
            // Never carry on as the client.
            std::process::abort();
        }
        (ok != 0).then_some(Token(tok))
    }
}

fn is_system(t: &Token) -> bool {
    let mut buf = vec![0u64; 64];
    let mut len = 0u32;
    let ok = unsafe { GetTokenInformation(t.0, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) };
    // SAFETY: on success the buffer starts with a TOKEN_USER.
    ok != 0 && unsafe { IsWellKnownSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, WinLocalSystemSid) != 0 }
}

thread_local! {
    /// The token this thread is checking access with, if any.
    static ACTING_AS: Cell<usize> = const { Cell::new(0) };
}

struct Revert;

impl Drop for Revert {
    fn drop(&mut self) {
        if unsafe { SetThreadToken(std::ptr::null(), std::ptr::null_mut()) } == 0 {
            std::process::abort();
        }
        ACTING_AS.with(|a| a.set(0));
    }
}

/// The fast check: `AccessCheck` against cached descriptors.
struct AccessVisibility<'a> {
    token: &'a Token,
    sds: &'a Descriptors,
    /// Per folder: 0 unknown, 1 visible, 2 hidden.
    cache: Vec<AtomicU8>,
    /// Per distinct descriptor (keyed by the object itself, which the map
    /// keeps alive, so a cache clear mid-query can't alias two of them).
    decided: Mutex<HashMap<usize, (Arc<Sd>, bool)>>,
}

impl Visibility for AccessVisibility<'_> {
    fn visible(&self, dir: usize, path: &str) -> bool {
        let Some(slot) = self.cache.get(dir) else { return false };
        match slot.load(Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        let ok = match self.sds.get(path) {
            None => false,
            Some(sd) => {
                let k = Arc::as_ptr(&sd) as usize;
                let known = self.decided.lock().unwrap_or_else(PoisonError::into_inner).get(&k).map(|e| e.1);
                known.unwrap_or_else(|| {
                    let ok = access::may_list(&sd, self.token.0);
                    self.decided.lock().unwrap_or_else(PoisonError::into_inner).insert(k, (sd, ok));
                    ok
                })
            }
        };
        slot.store(if ok { 1 } else { 2 }, Relaxed);
        ok
    }
}

/// The exact check: take on the token and open the folder.
struct TokenVisibility<'a> {
    token: &'a Token,
    /// Per folder: 0 unknown, 1 visible, 2 hidden.
    cache: Vec<AtomicU8>,
}

impl Visibility for TokenVisibility<'_> {
    fn enter(&self) -> Option<Box<dyn std::any::Any>> {
        let ok = unsafe { SetThreadToken(std::ptr::null(), self.token.0) } != 0;
        ACTING_AS.with(|a| a.set(if ok { self.token.0 as usize } else { 0 }));
        Some(Box::new(Revert))
    }

    fn visible(&self, dir: usize, path: &str) -> bool {
        if ACTING_AS.with(Cell::get) != self.token.0 as usize {
            return false;
        }
        let Some(slot) = self.cache.get(dir) else { return false };
        match slot.load(Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        // Listing the folder is exactly the right to check. The backup flag
        // is needed to open folders; the client's privileges aren't enabled,
        // so it bypasses nothing.
        let p = crate::win::wide(path, "");
        let h = unsafe {
            CreateFileW(p.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut())
        };
        let ok = h != INVALID_HANDLE_VALUE;
        if ok {
            unsafe { CloseHandle(h) };
        }
        slot.store(if ok { 1 } else { 2 }, Relaxed);
        ok
    }
}

fn serve(sh: &Shared, pipe: &File) {
    let Ok(req) = read_request(pipe) else { return };
    let mut w = pipe;
    let reply = |w: &mut &File, status: u8, msg: &str| {
        let mut out = vec![status];
        put_bytes(&mut out, msg.as_bytes());
        let _ = wire::send(w, &out);
    };
    let mut r = Reader::new(&req);
    let protocol = r.u8().ok();
    if protocol != Some(wire::PROTOCOL) {
        return reply(&mut w, proto::ERROR, &wire::protocol_mismatch(protocol));
    }
    match r.u8() {
        Ok(proto::QUERY) => {
            let parsed = (|| Ok::<_, std::io::Error>((r.u8()? != 0, r.u32()?, String::from_utf8_lossy(r.bytes()?).into_owned(), String::from_utf8_lossy(r.bytes()?).into_owned())))();
            let Ok((regex, limit, scope, text)) = parsed else { return reply(&mut w, proto::ERROR, "malformed request") };
            let order = r.u8().unwrap_or(proto::NAME_ASC);
            let first = r.u32().unwrap_or(u32::MAX) as usize;
            let seq = r.u64().unwrap_or(0);
            if text.len() > 4096 {
                return reply(&mut w, proto::ERROR, "queries are limited to 4096 bytes");
            }
            let Some(drive) = sh.drives.iter().find(|d| index::in_scope(&scope, &d.root)) else {
                let roots: Vec<&str> = sh.drives.iter().map(|d| d.root.as_str()).collect();
                return reply(&mut w, proto::NOT_COVERED, &roots.join("\n"));
            };
            let Some(live) = drive.live.read().unwrap_or_else(PoisonError::into_inner).clone() else {
                return reply(&mut w, proto::BUSY, "this drive's first index is still being built");
            };
            let m = match Matcher::new(&text, regex) {
                Ok(Some(m)) => m,
                Ok(None) => return send_rows(&mut w, Vec::new(), 0, order, first),
                Err(e) => return reply(&mut w, proto::ERROR, &e),
            };
            let Some(token) = client_token(pipe) else { return reply(&mut w, proto::ERROR, "could not identify the client") };
            let limit = limit.min(proto::MAX_LIMIT) as usize;
            let mut pid = 0u32;
            unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) };
            // Search as you type: a newer query from the same client (by the
            // client's own numbering, not arrival order) makes this one
            // obsolete. 0 means unnumbered: never superseded.
            let latest = {
                let mut clients = sh.clients.lock().unwrap_or_else(PoisonError::into_inner);
                if clients.len() > 256 {
                    clients.retain(|_, c| Arc::strong_count(c) > 1);
                }
                clients.entry(pid).or_default().clone()
            };
            if seq != 0 {
                latest.fetch_max(seq, Relaxed);
            }
            let cancelled = || seq != 0 && latest.load(Relaxed) > seq;
            let found = {
                let idx = index::read(&live.index);
                if is_system(&token) {
                    idx.query_as(&m, &scope, limit, &cancelled, &Everything)
                } else if drive.tracked.load(Relaxed) && access::bypasses_traverse(token.0) {
                    let vis = AccessVisibility { token: &token, sds: &sh.sds, cache: (0..idx.dir_slots()).map(|_| AtomicU8::new(0)).collect(), decided: Mutex::new(HashMap::new()) };
                    idx.query_as(&m, &scope, limit, &cancelled, &vis)
                } else {
                    let vis = TokenVisibility { token: &token, cache: (0..idx.dir_slots()).map(|_| AtomicU8::new(0)).collect() };
                    idx.query_as(&m, &scope, limit, &cancelled, &vis)
                }
            };
            // Superseded: the client has moved on.
            let Some((rows, total)) = found else { return };
            send_rows(&mut w, rows, total, order, first);
        }
        Ok(proto::STATUS) => {
            let mut st = Status {
                roots: sh.drives.iter().map(|d| d.root.clone()).collect(),
                verified_ns: sh.verified_ns.load(Relaxed),
                activity: sh.activity.lock().unwrap_or_else(PoisonError::into_inner).to_string(),
                paused: sh.throttle.paused().unwrap_or_default(),
                reserve: sh.throttle.reserve_state(),
                memory: memory_state(sh),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            };
            for d in &sh.drives {
                if let Some(live) = d.live.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    st.ready.push(d.root.clone());
                    if let Some(i) = index::try_read(&live.index) {
                        st.entries += i.entries as u64;
                    }
                }
            }
            let _ = wire::send(&mut w, &proto::encode_status(&st));
        }
        _ => reply(&mut w, proto::ERROR, "unknown request"),
    }
}

/// Stream rows: the `first` that sort first in `order` (selected in linear
/// time, then sorted), then the rest in chunks; folders sent once each.
fn send_rows(w: &mut &File, mut rows: Vec<Row>, total: usize, order: u8, first: usize) {
    let desc = order == proto::NAME_DESC;
    let cmp = |a: &Row, b: &Row| fsys::cmp_rows(a, b, SortKey::Name, desc);
    let first = first.clamp(1, MAX_FIRST).min(rows.len());
    if order != proto::UNORDERED && first > 0 {
        if first < rows.len() {
            rows.select_nth_unstable_by(first - 1, cmp);
        }
        rows[..first].sort_unstable_by(cmp);
    }
    let mut dirs: HashMap<Arc<str>, u32> = HashMap::new();
    let mut header = Some(total as u64);
    let mut at = 0;
    loop {
        let end = if at == 0 { first } else { (at + CHUNK_ROWS).min(rows.len()) };
        let last = end >= rows.len();
        let mut body = Vec::with_capacity((end - at) * 40 + 16);
        if let Some(t) = header.take() {
            body.push(proto::OK);
            put_u64(&mut body, t);
        }
        body.push(last as u8);
        let mut new_dirs: Vec<Arc<str>> = Vec::new();
        let mut encoded = Vec::with_capacity((end - at) * 36);
        for row in &rows[at..end] {
            let dir = row.dir.clone().unwrap_or_else(|| Arc::from(""));
            let next = dirs.len() as u32;
            let id = *dirs.entry(dir.clone()).or_insert_with(|| {
                new_dirs.push(dir);
                next
            });
            put_u32(&mut encoded, id);
            put_u32(&mut encoded, row.attrs);
            put_u64(&mut encoded, row.size);
            put_u64(&mut encoded, row.mtime);
            put_bytes(&mut encoded, row.name.as_bytes());
        }
        put_u32(&mut body, new_dirs.len() as u32);
        for d in &new_dirs {
            put_bytes(&mut body, d.as_bytes());
        }
        put_u32(&mut body, (end - at) as u32);
        body.extend_from_slice(&encoded);
        if wire::send(w, &body).is_err() || last {
            return;
        }
        at = end;
    }
}
