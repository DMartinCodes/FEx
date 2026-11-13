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

fn skips(cfg: &Config) -> HashSet<Vec<u8>> {
    let mut skip = HashSet::new();
    for m in sys::mounts() {
        if sys::is_pseudo_fs(&m.fstype) || (!cfg.include_network && is_network_fs(&m.fstype)) {
            skip.insert(m.point);
        }
    }
    skip.remove(&cfg.root);
    skip
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
    let throttle = Throttle::start(cfg.io_limit);
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
    if let Some(m) = maint {
        let _ = m.join();
    }
    if shared.dirty.load(Relaxed) {
        save(&shared, &snap, false);
    }
    let _ = std::fs::remove_file(&cfg.socket);
    ExitCode::SUCCESS
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
    let guard = sh.index.read().unwrap_or_else(PoisonError::into_inner);
    let Some(idx) = guard.as_ref() else { return };
    if let Err(e) = idx.save(snap) {
        log(&format!("saving {}: {e}", snap.display()));
        return;
    }
    let generation = idx.generation;
    drop(guard);
    sh.dirty.store(false, Relaxed);
    let saved = t.elapsed();
    // Swapping in the mapped, compacted copy frees the heap copy. Only this
    // thread writes, so nothing changed since the save.
    if remap && let Ok(mut fresh) = Index::load(snap) {
        fresh.generation = generation + 1;
        *sh.index.write().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
    }
    log(&format!("saved snapshot in {:.0} ms", saved.as_secs_f64() * 1e3));
}

/// Read changed folders (on idle threads), apply them, crawl new subtrees.
fn refresh(sh: &Shared, cfg: &Config, skip: &HashSet<Vec<u8>>, changed: Vec<Arc<[u8]>>) {
    if changed.is_empty() {
        return;
    }
    let changed: Vec<Arc<[u8]>> = changed.into_iter().filter(|p| fsys::in_scope(p, &sh.root)).collect();
    let next = AtomicUsize::new(0);
    let scans = Mutex::new(Vec::with_capacity(changed.len()));
    thread::scope(|sc| {
        for _ in 0..cfg.workers.min(changed.len()).max(1) {
            sc.spawn(|| {
                throttle::background_thread();
                let mut mine = Vec::new();
                while let Some(p) = changed.get(next.fetch_add(1, Relaxed)) {
                    sh.throttle.wait();
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
        let (shards, errors) = index::crawl(&pending, &background_crawl(cfg, sh, skip, &seen));
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
    refresh(sh, cfg, skip, changed);
    if !STOP.load(Relaxed) {
        sh.verified_ns.store(started, Relaxed);
    }
    log(&format!("verified in {:.0} ms; {n} folders changed, updated in {:.0} ms", swept.as_secs_f64() * 1e3, (t.elapsed() - swept).as_secs_f64() * 1e3));
    sh.set_activity("idle");
}

fn maintain(sh: &Shared, cfg: &Config, snap: &Path) {
    let skip = skips(cfg);
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
                refresh(sh, cfg, &skip, dirs.into_iter().map(Arc::from).collect());
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
    Some(Creds { uid: cred.uid, gid: cred.gid, groups })
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
    checks: AtomicUsize,
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
        self.checks.fetch_add(1, Relaxed);
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
            if !scope.starts_with(b"/") || text.len() > 4096 {
                return reply(&mut s, proto::ERROR, b"scope must be an absolute path; queries are limited to 4096 bytes");
            }
            let m = match Matcher::new(&text, regex) {
                Ok(Some(m)) => m,
                Ok(None) => return send_rows(&mut s, &[], 0),
                Err(e) => return reply(&mut s, proto::ERROR, e.as_bytes()),
            };
            let limit = limit.min(proto::MAX_LIMIT) as usize;
            let guard = sh.index.read().unwrap_or_else(PoisonError::into_inner);
            let Some(idx) = guard.as_ref() else {
                drop(guard);
                return reply(&mut s, proto::BUSY, b"the first index is still being built");
            };
            if !idx.covers(&scope) {
                let root = idx.root.to_vec();
                drop(guard);
                return reply(&mut s, proto::NOT_COVERED, &root);
            }
            let result = if creds.uid == 0 || !sh.privileged {
                idx.query_as(&m, &scope, limit, &|| false, &Everything)
            } else {
                let vis = UserVisibility { creds: &creds, cache: (0..idx.dir_slots()).map(|_| AtomicU8::new(0)).collect(), checks: AtomicUsize::new(0) };
                idx.query_as(&m, &scope, limit, &|| false, &vis)
            };
            drop(guard);
            let (rows, total) = result.unwrap_or_default();
            send_rows(&mut s, &rows, total);
        }
        Ok(proto::STATUS) => {
            let g = sh.index.read().unwrap_or_else(PoisonError::into_inner);
            let mut st = proto::Status {
                root: sh.root.clone(),
                verified_ns: sh.verified_ns.load(Relaxed),
                watched: sh.watched.load(Relaxed),
                activity: sh.activity.lock().unwrap_or_else(PoisonError::into_inner).to_string(),
                paused: sh.throttle.paused().unwrap_or_default(),
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

fn send_rows(s: &mut UnixStream, rows: &[fsys::Row], total: usize) {
    let mut dirs: HashMap<&[u8], u32> = HashMap::new();
    let mut order: Vec<&[u8]> = Vec::new();
    let mut body = Vec::with_capacity(rows.len() * 24 + 64);
    let mut encoded = Vec::with_capacity(rows.len() * 20);
    for row in rows {
        let dir: &[u8] = row.dir.as_deref().unwrap_or(b"");
        let id = *dirs.entry(dir).or_insert_with(|| {
            order.push(dir);
            (order.len() - 1) as u32
        });
        proto::put_u32(&mut encoded, id);
        encoded.push(row.flags);
        proto::put_bytes(&mut encoded, &row.name);
    }
    body.push(proto::OK);
    proto::put_u64(&mut body, total as u64);
    proto::put_u32(&mut body, order.len() as u32);
    for d in order {
        proto::put_bytes(&mut body, d);
    }
    proto::put_u32(&mut body, rows.len() as u32);
    body.extend_from_slice(&encoded);
    let _ = proto::send(s, &body);
}
