//! `fex daemon`: a system service that keeps the name index ready.
//!
//! * Starts by mapping its last snapshot, so it answers queries at once,
//!   then verifies it (a ctime sweep) and re-reads only what changed.
//! * Stays fresh with fanotify filesystem marks where the kernel allows
//!   them, plus a periodic sweep as a safety net (and for network mounts).
//! * Background work (crawling, sweeping) runs on idle-priority threads and
//!   pauses under others' I/O or memory pressure (`throttle.rs`). Queries and
//!   index updates run at normal priority, so a starved background thread
//!   can never hold up an answer.
//! * Each client sees only names in folders it could list itself. Identity
//!   comes from the kernel (SO_PEERCRED / SO_PEERGROUPS), never from the
//!   request. Workers take on the client's filesystem identity (fsuid,
//!   fsgid, groups: per thread on Linux) and `open()` each folder with
//!   matches: the kernel's own permission check, so ACLs and search
//!   permission on every ancestor count exactly. Decisions are cached per
//!   query (one check per folder with matches, not per match), and filtered
//!   matches are left out of the count too. Anything uncertain is hidden.
//!
//! Not covered: SELinux/AppArmor policy is checked in the daemon's domain,
//! not the client's; a folder made private after the query reaches the
//! index is hidden from the next query on.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::fanotify::{Events, Fanotify};
use crate::memory;
use super::profile::{self, Profile};
use super::index::{self, Crawl, Everything, Index, Matcher, Visibility};
use super::proto::{self, Reader};
use super::throttle::{self, Throttle};
use super::{fsys, sys};

pub struct Config {
    pub root: Vec<u8>,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    /// Full verification interval (fanotify covers the time between).
    pub sweep_every: Duration,
    pub save_every: Duration,
    pub fanotify: bool,
    pub include_network: bool,
    /// Background crawler and sweeper threads (they only use idle CPU).
    pub workers: usize,
    /// Pause background work when others' I/O stall share exceeds this (%).
    pub io_limit: f64,
    /// CPU share reserved under full load, overriding the profile's.
    pub reserve: Option<f64>,
    /// Give memory back when less than this fraction of it is available.
    pub memory_floor: f64,
}

/// Concurrent client connections served; more wait in the listen queue.
const MAX_CLIENTS: usize = 32;
/// Concurrent connections from one user, so no one user can take every
/// slot and starve the others (their clients then search by themselves).
const MAX_PER_UID: usize = 8;

static STOP: AtomicBool = AtomicBool::new(false);
/// SIGUSR1: give back all the memory the daemon can, now.
static SHED_NOW: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    SHED_NOW.store(true, Relaxed);
}

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Relaxed);
}

struct Shared {
    root: Vec<u8>,
    index: RwLock<Option<Index>>,
    activity: Mutex<&'static str>,
    verified_ns: AtomicU64,
    watched: AtomicU32,
    throttle: Arc<Throttle>,
    /// `throttle.wait` as a closure, for crawls' `pace` hook.
    pace: Box<dyn Fn() + Send + Sync>,
    /// Running as root: can filter per user. Otherwise serves only its own uid.
    privileged: bool,
    dirty: AtomicBool,
    /// The memory thread asks the maintenance thread to fold heap-held
    /// changes into the snapshot.
    compact_requested: AtomicBool,
    /// Folders left out of the index (`skips`): queries inside them are not
    /// covered, so clients search them themselves.
    skip: RwLock<HashSet<Vec<u8>>>,
    /// Per client process, the number of its latest query: a newer query
    /// (search as you type) makes older ones stop.
    clients: Mutex<HashMap<i32, Arc<AtomicU64>>>,
    /// Bumped (under the write lock) whenever the index is replaced: folder
    /// ids taken from an older index mean nothing in the new one.
    epoch: AtomicU64,
    /// Permission bits recorded at indexing time are kept current (fanotify
    /// reports attribute changes on every indexed filesystem), so queries
    /// may skip the live check for folders everyone can list.
    perms_live: AtomicBool,
    /// Open connections per uid (see `MAX_PER_UID`).
    per_uid: Mutex<HashMap<u32, usize>>,
    /// Queries answered since the heap was last trimmed.
    queries: AtomicU64,
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn log(msg: &str) {
    // stderr goes to the journal under systemd.
    eprintln!("fex-indexd: {msg}");
}

impl Shared {
    fn set_activity(&self, a: &'static str) {
        *self.activity.lock().unwrap_or_else(PoisonError::into_inner) = a;
    }

    /// Put `idx` in place of the current index (new epoch).
    fn replace_index(&self, idx: Option<Index>) {
        let mut g = self.index.write().unwrap_or_else(PoisonError::into_inner);
        *g = idx;
        self.epoch.fetch_add(1, Relaxed);
    }
}

/// Filesystems that only report local changes, or none at all: not crawled
/// unless asked for, since crawling them loads a shared server.
fn is_network_fs(t: &str) -> bool {
    sys::is_network_fs(t)
}

/// Folders the index leaves out: pseudo filesystems, network mounts (unless
/// asked for), and the daemon's own state folder (its snapshot writes would
/// otherwise mark the index changed with every save).
fn skips(cfg: &Config) -> HashSet<Vec<u8>> {
    let mut skip = HashSet::new();
    for m in sys::mounts() {
        if sys::is_pseudo_fs(&m.fstype) || (!cfg.include_network && is_network_fs(&m.fstype)) {
            skip.insert(m.point);
        }
    }
    skip.remove(&cfg.root);
    skip.insert(cfg.state_dir.as_os_str().as_encoded_bytes().to_vec());
    skip
}

fn skipped(path: &[u8], skip: &HashSet<Vec<u8>>) -> bool {
    skip.iter().any(|s| fsys::in_scope(path, s))
}

/// Paths to mark with fanotify: the root's filesystem and every indexed
/// filesystem mounted below it.
fn watch_points(cfg: &Config, skip: &HashSet<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut points = vec![cfg.root.clone()];
    for m in sys::mounts() {
        if m.point != cfg.root && fsys::in_scope(&m.point, &cfg.root) && !skip.contains(&m.point) && !is_network_fs(&m.fstype) {
            points.push(m.point);
        }
    }
    points
}

pub fn run(cfg: Config) -> ExitCode {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGUSR1, on_usr1 as *const () as libc::sighandler_t);
    }
    let privileged = unsafe { libc::geteuid() } == 0;
    // Every client thread would otherwise get a malloc arena of its own and
    // keep what its queries freed: a long-running daemon's heap would grow
    // to many times its index.
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.state_dir) {
        log(&format!("cannot create {}: {e}", cfg.state_dir.display()));
        return ExitCode::FAILURE;
    }
    // The snapshot holds every name on the machine: owner-only.
    let _ = std::fs::set_permissions(&cfg.state_dir, std::fs::Permissions::from_mode(0o700));
    let snap = cfg.state_dir.join("index.snap");

    let t = Instant::now();
    let loaded = match Index::load(&snap) {
        Ok(i) if *i.root == *cfg.root => {
            log(&format!("mapped snapshot: {} entries, {} folders in {:.1} ms", i.entries, i.dirs, t.elapsed().as_secs_f64() * 1e3));
            Some(i)
        }
        Ok(_) => {
            log("snapshot is for another root; crawling");
            None
        }
        Err(e) if snap.exists() => {
            log(&format!("{e}; crawling"));
            None
        }
        Err(_) => None,
    };
    let verified = loaded.as_ref().map_or(0, |i| i.created_ns);
    let profile_path = cfg.state_dir.join("profile");
    let skip = skips(&cfg);
    let profile = match Profile::load(&profile_path) {
        Some(p) if p.cores == super::walk::cores() => p,
        _ => {
            let known = shared_entries(&snap);
            let p = profile::measure(&cfg.root, &skip, known);
            if let Err(e) = p.save(&profile_path) {
                log(&format!("saving {}: {e}", profile_path.display()));
            }
            p
        }
    };
    log(&format!("profile: {}", profile.describe().replace('\n', "; ")));
    let throttle = Throttle::start(cfg.io_limit, cfg.reserve.unwrap_or(profile.reserve), profile.cores, cfg.memory_floor);
    if !throttle.own_cgroup {
        log("not in a cgroup of its own: its own I/O stalls count as pressure (run it as a systemd service for accurate throttling)");
    }
    let shared = Arc::new(Shared {
        root: cfg.root.clone(),
        index: RwLock::new(loaded),
        activity: Mutex::new("starting"),
        verified_ns: AtomicU64::new(verified),
        watched: AtomicU32::new(0),
        pace: {
            let t = throttle.clone();
            Box::new(move || t.wait())
        },
        throttle,
        privileged,
        dirty: AtomicBool::new(false),
        skip: RwLock::new(skip.clone()),
        compact_requested: AtomicBool::new(false),
        clients: Mutex::new(HashMap::new()),
        epoch: AtomicU64::new(0),
        perms_live: AtomicBool::new(false),
        per_uid: Mutex::new(HashMap::new()),
        queries: AtomicU64::new(0),
    });

    if let Some(dir) = cfg.socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = match UnixListener::bind(&cfg.socket) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("cannot listen on {}: {e}", cfg.socket.display()));
            return ExitCode::FAILURE;
        }
    };
    // Anyone may connect; what they see is filtered per query.
    let _ = std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o666));
    // Which socket file is ours: a successor may replace it before we exit.
    let ours = std::fs::metadata(&cfg.socket).map(|m| {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    });
    log(&format!(
        "serving {} on {}{}",
        String::from_utf8_lossy(&cfg.root),
        cfg.socket.display(),
        if privileged { "" } else { " (unprivileged: own user only, no fanotify)" }
    ));
    sd_notify("READY=1");

    let cfg = Arc::new(cfg);
    {
        let sh = shared.clone();
        thread::Builder::new().name("fex-memory".into()).spawn(move || watch_memory(&sh)).ok();
    }
    let maint = {
        let (sh, cfg, snap) = (shared.clone(), cfg.clone(), snap.clone());
        thread::Builder::new().name("fex-maintain".into()).spawn(move || supervise(&sh, &cfg, &snap)).ok()
    };

    let active = Arc::new(AtomicUsize::new(0));
    while !STOP.load(Relaxed) {
        let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 500) } <= 0 {
            continue;
        }
        let Ok((stream, _)) = listener.accept() else { continue };
        if active.load(Relaxed) >= MAX_CLIENTS {
            continue; // dropping the connection; the client falls back
        }
        let Some(creds) = peer_creds(&stream) else { continue };
        {
            let mut per = shared.per_uid.lock().unwrap_or_else(PoisonError::into_inner);
            let n = per.entry(creds.uid).or_insert(0);
            if *n >= MAX_PER_UID {
                continue;
            }
            *n += 1;
        }
        active.fetch_add(1, Relaxed);
        let (sh, counter) = (shared.clone(), active.clone());
        let uid = creds.uid;
        let release = move |sh: &Shared| {
            let mut per = sh.per_uid.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(n) = per.get_mut(&uid) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    per.remove(&uid);
                }
            }
        };
        let spawned = thread::Builder::new().name("fex-client".into()).spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(&sh, stream, creds)));
            release(&sh);
            counter.fetch_sub(1, Relaxed);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Relaxed);
            release(&shared);
        }
    }
    log("stopping");
    // Background threads paused for pressure or battery must still see STOP.
    shared.throttle.stop();
    if let Some(m) = maint {
        let _ = m.join();
    }
    if shared.dirty.load(Relaxed) {
        save(&shared, &snap, false);
    }
    let still_ours = std::fs::metadata(&cfg.socket).is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        ours.as_ref().is_ok_and(|&(dev, ino)| (m.dev(), m.ino()) == (dev, ino))
    });
    if still_ours {
        let _ = std::fs::remove_file(&cfg.socket);
    }
    ExitCode::SUCCESS
}

/// Entries in an existing snapshot, if any (a better file count than inodes).
fn shared_entries(snap: &Path) -> Option<u64> {
    Index::load(snap).ok().map(|i| i.entries as u64)
}

/// Tell systemd (Type=notify) that the socket is ready.
fn sd_notify(msg: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    use std::os::unix::ffi::OsStrExt;
    let p = path.as_bytes();
    let _ = match p.strip_prefix(b"@") {
        Some(name) => {
            use std::os::linux::net::SocketAddrExt;
            std::os::unix::net::SocketAddr::from_abstract_name(name).and_then(|a| sock.send_to_addr(msg.as_bytes(), &a))
        }
        None => sock.send_to(msg.as_bytes(), Path::new(&path)),
    };
}

// ------------------------------------------------------------ maintenance

fn save(sh: &Shared, snap: &Path, remap: bool) {
    let t = Instant::now();
    // Streamed from the index under the read lock, with no copy of it.
    // Queries aren't held up: a read lock only stalls them when a writer is
    // waiting, and the only writer is this (maintenance) thread.
    let (generation, perms) = {
        let guard = sh.index.read().unwrap_or_else(PoisonError::into_inner);
        let Some(idx) = guard.as_ref() else { return };
        if let Err(e) = idx.save(snap) {
            log(&format!("saving {}: {e}", snap.display()));
            return;
        }
        (idx.generation, idx.live_perms())
    };
    sh.dirty.store(false, Relaxed);
    let saved = t.elapsed();
    // Swapping in the mapped, compacted copy frees the heap copy. Only this
    // thread writes, so nothing changed since the save.
    if remap && let Ok(mut fresh) = Index::load(snap) {
        fresh.generation = generation + 1;
        // The snapshot doesn't store permission bits; carry them over.
        fresh.adopt_perms(perms);
        // Built here, outside any lock, so the next update needn't.
        let map = fresh.build_map();
        fresh.install_map(map);
        sh.replace_index(Some(fresh));
    }
    log(&format!("saved snapshot in {:.0} ms", saved.as_secs_f64() * 1e3));
}

/// Read folders on `workers` threads; `background`: at idle priority,
/// pausing under pressure (`throttle`), as a sweep's follow-up does.
fn read_dirs(sh: &Shared, workers: usize, changed: &[Arc<[u8]>], background: bool) -> Vec<index::DirScan> {
    let next = AtomicUsize::new(0);
    let scans = Mutex::new(Vec::with_capacity(changed.len()));
    thread::scope(|sc| {
        for _ in 0..workers.min(changed.len()).max(1) {
            sc.spawn(|| {
                if background {
                    throttle::background_thread();
                }
                let mut mine = Vec::new();
                while let Some(p) = changed.get(next.fetch_add(1, Relaxed)) {
                    if STOP.load(Relaxed) {
                        break;
                    }
                    if background {
                        sh.throttle.wait();
                    }
                    mine.push(index::read_dir(p.clone()));
                }
                scans.lock().unwrap_or_else(PoisonError::into_inner).extend(mine);
            });
        }
    });
    scans.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// Apply re-read folders, then re-read the subfolders that no longer match
/// their listing (replaced by another folder of the same name, or changed
/// too) until none are left. New subtrees are returned for crawling.
fn apply_scans(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>, mut scans: Vec<index::DirScan>) -> Vec<Arc<[u8]>> {
    let mut pending = Vec::new();
    // Each round goes one level deeper; a bound keeps a pathological tree
    // from holding up updates (what's left is crawled instead).
    for _ in 0..64 {
        if scans.is_empty() {
            return pending;
        }
        ensure_map(sh);
        let applied = {
            let mut g = sh.index.write().unwrap_or_else(PoisonError::into_inner);
            match g.as_mut() {
                Some(idx) => idx.apply(scans, skip),
                None => return pending,
            }
        };
        sh.dirty.store(true, Relaxed);
        pending.extend(applied.pending);
        let recheck: Vec<Arc<[u8]>> = applied.recheck.into_iter().filter(|p| !skipped(p, skip)).collect();
        scans = read_dirs(sh, cfg.workers, &recheck, false);
    }
    pending.extend(scans.into_iter().map(|s| s.path));
    pending
}

/// Crawl new subtrees and add them to the index.
fn crawl_into(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>, pending: &[Arc<[u8]>], background: bool) {
    if pending.is_empty() {
        return;
    }
    let seen = AtomicUsize::new(0);
    let crawl = if background {
        background_crawl(cfg, sh, skip, &seen)
    } else {
        Crawl { workers: cfg.workers, cancel: &STOP, skip, with_meta: false, strict: true, seen: &seen, init: &|| {}, pace: &|| {} }
    };
    let (shards, errors) = index::crawl(pending, &crawl);
    if let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut() {
        idx.add_shards(shards, errors);
    }
    sh.dirty.store(true, Relaxed);
}

/// Changes reported as they happen (fanotify): index updates, at normal
/// priority and never paused, so the index stays current on battery or
/// under pressure.
fn refresh_live(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>, changed: Vec<Arc<[u8]>>) {
    let changed: Vec<Arc<[u8]>> = changed.into_iter().filter(|p| fsys::in_scope(p, &sh.root) && !skipped(p, skip)).collect();
    if changed.is_empty() {
        return;
    }
    let scans = read_dirs(sh, cfg.workers, &changed, false);
    let pending = apply_scans(sh, cfg, skip, scans);
    crawl_into(sh, cfg, skip, &pending, false);
}

/// Folders whose attributes changed: record whether everyone may list them
/// now, which updates the chain bits of their subtrees.
fn refresh_perms(sh: &Shared, paths: Vec<Vec<u8>>) {
    let found: Vec<(Vec<u8>, u8)> = paths
        .into_iter()
        .filter(|p| fsys::in_scope(p, &sh.root))
        .map(|p| {
            let own = if sys::path_world_listable(&p) { index::OWN_PUBLIC } else { 0 };
            (p, own)
        })
        .collect();
    if found.is_empty() {
        return;
    }
    ensure_map(sh);
    if let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut() {
        for (p, own) in &found {
            idx.update_own(p, *own);
        }
    }
}

fn background_crawl<'a>(cfg: &Config, sh: &'a Shared, skip: &'a HashSet<Vec<u8>>, seen: &'a AtomicUsize) -> Crawl<'a> {
    Crawl { workers: cfg.workers, cancel: &STOP, skip, with_meta: false, strict: true, seen, init: &throttle::background_thread, pace: &*sh.pace }
}

/// Background work, done on a thread of its own so the maintenance thread
/// keeps applying live changes while it runs (or waits out a pause).
enum Job {
    /// Verify every folder, and re-read the ones that changed.
    Sweep,
    /// Crawl new subtrees a sweep found.
    Crawl { pending: Vec<Arc<[u8]>>, skip: HashSet<Vec<u8>> },
}

enum Done {
    Swept { epoch: u64, started_ns: u64, took: Duration, perms: Vec<(u32, u32, u8)>, scans: Vec<index::DirScan> },
    /// The index was replaced or went away mid-sweep; sweep again.
    SweepLost,
    Crawled,
    Failed(String),
}

fn run_job(sh: &Shared, cfg: &Config, job: Job) -> Done {
    match job {
        Job::Sweep => {
            throttle::background_thread();
            let started_ns = now_ns();
            let t = Instant::now();
            let epoch = sh.epoch.load(Relaxed);
            let with_index = |f: &mut dyn FnMut(&Index)| {
                let g = sh.index.read().unwrap_or_else(PoisonError::into_inner);
                match g.as_ref() {
                    Some(idx) if sh.epoch.load(Relaxed) == epoch => {
                        f(idx);
                        true
                    }
                    _ => false,
                }
            };
            let Some(swept) = index::sweep_in_batches(&with_index, cfg.workers, &STOP, &throttle::background_thread, &*sh.pace) else { return Done::SweepLost };
            let took = t.elapsed();
            let scans = read_dirs(sh, cfg.workers, &swept.changed, true);
            Done::Swept { epoch, started_ns, took, perms: swept.perms, scans }
        }
        Job::Crawl { pending, skip } => {
            crawl_into(sh, cfg, &skip, &pending, true);
            Done::Crawled
        }
    }
}

/// The background job thread: one job at a time, a panic reported as `Failed`.
fn start_jobs(sh: &Arc<Shared>, cfg: &Arc<Config>) -> Option<(std::sync::mpsc::Sender<Job>, std::sync::mpsc::Receiver<Done>)> {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Done>();
    let (sh, cfg) = (sh.clone(), cfg.clone());
    thread::Builder::new()
        .name("fex-background".into())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&sh, &cfg, job))).unwrap_or_else(|p| Done::Failed(panic_text(&p)));
                if done_tx.send(done).is_err() {
                    return;
                }
            }
        })
        .ok()?;
    Some((job_tx, done_rx))
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown".into())
}

/// Keep maintenance running: a panic in it must not leave the daemon
/// answering from an index nobody updates any more. The index it was
/// changing may be half-updated, so the last snapshot is reloaded (a sweep
/// then catches up), and maintenance starts over.
fn supervise(sh: &Arc<Shared>, cfg: &Arc<Config>, snap: &Path) {
    let mut failures = 0u32;
    while !STOP.load(Relaxed) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| maintain(sh, cfg, snap))) {
            Ok(()) => return,
            Err(p) => {
                failures += 1;
                sh.perms_live.store(false, Relaxed);
                log(&format!("maintenance stopped by an internal error ({}); reloading the last snapshot and starting over", panic_text(&p)));
                let reloaded = Index::load(snap).ok().filter(|i| *i.root == *cfg.root);
                sh.replace_index(reloaded);
                sh.dirty.store(false, Relaxed);
                sh.set_activity("recovering");
                // Back off if it keeps failing, so a persistent fault doesn't spin.
                let wait = Duration::from_secs((5 * failures as u64).min(300));
                let until = Instant::now() + wait;
                while Instant::now() < until && !STOP.load(Relaxed) {
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
}

fn maintain(sh: &Arc<Shared>, cfg: &Arc<Config>, snap: &Path) {
    let mut skip = skips(cfg);
    // Watch first, so nothing that changes during the crawl or sweep is missed.
    let fan = if cfg.fanotify && sh.privileged {
        match Fanotify::start(&watch_points(cfg, &skip)) {
            Ok(f) => {
                for (p, e) in &f.refused {
                    log(&format!("no fanotify on {}: {e} (periodic sweeps only)", String::from_utf8_lossy(p)));
                }
                if !f.attrib {
                    log("fanotify doesn't report attribute changes here: permissions are checked at query time");
                }
                sh.watched.store(f.marked.len() as u32, Relaxed);
                Some(f)
            }
            Err(e) => {
                log(&format!("{e}; periodic sweeps only"));
                None
            }
        }
    } else {
        None
    };
    // Permission bits can be trusted only if every change to them is seen.
    let perms_live = fan.as_ref().is_some_and(|f| f.attrib && f.refused.is_empty() && !f.marked.is_empty());

    // The folder map updates need, built under a read lock and installed
    // with a brief write lock (queries never wait for its construction).
    ensure_map(sh);
    let have = sh.index.read().unwrap_or_else(PoisonError::into_inner).is_some();
    if !have {
        sh.set_activity("crawling");
        let t = Instant::now();
        let seen = AtomicUsize::new(0);
        let started = now_ns();
        let Some(mut idx) = index::build(&cfg.root, &background_crawl(cfg, sh, &skip, &seen)) else { return };
        log(&format!("crawled {} entries, {} folders in {:.2} s ({} unreadable)", idx.entries, idx.dirs, t.elapsed().as_secs_f64(), idx.errors));
        // The profile estimated the file count; now it's known.
        let path = cfg.state_dir.join("profile");
        if let Some(p) = Profile::load(&path) {
            let files = idx.entries as u64;
            if files.abs_diff(p.files) * 5 > p.files {
                let p = p.with_files(files);
                let _ = p.save(&path);
                if cfg.reserve.is_none() {
                    sh.throttle.set_reserve(p.reserve);
                }
                log(&format!("profile updated with the real file count: reserve {:.2}%", p.reserve * 100.0));
            }
        }
        // The crawl recorded each folder's own permission bit; chain them.
        idx.install_perms(&[]);
        sh.replace_index(Some(idx));
        sh.verified_ns.store(started, Relaxed);
        save(sh, snap, true);
        sh.set_activity("idle");
    }
    sh.perms_live.store(perms_live, Relaxed);

    let Some((jobs, done)) = start_jobs(sh, cfg) else {
        log("cannot start the background thread; live updates only");
        return maintain_live_only(sh, cfg, snap, fan.as_ref(), skip);
    };
    let mut busy = 0usize;
    let mut sweep_wanted = have; // a loaded snapshot is verified first
    let mut last_sweep = Instant::now();
    let mut last_save = Instant::now();
    let mut last_trim = Instant::now();

    while !STOP.load(Relaxed) {
        if sweep_wanted && busy == 0 && jobs.send(Job::Sweep).is_ok() {
            sweep_wanted = false;
            busy += 1;
            sh.set_activity("verifying");
        }
        let events = match &fan {
            Some(f) => f.wait(1000),
            None => {
                thread::sleep(Duration::from_secs(1));
                Events::Dirs { dirs: Vec::new(), attrib: Vec::new() }
            }
        };
        match events {
            Events::Dirs { dirs, attrib } => {
                if !dirs.is_empty() {
                    refresh_live(sh, cfg, &skip, dirs.into_iter().map(Arc::from).collect());
                }
                if !attrib.is_empty() && perms_live {
                    refresh_perms(sh, attrib);
                }
            }
            Events::Overflow => {
                log("change queue overflowed; verifying everything");
                // Permission changes may have been missed too.
                if let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut() {
                    idx.clear_chains();
                }
                sweep_wanted = true;
            }
        }
        while let Ok(d) = done.try_recv() {
            busy = busy.saturating_sub(1);
            match d {
                Done::Swept { epoch, started_ns, took, perms, scans } => {
                    if sh.epoch.load(Relaxed) != epoch {
                        sweep_wanted = true;
                        continue;
                    }
                    let n = scans.len();
                    let t = Instant::now();
                    ensure_map(sh);
                    if let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut() {
                        idx.install_perms(&perms);
                    }
                    let pending = apply_scans(sh, cfg, &skip, scans);
                    if !pending.is_empty() && jobs.send(Job::Crawl { pending, skip: skip.clone() }).is_ok() {
                        busy += 1;
                    }
                    sh.verified_ns.store(started_ns, Relaxed);
                    log(&format!("verified in {:.0} ms; {n} folders changed, updated in {:.0} ms", took.as_secs_f64() * 1e3, t.elapsed().as_secs_f64() * 1e3));
                }
                Done::SweepLost => sweep_wanted = true,
                Done::Crawled => {}
                Done::Failed(e) => log(&format!("background work failed ({e}); it will be retried at the next verification")),
            }
            if busy == 0 {
                sh.set_activity("idle");
            }
        }
        if last_sweep.elapsed() >= cfg.sweep_every {
            // Mounts come and go: a network share mounted since the last
            // sweep must neither be crawled nor reported as covered.
            skip = skips(cfg);
            *sh.skip.write().unwrap_or_else(PoisonError::into_inner) = skip.clone();
            sweep_wanted = true;
            last_sweep = Instant::now();
        }
        // Saving replaces the index (new folder ids), so it waits until no
        // sweep is using the old ids. Memory critical: changes held in heap
        // move into the (reclaimable) mapped snapshot.
        if busy == 0 {
            if sh.compact_requested.swap(false, Relaxed) {
                save(sh, snap, true);
                last_save = Instant::now();
            }
            let compact = sh.index.read().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(Index::needs_compaction);
            if sh.dirty.load(Relaxed) && (compact || last_save.elapsed() >= cfg.save_every) {
                save(sh, snap, true);
                last_save = Instant::now();
            }
        }
        // Queries free what they allocated; give it back to the system now
        // and then rather than holding it until memory gets tight.
        if last_trim.elapsed() >= Duration::from_secs(60) && sh.queries.swap(0, Relaxed) > 0 {
            #[cfg(target_env = "gnu")]
            unsafe {
                libc::malloc_trim(0);
            }
            last_trim = Instant::now();
        }
    }
}

/// Maintenance without a background thread (it couldn't be started):
/// live changes only, sweeps inline.
fn maintain_live_only(sh: &Shared, cfg: &Config, snap: &Path, fan: Option<&Fanotify>, skip: HashSet<Vec<u8>>) {
    let mut last_save = Instant::now();
    while !STOP.load(Relaxed) {
        match fan.map(|f| f.wait(1000)) {
            Some(Events::Dirs { dirs, .. }) if !dirs.is_empty() => refresh_live(sh, cfg, &skip, dirs.into_iter().map(Arc::from).collect()),
            Some(_) => {}
            None => thread::sleep(Duration::from_secs(1)),
        }
        if sh.dirty.load(Relaxed) && last_save.elapsed() >= cfg.save_every {
            save(sh, snap, true);
            last_save = Instant::now();
        }
    }
}

// ------------------------------------------------------------ memory

/// Resident and anonymous (heap) memory of this process, in kB.
fn resident_kb() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)?.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).unwrap_or(0);
    (field("Rss:"), field("Anonymous:"))
}

fn memory_state(sh: &Shared) -> String {
    let (rss, anon) = resident_kb();
    format!(
        "{:.1} MB resident ({:.1} MB heap, {:.1} MB file-backed) · {} ({:.0}% of memory available)",
        rss as f64 / 1024.0,
        anon as f64 / 1024.0,
        rss.saturating_sub(anon) as f64 / 1024.0,
        sh.throttle.memory_level().name(),
        sh.throttle.memory_available() * 100.0
    )
}

/// The folder map, built under a read lock if it was dropped, so updates
/// never build it while holding the write lock.
fn ensure_map(sh: &Shared) {
    let built = sh.index.read().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|i| !i.has_map()).map(Index::build_map);
    if let Some(map) = built
        && let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut()
        && !idx.has_map()
    {
        idx.install_map(map);
    }
}

/// Shed memory when the level rises, and again every 30 s while it stays up
/// (what was freed may have been refilled since); SIGUSR1 sheds at once. On
/// a thread of its own, so it works while the maintenance thread is busy
/// with (or paused in) a crawl or sweep.
fn watch_memory(sh: &Shared) {
    let mut was = memory::Level::Normal;
    let mut last = Instant::now();
    while !STOP.load(Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let level = sh.throttle.memory_level();
        if SHED_NOW.swap(false, Relaxed) {
            shed(sh, memory::Level::Critical, "on request");
            last = Instant::now();
        } else if level > was || (level != memory::Level::Normal && last.elapsed() >= Duration::from_secs(30)) {
            shed(sh, level, &format!("{:.0}% of memory available", sh.throttle.memory_available() * 100.0));
            last = Instant::now();
        }
        was = level;
    }
}

/// Give memory back (background work already pauses while memory is tight).
/// Tight: return freed heap to the OS and mark the mapped index cold, so the
/// kernel reclaims it before other programs' memory. Critical: also drop the
/// folder map (rebuilt when an update needs it), have changes held in heap
/// folded into the snapshot, and page the mapped index out now.
fn shed(sh: &Shared, level: memory::Level, why: &str) {
    let (before, _) = resident_kb();
    let mut dropped = 0;
    if level == memory::Level::Critical {
        let overlay = {
            let mut g = sh.index.write().unwrap_or_else(PoisonError::into_inner);
            g.as_mut().map_or(0, |idx| {
                dropped = idx.drop_map();
                idx.bytes()
            })
        };
        if overlay > 4 << 20 {
            sh.compact_requested.store(true, Relaxed);
        }
    }
    if let Some(idx) = sh.index.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
        idx.advise_mapped(if level == memory::Level::Critical { libc::MADV_PAGEOUT } else { libc::MADV_COLD });
    }
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
    let (after, _) = resident_kb();
    log(&format!(
        "memory {} ({why}): resident {:.1} -> {:.1} MB{}",
        level.name(),
        before as f64 / 1024.0,
        after as f64 / 1024.0,
        if dropped > 0 { format!(", folder map ({:.1} MB) dropped", dropped as f64 / 1048576.0) } else { String::new() }
    ));
}

// ------------------------------------------------------------ clients

struct Creds {
    pid: i32,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

/// SO_PEERGROUPS (asm-generic/socket.h); not in the libc crate.
const SO_PEERGROUPS: libc::c_int = 59;

fn peer_creds(s: &UnixStream) -> Option<Creds> {
    let fd = s.as_raw_fd();
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) } != 0 {
        return None;
    }
    // Supplementary groups as they were at connect time. If they can't be
    // read, use the primary group alone: fewer groups can only hide more.
    let mut groups = vec![0u32; 64];
    loop {
        let mut len = (groups.len() * 4) as libc::socklen_t;
        let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, SO_PEERGROUPS, groups.as_mut_ptr() as *mut libc::c_void, &mut len) };
        if rc == 0 {
            groups.truncate(len as usize / 4);
            break;
        }
        let needed = len as usize / 4;
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) && needed > groups.len() && needed <= 65536 {
            groups.resize(needed, 0);
        } else {
            groups.clear();
            break;
        }
    }
    Some(Creds { pid: cred.pid, uid: cred.uid, gid: cred.gid, groups })
}

thread_local! {
    /// The uid this thread is currently checking access as, if any.
    static ACTING_AS: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Take on `c`'s filesystem identity on this thread only (Linux credentials
/// are per thread; the raw setgroups syscall avoids glibc applying it to the
/// whole process). Leaving fsuid 0 drops the filesystem capabilities, so the
/// kernel checks access exactly as for the client.
fn impersonate(c: &Creds) -> bool {
    unsafe {
        if libc::syscall(libc::SYS_setgroups, c.groups.len(), c.groups.as_ptr()) != 0 {
            return false;
        }
        libc::setfsgid(c.gid);
        libc::setfsuid(c.uid);
        // Passing an invalid id changes nothing and returns the current one.
        libc::setfsuid(u32::MAX) as u32 == c.uid && libc::setfsgid(u32::MAX) as u32 == c.gid
    }
}

fn restore_identity() {
    unsafe {
        libc::setfsuid(0);
        libc::setfsgid(0);
        libc::syscall(libc::SYS_setgroups, 0usize, std::ptr::null::<u32>());
    }
    ACTING_AS.with(|a| a.set(None));
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore_identity();
    }
}

/// Per-query visibility for one client. A user-level crawl can only list
/// folders it can read, reached through folders it can read, so a match is
/// shown only if its folder and every folder between the search scope and
/// it can be listed by the client (names under an execute-only folder stay
/// private). Folders everyone can list, as recorded at indexing time, need
/// no check at all.
struct UserVisibility<'a> {
    creds: &'a Creds,
    /// The search scope, without trailing slashes (the client named it).
    scope: &'a [u8],
    trusted: bool,
    /// Per folder: 0 unknown, 1 visible, 2 hidden.
    cache: Vec<AtomicU8>,
    /// Folders above matches (between them and the scope): listable or not.
    above: Mutex<HashMap<Vec<u8>, bool>>,
}

impl UserVisibility<'_> {
    /// Can the client list `path`? Listing a folder needs read permission on
    /// it and search permission on every ancestor: exactly what opening it
    /// for reading requires (the thread acts as the client).
    fn listable(&self, idx: &Index, path: &[u8]) -> bool {
        if self.trusted && idx.chain_public_path(path) {
            return true;
        }
        sys::cstr(path).is_ok_and(|c| {
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
            fd >= 0
        })
    }
}

impl Visibility for UserVisibility<'_> {
    fn enter(&self) -> Option<Box<dyn std::any::Any>> {
        let ok = impersonate(self.creds);
        ACTING_AS.with(|a| a.set(ok.then_some(self.creds.uid)));
        Some(Box::new(Restore))
    }

    fn trusts_public(&self) -> bool {
        self.trusted
    }

    fn visible(&self, idx: &Index, dir: usize, path: &[u8]) -> bool {
        // Fail closed if this thread isn't acting as the client.
        if ACTING_AS.with(Cell::get) != Some(self.creds.uid) {
            return false;
        }
        let Some(slot) = self.cache.get(dir) else { return false };
        match slot.load(Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        let mut ok = self.listable(idx, path);
        // Every folder from the match's parent up to the scope.
        let mut at = fsys::trim_end_slashes(path).to_vec();
        while ok && at.as_slice() != self.scope && fsys::in_scope(&at, self.scope) {
            let Some(up) = fsys::parent(&at) else { break };
            at = fsys::trim_end_slashes(&up).to_vec();
            let known = self.above.lock().unwrap_or_else(PoisonError::into_inner).get(&at).copied();
            ok = match known {
                Some(v) => v,
                None => {
                    let v = self.listable(idx, &at);
                    self.above.lock().unwrap_or_else(PoisonError::into_inner).insert(at.clone(), v);
                    v
                }
            };
        }
        slot.store(if ok { 1 } else { 2 }, Relaxed);
        ok
    }
}

fn serve(sh: &Shared, mut s: UnixStream, creds: Creds) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let reply = |s: &mut UnixStream, status: u8, msg: &[u8]| {
        let mut out = vec![status];
        proto::put_bytes(&mut out, msg);
        let _ = proto::send(s, &out);
    };
    let me = unsafe { libc::geteuid() };
    if !sh.privileged && creds.uid != me {
        return reply(&mut s, proto::ERROR, format!("this index daemon serves only uid {me}").as_bytes());
    }
    let Ok(req) = proto::recv(&mut s, proto::MAX_REQUEST) else { return };
    let mut r = Reader::new(&req);
    let protocol = r.u8().ok();
    if protocol != Some(crate::wire::PROTOCOL) {
        return reply(&mut s, proto::ERROR, crate::wire::protocol_mismatch(protocol).as_bytes());
    }
    match r.u8() {
        Ok(proto::QUERY) => {
            let parsed = (|| Ok::<_, std::io::Error>((r.u8()? != 0, r.u32()?, r.bytes()?.to_vec(), String::from_utf8_lossy(r.bytes()?).into_owned())))();
            let Ok((regex, limit, scope, text)) = parsed else {
                return reply(&mut s, proto::ERROR, b"malformed request");
            };
            // Older clients stop here: whole result, name order.
            let order = r.u8().unwrap_or(proto::NAME_ASC);
            let first = r.u32().unwrap_or(u32::MAX) as usize;
            let seq = r.u64().unwrap_or(0);
            if !scope.starts_with(b"/") || text.len() > 4096 {
                return reply(&mut s, proto::ERROR, b"scope must be an absolute path; queries are limited to 4096 bytes");
            }
            let m = match Matcher::new(&text, regex) {
                Ok(Some(m)) => m,
                Ok(None) => {
                    let _ = proto::send(&mut s, &Encoder::new(0).chunk(&[], true));
                    return;
                }
                Err(e) => return reply(&mut s, proto::ERROR, e.as_bytes()),
            };
            let limit = limit.min(proto::MAX_LIMIT) as usize;
            let guard = sh.index.read().unwrap_or_else(PoisonError::into_inner);
            let Some(idx) = guard.as_ref() else {
                drop(guard);
                return reply(&mut s, proto::BUSY, b"the first index is still being built");
            };
            if !idx.covers(&scope) || skipped(&scope, &sh.skip.read().unwrap_or_else(PoisonError::into_inner)) {
                let root = idx.root.to_vec();
                drop(guard);
                return reply(&mut s, proto::NOT_COVERED, &root);
            }
            // Search as you type: a newer query from the same client (by the
            // client's own numbering, not arrival order) makes this one
            // obsolete. 0 means unnumbered: never superseded.
            let latest = {
                let mut clients = sh.clients.lock().unwrap_or_else(PoisonError::into_inner);
                if clients.len() > 256 {
                    clients.retain(|_, c| Arc::strong_count(c) > 1);
                }
                clients.entry(creds.pid).or_default().clone()
            };
            if seq != 0 {
                latest.fetch_max(seq, Relaxed);
            }
            let cancelled = || seq != 0 && latest.load(Relaxed) > seq;
            sh.queries.fetch_add(1, Relaxed);
            let hits = if creds.uid == 0 || !sh.privileged {
                idx.query_hits(&m, &scope, &cancelled, &Everything)
            } else {
                let vis = UserVisibility {
                    creds: &creds,
                    scope: fsys::trim_end_slashes(&scope),
                    trusted: sh.perms_live.load(Relaxed),
                    cache: std::iter::repeat_with(|| AtomicU8::new(0)).take(idx.dir_slots()).collect(),
                    above: Mutex::new(HashMap::new()),
                };
                idx.query_hits(&m, &scope, &cancelled, &vis)
            };
            // Superseded: the client has moved on.
            let Some(mut hits) = hits else {
                drop(guard);
                return;
            };
            let total = hits.len();
            let desc = order == proto::NAME_DESC;
            // The first screenful, found and sorted in place in the index
            // (one linear selection over all matches, before anything else).
            let first = first.clamp(1, MAX_FIRST).min(hits.len()).min(limit);
            if order != proto::UNORDERED && first > 0 {
                if first < hits.len() {
                    hits.select_nth_unstable_by(first - 1, |a, b| idx.hit_cmp(a, b, desc));
                }
                hits[..first].sort_unstable_by(|a, b| idx.hit_cmp(a, b, desc));
            }
            let kept = limit.min(hits.len());
            // Sent under the index lock, so it must fit in the socket buffer
            // (a client that stops reading then can't block the send): fewer
            // rows if long paths make it big. They stay a sorted prefix; the
            // rest follow with the other rows.
            let mut first = first;
            let (mut enc, body) = loop {
                let mut enc = Encoder::new(total);
                let body = enc.chunk(&idx.hit_rows(&hits[..first]), first == kept);
                if body.len() <= MAX_FIRST_BYTES || first <= 1 {
                    break (enc, body);
                }
                first /= 2;
            };
            let sent = proto::send(&mut s, &body).is_ok();
            // Past the limit, keep the rows that come first by name.
            let mut tail = hits.split_off(first);
            idx.top_hits(&mut tail, kept - first, desc);
            let rest = idx.hit_rows(&tail);
            let hits_len = kept;
            drop(guard);
            if sent && first < hits_len {
                let mut at = 0;
                while at < rest.len() {
                    let end = (at + CHUNK_ROWS).min(rest.len());
                    if proto::send(&mut s, &enc.chunk(&rest[at..end], end == rest.len())).is_err() {
                        break;
                    }
                    at = end;
                }
            }
        }
        Ok(proto::STATUS) => {
            let g = sh.index.read().unwrap_or_else(PoisonError::into_inner);
            let mut st = proto::Status {
                root: sh.root.clone(),
                verified_ns: sh.verified_ns.load(Relaxed),
                watched: sh.watched.load(Relaxed),
                activity: sh.activity.lock().unwrap_or_else(PoisonError::into_inner).to_string(),
                paused: sh.throttle.paused().unwrap_or_default(),
                reserve: sh.throttle.reserve_state(),
                memory: memory_state(sh),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            };
            if let Some(i) = g.as_ref() {
                st.ready = true;
                st.entries = i.entries as u64;
                st.dirs = i.dirs as u64;
                st.generation = i.generation;
                st.mapped_bytes = i.mapped_bytes() as u64;
                st.heap_bytes = i.bytes() as u64;
            }
            drop(g);
            let _ = proto::send(&mut s, &proto::encode_status(&st));
        }
        _ => reply(&mut s, proto::ERROR, b"unknown request"),
    }
}

/// Rows per chunk after the first.
const CHUNK_ROWS: usize = 16 * 1024;
/// Largest first chunk (rows); about 60 KB, well inside a socket buffer.
const MAX_FIRST: usize = 1000;
/// Largest first chunk in bytes: well inside a socket's default send buffer
/// (about 208 KB), so sending it while holding the index lock never blocks.
const MAX_FIRST_BYTES: usize = 64 * 1024;

/// Encodes a streamed reply: the header goes with the first chunk, and
/// each folder path is sent once, numbered across chunks.
struct Encoder {
    header: Option<u64>,
    dirs: HashMap<Arc<[u8]>, u32>,
}

impl Encoder {
    fn new(total: usize) -> Encoder {
        Encoder { header: Some(total as u64), dirs: HashMap::new() }
    }

    fn chunk(&mut self, rows: &[fsys::Row], last: bool) -> Vec<u8> {
        let mut body = Vec::with_capacity(rows.len() * 24 + 32);
        if let Some(total) = self.header.take() {
            body.push(proto::OK);
            proto::put_u64(&mut body, total);
        }
        body.push(last as u8);
        let mut new_dirs: Vec<Arc<[u8]>> = Vec::new();
        let mut encoded = Vec::with_capacity(rows.len() * 20);
        for row in rows {
            let dir = row.dir.clone().unwrap_or_else(|| Arc::from(&b""[..]));
            let next = self.dirs.len() as u32;
            let id = *self.dirs.entry(dir.clone()).or_insert_with(|| {
                new_dirs.push(dir);
                next
            });
            proto::put_u32(&mut encoded, id);
            encoded.push(row.flags);
            proto::put_bytes(&mut encoded, &row.name);
        }
        proto::put_u32(&mut body, new_dirs.len() as u32);
        for d in &new_dirs {
            proto::put_bytes(&mut body, d);
        }
        proto::put_u32(&mut body, rows.len() as u32);
        body.extend_from_slice(&encoded);
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    /// Names below a folder the client can't list stay hidden even where
    /// the folder holding them is readable: a crawl as the client could
    /// never have reached them. (Runs as the current user, so the identity
    /// switch is skipped; the checks themselves are the kernel's.)
    #[test]
    fn matches_under_an_unlistable_folder_stay_hidden() {
        let root_dir = std::env::temp_dir().join(format!("fex-vis-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root_dir);
        fs::create_dir_all(root_dir.join("open/sub")).unwrap();
        fs::create_dir_all(root_dir.join("closed/pub")).unwrap();
        fs::write(root_dir.join("open/sub/match-a.txt"), b"").unwrap();
        fs::write(root_dir.join("closed/pub/match-b.txt"), b"").unwrap();
        let r = root_dir.as_os_str().as_bytes().to_vec();
        let idx = index::Build::new(&r, false).run().unwrap();
        // Search permission only: reachable by a known path, not listable.
        fs::set_permissions(root_dir.join("closed"), fs::Permissions::from_mode(0o311)).unwrap();
        // Root bypasses permission checks, so as root (e.g. CI in a
        // container) check as an unprivileged client through the daemon's
        // real per-thread impersonation instead.
        let root = unsafe { libc::geteuid() } == 0;
        for d in [&root_dir, &root_dir.join("open"), &root_dir.join("open/sub")] {
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let creds = if root {
            Creds { pid: 0, uid: 65534, gid: 65534, groups: Vec::new() }
        } else {
            Creds { pid: 0, uid: unsafe { libc::geteuid() }, gid: unsafe { libc::getegid() }, groups: Vec::new() }
        };
        let vis = UserVisibility { creds: &creds, scope: &r, trusted: false, cache: std::iter::repeat_with(|| AtomicU8::new(0)).take(idx.dir_slots()).collect(), above: Mutex::new(HashMap::new()) };
        struct AsMe<'a>(&'a UserVisibility<'a>);
        impl Visibility for AsMe<'_> {
            fn enter(&self) -> Option<Box<dyn std::any::Any>> {
                ACTING_AS.with(|a| a.set(Some(self.0.creds.uid)));
                None
            }
            fn visible(&self, idx: &Index, dir: usize, path: &[u8]) -> bool {
                self.0.visible(idx, dir, path)
            }
        }
        let m = Matcher::new("match", false).unwrap().unwrap();
        let hits = if root { idx.query_hits(&m, &r, &|| false, &vis) } else { idx.query_hits(&m, &r, &|| false, &AsMe(&vis)) }.unwrap();
        let names: Vec<Vec<u8>> = idx.hit_rows(&hits).iter().map(|row| row.name.to_vec()).collect();
        fs::set_permissions(root_dir.join("closed"), fs::Permissions::from_mode(0o755)).unwrap();
        ACTING_AS.with(|a| a.set(None));
        assert_eq!(names, vec![b"match-a.txt".to_vec()]);
        let _ = fs::remove_dir_all(root_dir);
    }
}
