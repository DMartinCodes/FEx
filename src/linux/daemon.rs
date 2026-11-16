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
}

/// Concurrent client connections served; more wait in the listen queue.
const MAX_CLIENTS: usize = 32;

static STOP: AtomicBool = AtomicBool::new(false);

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
    /// Folders left out of the index (`skips`): queries inside them are not
    /// covered, so clients search them themselves.
    skip: RwLock<HashSet<Vec<u8>>>,
    /// Per client process, the number of its latest query: a newer query
    /// (search as you type) makes older ones stop.
    clients: Mutex<HashMap<i32, Arc<AtomicU64>>>,
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
}

/// Filesystems that only report local changes, or none at all: not crawled
/// unless asked for, since crawling them loads a shared server.
fn is_network_fs(t: &str) -> bool {
    matches!(t, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "fuse.sshfs" | "ceph" | "lustre" | "glusterfs" | "fuse.glusterfs" | "beegfs" | "gpfs" | "9p" | "afs")
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
    }
    let privileged = unsafe { libc::geteuid() } == 0;
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
    let throttle = Throttle::start(cfg.io_limit, cfg.reserve.unwrap_or(profile.reserve), profile.cores);
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
        clients: Mutex::new(HashMap::new()),
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
    let maint = {
        let (sh, cfg, snap) = (shared.clone(), cfg.clone(), snap.clone());
        thread::Builder::new().name("fex-maintain".into()).spawn(move || maintain(&sh, &cfg, &snap)).ok()
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
        active.fetch_add(1, Relaxed);
        let (sh, counter) = (shared.clone(), active.clone());
        let spawned = thread::Builder::new().name("fex-client".into()).spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(&sh, stream)));
            counter.fetch_sub(1, Relaxed);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Relaxed);
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
    let generation = {
        let guard = sh.index.read().unwrap_or_else(PoisonError::into_inner);
        let Some(idx) = guard.as_ref() else { return };
        if let Err(e) = idx.save(snap) {
            log(&format!("saving {}: {e}", snap.display()));
            return;
        }
        idx.generation
    };
    sh.dirty.store(false, Relaxed);
    let saved = t.elapsed();
    // Swapping in the mapped, compacted copy frees the heap copy. Only this
    // thread writes, so nothing changed since the save.
    if remap && let Ok(mut fresh) = Index::load(snap) {
        fresh.generation = generation + 1;
        // Built here, outside any lock, so the next update needn't.
        let map = fresh.build_map();
        fresh.install_map(map);
        *sh.index.write().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
    }
    log(&format!("saved snapshot in {:.0} ms", saved.as_secs_f64() * 1e3));
}

/// Read changed folders (on idle threads), apply them, crawl new subtrees.
/// `live`: changes reported as they happen (fanotify). Those are index
/// updates and run at normal priority, never paused, so the index stays
/// current on battery or under pressure; folders a verification sweep found
/// changed are read on background threads, like the sweep itself.
fn refresh(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>, changed: Vec<Arc<[u8]>>, live: bool) {
    if changed.is_empty() {
        return;
    }
    let changed: Vec<Arc<[u8]>> = changed.into_iter().filter(|p| fsys::in_scope(p, &sh.root) && !skipped(p, skip)).collect();
    if changed.is_empty() {
        return;
    }
    let next = AtomicUsize::new(0);
    let scans = Mutex::new(Vec::with_capacity(changed.len()));
    thread::scope(|sc| {
        for _ in 0..cfg.workers.min(changed.len()).max(1) {
            sc.spawn(|| {
                if !live {
                    throttle::background_thread();
                }
                let mut mine = Vec::new();
                while let Some(p) = changed.get(next.fetch_add(1, Relaxed)) {
                    if STOP.load(Relaxed) {
                        break;
                    }
                    if !live {
                        sh.throttle.wait();
                    }
                    mine.push(index::read_dir(p.clone()));
                }
                scans.lock().unwrap_or_else(PoisonError::into_inner).extend(mine);
            });
        }
    });
    let scans = scans.into_inner().unwrap_or_else(PoisonError::into_inner);
    let pending = {
        let mut g = sh.index.write().unwrap_or_else(PoisonError::into_inner);
        match g.as_mut() {
            Some(idx) => idx.apply(scans, skip),
            None => return,
        }
    };
    if !pending.is_empty() {
        let seen = AtomicUsize::new(0);
        let crawl = if live {
            Crawl { workers: cfg.workers, cancel: &STOP, skip, with_meta: false, seen: &seen, init: &|| {}, pace: &|| {} }
        } else {
            background_crawl(cfg, sh, skip, &seen)
        };
        let (shards, errors) = index::crawl(&pending, &crawl);
        if let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut() {
            idx.add_shards(shards, errors);
        }
    }
    sh.dirty.store(true, Relaxed);
}

fn background_crawl<'a>(cfg: &Config, sh: &'a Shared, skip: &'a HashSet<Vec<u8>>, seen: &'a AtomicUsize) -> Crawl<'a> {
    Crawl { workers: cfg.workers, cancel: &STOP, skip, with_meta: false, seen, init: &throttle::background_thread, pace: &*sh.pace }
}

fn sweep(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>) {
    sh.set_activity("verifying");
    let started = now_ns();
    let t = Instant::now();
    let changed = {
        let g = sh.index.read().unwrap_or_else(PoisonError::into_inner);
        let Some(idx) = g.as_ref() else { return };
        idx.sweep(cfg.workers, &STOP, &throttle::background_thread, &*sh.pace)
    };
    let swept = t.elapsed();
    let n = changed.len();
    refresh(sh, cfg, skip, changed, false);
    if !STOP.load(Relaxed) {
        sh.verified_ns.store(started, Relaxed);
    }
    log(&format!("verified in {:.0} ms; {n} folders changed, updated in {:.0} ms", swept.as_secs_f64() * 1e3, (t.elapsed() - swept).as_secs_f64() * 1e3));
    sh.set_activity("idle");
}

fn maintain(sh: &Shared, cfg: &Config, snap: &Path) {
    let mut skip = skips(cfg);
    // Watch first, so nothing that changes during the crawl or sweep is missed.
    let fan = if cfg.fanotify && sh.privileged {
        match Fanotify::start(&watch_points(cfg, &skip)) {
            Ok(f) => {
                for (p, e) in &f.refused {
                    log(&format!("no fanotify on {}: {e} (periodic sweeps only)", String::from_utf8_lossy(p)));
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

    // The folder map updates need, built under a read lock and installed
    // with a brief write lock (queries never wait for its construction).
    let built = sh.index.read().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|i| !i.has_map()).map(Index::build_map);
    if let Some(map) = built
        && let Some(idx) = sh.index.write().unwrap_or_else(PoisonError::into_inner).as_mut()
    {
        idx.install_map(map);
    }
    let have = sh.index.read().unwrap_or_else(PoisonError::into_inner).is_some();
    if have {
        sweep(sh, cfg, &skip);
    } else {
        sh.set_activity("crawling");
        let t = Instant::now();
        let seen = AtomicUsize::new(0);
        let started = now_ns();
        let Some(idx) = index::build(&cfg.root, &background_crawl(cfg, sh, &skip, &seen)) else { return };
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
        *sh.index.write().unwrap_or_else(PoisonError::into_inner) = Some(idx);
        sh.verified_ns.store(started, Relaxed);
        save(sh, snap, true);
        sh.set_activity("idle");
    }

    let mut last_sweep = Instant::now();
    let mut last_save = Instant::now();
    while !STOP.load(Relaxed) {
        let events = match &fan {
            Some(f) => f.wait(1000),
            None => {
                thread::sleep(Duration::from_secs(1));
                Events::Dirs(Vec::new())
            }
        };
        match events {
            Events::Dirs(dirs) if !dirs.is_empty() => {
                sh.set_activity("updating");
                refresh(sh, cfg, &skip, dirs.into_iter().map(Arc::from).collect(), true);
                sh.set_activity("idle");
            }
            Events::Dirs(_) => {}
            Events::Overflow => {
                log("change queue overflowed; verifying everything");
                sweep(sh, cfg, &skip);
                last_sweep = Instant::now();
            }
        }
        if last_sweep.elapsed() >= cfg.sweep_every {
            // Mounts come and go: a network share mounted since the last
            // sweep must neither be crawled nor reported as covered.
            skip = skips(cfg);
            *sh.skip.write().unwrap_or_else(PoisonError::into_inner) = skip.clone();
            sweep(sh, cfg, &skip);
            last_sweep = Instant::now();
        }
        let compact = sh.index.read().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(Index::needs_compaction);
        if sh.dirty.load(Relaxed) && (compact || last_save.elapsed() >= cfg.save_every) {
            save(sh, snap, true);
            last_save = Instant::now();
        }
    }
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

/// Per-query visibility for one client.
struct UserVisibility<'a> {
    creds: &'a Creds,
    /// Per folder: 0 unknown, 1 visible, 2 hidden.
    cache: Vec<AtomicU8>,
}

impl Visibility for UserVisibility<'_> {
    fn enter(&self) -> Option<Box<dyn std::any::Any>> {
        let ok = impersonate(self.creds);
        ACTING_AS.with(|a| a.set(ok.then_some(self.creds.uid)));
        Some(Box::new(Restore))
    }

    fn visible(&self, dir: usize, path: &[u8]) -> bool {
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
        // Listing a folder needs read permission on it and search permission
        // on every ancestor: exactly what opening it for reading requires.
        let ok = sys::cstr(path).is_ok_and(|c| {
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
            fd >= 0
        });
        slot.store(if ok { 1 } else { 2 }, Relaxed);
        ok
    }
}

fn serve(sh: &Shared, mut s: UnixStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let Some(creds) = peer_creds(&s) else { return };
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
            let hits = if creds.uid == 0 || !sh.privileged {
                idx.query_hits(&m, &scope, &cancelled, &Everything)
            } else {
                let vis = UserVisibility { creds: &creds, cache: (0..idx.dir_slots()).map(|_| AtomicU8::new(0)).collect() };
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
