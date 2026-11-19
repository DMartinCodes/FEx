//! Multithreaded copy/move engine (the Linux side of `ops.rs`, same design).
//!
//! A job runs in three steps:
//! 1. **Plan:** expand the sources into folders to create and files to
//!    transfer (a parallel walk). A move within one filesystem that doesn't
//!    collide with anything is a single rename, however large the tree.
//! 2. **Resolve conflicts:** if destination files already exist, the user picks
//!    once per job: replace, skip, update (rsync's quick check: skip when
//!    size and modification time match), or keep both.
//! 3. **Transfer:** files go to a worker pool largest-first so the slowest
//!    files start early. Rotational disks get few workers, SSDs more, and
//!    network mounts the most (per-file latency dominates there).
//!
//! Symbolic links are copied as links, never followed, so a move can't
//! delete the files a link points to.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::fileop;
use super::fsys;
use super::sys::{self, DIR, LINK};
use super::walk::{self, Visit};

pub type Path = Vec<u8>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Copy,
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conflict {
    Replace,
    Skip,
    /// Skip files whose size and modification time match (rsync quick check).
    Update,
    KeepBoth,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    Planning = 0,
    AwaitingDecision = 1,
    Transferring = 2,
    Done = 3,
    Cancelled = 4,
}

pub struct Job {
    pub kind: Kind,
    /// Destination folder, for display (the first one when targets differ).
    pub dest: Path,
    pub sources: Vec<Path>,
    /// Each source and the full path it goes to.
    targets: Vec<(Path, Path)>,
    /// Top-level items this job created, as (source, destination). Only items
    /// that didn't exist before are listed, so undo never touches files that
    /// were merged into or replaced.
    pub created: Mutex<Vec<(Path, Path)>>,
    phase: AtomicU8,
    pub files_total: AtomicUsize,
    pub files_done: AtomicUsize,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub conflicts: AtomicUsize,
    pub threads: AtomicUsize,
    cancel: AtomicBool,
    pub errors: Mutex<Vec<String>>,
    pub started: Instant,
    pub elapsed: Mutex<Option<Duration>>,
    decision: Mutex<Option<Sender<Conflict>>>,
}

impl Job {
    pub fn phase(&self) -> Phase {
        match self.phase.load(Relaxed) {
            0 => Phase::Planning,
            1 => Phase::AwaitingDecision,
            2 => Phase::Transferring,
            3 => Phase::Done,
            _ => Phase::Cancelled,
        }
    }

    fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Relaxed);
    }

    pub fn finished(&self) -> bool {
        matches!(self.phase(), Phase::Done | Phase::Cancelled)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
        self.resolve(Conflict::Cancel);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    pub fn resolve(&self, c: Conflict) {
        if let Some(tx) = self.decision.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = tx.send(c);
        }
    }

    fn error(&self, msg: String) {
        self.errors.lock().unwrap_or_else(PoisonError::into_inner).push(msg);
    }

    pub fn error_count(&self) -> usize {
        self.errors.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    fn record(&self, src: &[u8], dst: &[u8]) {
        self.created.lock().unwrap_or_else(PoisonError::into_inner).push((src.to_vec(), dst.to_vec()));
    }

    /// Whether the job reads or writes under `root`.
    pub fn touches(&self, root: &[u8]) -> bool {
        fsys::in_scope(&self.dest, root) || self.sources.iter().any(|s| fsys::in_scope(s, root))
    }
}

/// One file (or link) to transfer.
struct FileItem {
    src: Path,
    dst: Path,
    size: u64,
    mtime: i64,
    /// Destination already exists: (size, mtime).
    existing: Option<(u64, i64)>,
    /// Same-filesystem move: rename instead of copy.
    rename: bool,
    /// One of the job's sources itself, not something inside a folder.
    top: bool,
}

#[derive(Default)]
struct Plan {
    /// Whole-tree renames that collide with nothing.
    renames: Vec<(Path, Path)>,
    /// Folders to create, with the source's mode.
    dirs: Vec<(Path, u32)>,
    files: Vec<FileItem>,
    /// Source folders to remove after a move, children before parents.
    cleanup: Vec<Path>,
    /// Top-level folders created fresh at the destination: (source, destination).
    trees: Vec<(Path, Path)>,
}

/// Start a job on a background thread. `notify` is called when the job needs
/// attention (decision required, finished).
pub fn start(kind: Kind, sources: Vec<Path>, dest: Path, notify: Arc<dyn Fn() + Send + Sync>) -> Arc<Job> {
    let targets = sources.iter().map(|s| (s.clone(), fsys::join(&dest, fsys::base_name(s)))).collect();
    start_pairs(kind, targets, notify)
}

/// Like `start`, but each source names its own full destination path (used
/// by undo to put items back where they came from, under their old names).
pub fn start_pairs(kind: Kind, targets: Vec<(Path, Path)>, notify: Arc<dyn Fn() + Send + Sync>) -> Arc<Job> {
    let dest = targets.first().and_then(|(_, d)| fsys::parent(d)).unwrap_or_default();
    let job = Arc::new(Job {
        kind,
        dest,
        sources: targets.iter().map(|(s, _)| s.clone()).collect(),
        targets,
        created: Mutex::new(Vec::new()),
        phase: AtomicU8::new(Phase::Planning as u8),
        files_total: AtomicUsize::new(0),
        files_done: AtomicUsize::new(0),
        bytes_total: AtomicU64::new(0),
        bytes_done: AtomicU64::new(0),
        conflicts: AtomicUsize::new(0),
        threads: AtomicUsize::new(0),
        cancel: AtomicBool::new(false),
        errors: Mutex::new(Vec::new()),
        started: Instant::now(),
        elapsed: Mutex::new(None),
        decision: Mutex::new(None),
    });
    let j = job.clone();
    thread::spawn(move || {
        // A bug in the engine fails this job, not the whole application.
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&j, &*notify))) {
            let msg = panic.downcast_ref::<String>().map(String::as_str).or(panic.downcast_ref::<&str>().copied()).unwrap_or("unknown");
            j.error(format!("Internal error, job stopped: {msg}"));
            j.cancel.store(true, Relaxed);
        }
        *j.elapsed.lock().unwrap_or_else(PoisonError::into_inner) = Some(j.started.elapsed());
        j.set_phase(if j.cancelled() { Phase::Cancelled } else { Phase::Done });
        notify();
    });
    job
}

/// Split a name into stem and extension (`.bashrc` has none).
fn split_ext(name: &[u8]) -> (&[u8], &[u8]) {
    match memchr::memrchr(b'.', name) {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, b""),
    }
}

/// `name (2).ext`, `name (3).ext`, ... (or `name (copy).ext` style when
/// `copy_suffix` is set), first one not present in `dir`.
pub fn unique_name(dir: &[u8], name: &[u8], copy_suffix: bool) -> Path {
    let (stem, ext) = split_ext(name);
    (1..)
        .map(|n| {
            let mut v = stem.to_vec();
            match (copy_suffix, n) {
                (true, 1) => v.extend_from_slice(b" (copy)"),
                (true, n) => v.extend_from_slice(format!(" (copy {n})").as_bytes()),
                (false, 1) => return name.to_vec(),
                (false, n) => v.extend_from_slice(format!(" ({n})").as_bytes()),
            }
            v.extend_from_slice(ext);
            v
        })
        .find(|candidate| !fileop::exists(&fsys::join(dir, candidate)))
        .unwrap()
}

fn transfer_threads(src: &[u8], dst: &[u8]) -> usize {
    if let Some(n) = std::env::var("FEX_COPY_THREADS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0) {
        return n;
    }
    if fileop::is_network(src) || fileop::is_network(dst) {
        return 16;
    }
    let spinning = fileop::seek_penalty(src) == Some(true) || fileop::seek_penalty(dst) == Some(true);
    if spinning {
        // Parallel streams on one spindle just thrash the head.
        return if fileop::same_volume(src, dst) { 1 } else { 2 };
    }
    walk::cores().clamp(4, 16)
}

fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

fn run(job: &Job, notify: &dyn Fn()) {
    let Some(plan) = plan(job) else { return };
    if job.cancelled() {
        return;
    }

    let conflicts = plan.files.iter().filter(|f| f.existing.is_some()).count();
    job.conflicts.store(conflicts, Relaxed);
    let policy = if conflicts > 0 {
        let (tx, rx) = channel();
        *job.decision.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        job.set_phase(Phase::AwaitingDecision);
        notify();
        rx.recv().unwrap_or(Conflict::Cancel)
    } else {
        Conflict::Replace
    };
    if policy == Conflict::Cancel {
        job.cancel.store(true, Relaxed);
        return;
    }
    let Plan { renames, dirs, mut files, mut cleanup, trees } = plan;
    files.retain_mut(|f| match (f.existing, policy) {
        (None, _) => true,
        (Some(_), Conflict::Skip) => false,
        (Some((size, mtime)), Conflict::Update) => !(size == f.size && mtime == f.mtime),
        (Some(_), Conflict::KeepBoth) => {
            let dir = fsys::parent(&f.dst).unwrap_or_else(|| job.dest.clone());
            f.dst = fsys::join(&dir, &unique_name(&dir, fsys::base_name(&f.dst), false));
            f.existing = None;
            true
        }
        _ => true,
    });

    job.files_total.store(renames.len() + files.len(), Relaxed);
    job.bytes_total.store(files.iter().map(|f| f.size).sum(), Relaxed);
    job.set_phase(Phase::Transferring);
    notify();

    for (src, dst) in &renames {
        match fileop::rename(src, dst, false) {
            Ok(()) => job.record(src, dst),
            Err(e) => job.error(format!("{}: {}", lossy(src), sys::error_text(e))),
        }
        job.files_done.fetch_add(1, Relaxed);
    }

    let tree_roots: HashSet<&[u8]> = trees.iter().map(|(_, d)| d.as_slice()).collect();
    let mut dirs = dirs;
    dirs.sort_by_key(|(d, _)| d.len()); // parents before children
    for (d, mode) in &dirs {
        if job.cancelled() {
            return;
        }
        // Owner-writable, so the folder can be filled; less the umask, as cp does.
        match fileop::create_dir_mode(d, mode | 0o700) {
            // Recorded as soon as it exists, so even a cancelled copy can be undone.
            Ok(()) if tree_roots.contains(d.as_slice()) => {
                if let Some((src, dst)) = trees.iter().find(|(_, t)| t == d) {
                    job.record(src, dst);
                }
            }
            Ok(()) => {}
            Err(libc::EEXIST) if fileop::stat(d).is_some_and(|s| s.is_dir()) => {}
            Err(e) => job.error(format!("{}: {}", lossy(d), sys::error_text(e))),
        }
    }

    files.sort_unstable_by_key(|f| std::cmp::Reverse(f.size));
    let workers = transfer_threads(job.sources.first().map_or(&b""[..], Vec::as_slice), &job.dest).min(files.len()).max(1);
    job.threads.store(workers, Relaxed);
    let next = AtomicUsize::new(0);
    let delete_source = job.kind == Kind::Move;
    thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| {
                while let Some(f) = files.get(next.fetch_add(1, Relaxed)) {
                    if job.cancelled() {
                        break;
                    }
                    if transfer(job, f, delete_source) && f.top && f.existing.is_none() {
                        job.record(&f.src, &f.dst);
                    }
                    job.files_done.fetch_add(1, Relaxed);
                }
            });
        }
    });

    if delete_source && !job.cancelled() {
        // Non-empty folders (skipped or failed files) are left in place.
        cleanup.sort_by_key(|d| std::cmp::Reverse(d.len()));
        for d in &cleanup {
            let _ = fileop::remove_dir(d);
        }
    }
}

/// Transfer one file; true when it arrived at its destination.
fn transfer(job: &Job, f: &FileItem, delete_source: bool) -> bool {
    if f.rename {
        return match fileop::rename(&f.src, &f.dst, f.existing.is_some()) {
            Ok(()) => {
                job.bytes_done.fetch_add(f.size, Relaxed);
                true
            }
            Err(e) => {
                job.error(format!("{}: {}", lossy(&f.src), sys::error_text(e)));
                false
            }
        };
    }
    let mut reported = 0u64;
    let result = fileop::copy_file(&f.src, &f.dst, f.existing.is_some(), &job.cancel, &mut |done| {
        if done > reported {
            job.bytes_done.fetch_add(done - reported, Relaxed);
            reported = done;
        }
    });
    match result {
        Ok(()) => {
            // Files that copied without reporting progress (empty files, links).
            job.bytes_done.fetch_add(f.size.saturating_sub(reported), Relaxed);
            if delete_source && let Err(e) = fileop::delete_file(&f.src) {
                job.error(format!("{} (copied, but source not removed): {}", lossy(&f.src), sys::error_text(e)));
            }
            true
        }
        Err(e) => {
            job.bytes_done.fetch_sub(reported, Relaxed);
            if !job.cancelled() {
                let why = if e == libc::EOPNOTSUPP { "special files (devices, sockets, pipes) are not copied".into() } else { sys::error_text(e) };
                job.error(format!("{}: {why}", lossy(&f.src)));
            }
            false
        }
    }
}

fn plan(job: &Job) -> Option<Plan> {
    let mut plan = Plan::default();
    let network = fileop::is_network(&job.dest) || job.sources.first().is_some_and(|s| fileop::is_network(s));
    let workers = if network { 16 } else { walk::cores() };

    for (src, target) in &job.targets {
        if job.cancelled() {
            return None;
        }
        let Some(st) = fileop::stat(src) else {
            job.error(format!("{}: not found", lossy(src)));
            continue;
        };
        let Some(dest) = fsys::parent(target) else {
            job.error(format!("{}: not a valid destination", lossy(target)));
            continue;
        };
        let is_dir = st.is_dir();
        let same_dir = src == target;
        let name = fsys::base_name(target);

        if same_dir && job.kind == Kind::Move {
            continue; // moving into its own folder is a no-op
        }
        if is_dir && fsys::in_scope(&dest, src) {
            job.error(format!("Cannot {} '{}' into itself", if job.kind == Kind::Move { "move" } else { "copy" }, lossy(name)));
            continue;
        }
        let dst = if same_dir {
            fsys::join(&dest, &unique_name(&dest, name, true)) // "name (copy)"
        } else {
            target.clone()
        };
        let existing = fileop::stat(&dst);
        let rename = job.kind == Kind::Move && fileop::same_volume(src, &dest);

        if rename && existing.is_none() {
            plan.renames.push((src.clone(), dst));
            continue;
        }
        if !is_dir {
            plan.files.push(FileItem {
                src: src.clone(),
                dst,
                // A link's own size (its target's length), as merges compare it.
                size: st.size,
                mtime: st.mtime,
                existing: existing.map(|e| (e.size, e.mtime)),
                rename,
                top: true,
            });
            continue;
        }
        if existing.as_ref().is_some_and(|e| !e.is_dir()) {
            job.error(format!("Cannot replace file '{}' with a folder", lossy(&dst)));
            continue;
        }
        plan_tree(job, &mut plan, src, &dst, st.mode, existing.is_some(), rename, workers);
    }
    Some(plan)
}

/// Names in a destination folder, with their size and date.
type Listed = HashMap<Box<[u8]>, (u64, i64)>;

/// A planning worker's share of the plan, plus the last destination folder
/// it listed (for merges: one listing per folder instead of one lookup per file).
#[derive(Default)]
struct PlanPart {
    plan: Plan,
    listed: Option<(Path, Listed)>,
}

impl PlanPart {
    /// Size and date of `name` in `dir`, if it exists there.
    fn existing(&mut self, dir: &[u8], name: &[u8]) -> Option<(u64, i64)> {
        if self.listed.as_ref().is_none_or(|(d, _)| d != dir) {
            let mut found = HashMap::new();
            if let Ok(d) = sys::Dir::open(dir) {
                let mut names = Vec::new();
                let _ = d.scan(&mut Vec::new(), |e| names.push(Box::<[u8]>::from(e.name)));
                for n in names {
                    if let Some(m) = d.stat(&n) {
                        found.insert(n, (m.size, m.mtime));
                    }
                }
            }
            self.listed = Some((dir.to_vec(), found));
        }
        self.listed.as_ref().and_then(|(_, found)| found.get(name).copied())
    }
}

/// Expand folder `src` → `dst` in parallel. Existence checks are only
/// needed when merging into a folder that already existed.
#[allow(clippy::too_many_arguments)]
fn plan_tree(job: &Job, plan: &mut Plan, src: &[u8], dst: &[u8], mode: u32, merging: bool, rename: bool, workers: usize) {
    plan.dirs.push((dst.to_vec(), mode & 0o7777));
    if !merging {
        plan.trees.push((src.to_vec(), dst.to_vec()));
    }
    if job.kind == Kind::Move {
        plan.cleanup.push(src.to_vec());
    }
    let src_root = src.strip_suffix(b"/").unwrap_or(src);
    let (parts, failed) = walk::walk(&[Arc::from(src)], workers, &job.cancel, &HashSet::new(), false, &|| {}, PlanPart::default, |part: &mut PlanPart, dir, visit| {
        let (d, e) = match visit {
            Visit::Begin(_) => return,
            Visit::Unreadable => {
                job.error(format!("{}: could not be read", lossy(dir)));
                return;
            }
            Visit::Entry(d, e) => (d, e),
        };
        let rel = dir.get(src_root.len()..).unwrap_or(b"");
        let dst_dir = [dst, rel].concat();
        let s = fsys::join(dir, e.name);
        let t = fsys::join(&dst_dir, e.name);
        let meta = d.stat(e.name).unwrap_or_default();
        if e.flags & (DIR | LINK) == DIR {
            if job.kind == Kind::Move {
                part.plan.cleanup.push(s);
            }
            let mode = d.stat_full(e.name).map_or(0o755, |m| m.mode & 0o7777);
            part.plan.dirs.push((t, mode));
        } else {
            let existing = if merging { part.existing(&dst_dir, e.name) } else { None };
            part.plan.files.push(FileItem { src: s, dst: t, size: meta.size, mtime: meta.mtime, existing, rename, top: false });
        }
    });
    if failed > 0 {
        // Part of the tree was never listed: transferring the rest would
        // leave a silently incomplete copy (or half-moved folder).
        job.error(format!("Internal error while listing {}; nothing was transferred", lossy(src)));
        job.cancel();
        return;
    }
    for p in parts.into_iter().map(|part| part.plan) {
        plan.dirs.extend(p.dirs);
        plan.files.extend(p.files);
        plan.cleanup.extend(p.cleanup);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    fn scratch(name: &str) -> Path {
        let dir = std::env::temp_dir().join(format!("fex-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.as_os_str().as_bytes().to_vec()
    }

    fn p(root: &[u8], rel: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(sys::os(&fsys::join(root, rel.as_bytes())))
    }

    fn wait(job: &Arc<Job>) {
        while !job.finished() {
            if job.phase() == Phase::AwaitingDecision {
                job.resolve(Conflict::Update);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn tree(root: &[u8]) {
        fs::create_dir_all(p(root, "src/a/b")).unwrap();
        fs::write(p(root, "src/top.txt"), b"top").unwrap();
        fs::write(p(root, "src/a/mid.bin"), vec![7u8; 100_000]).unwrap();
        fs::write(p(root, "src/a/b/deep.txt"), b"deep").unwrap();
        fs::write(p(root, "src/a/b/empty"), b"").unwrap();
        std::os::unix::fs::symlink("../top.txt", p(root, "src/a/link")).unwrap();
        fs::create_dir_all(p(root, "dst")).unwrap();
    }

    fn noop() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    #[test]
    fn copy_tree_then_update() {
        let root = scratch("copy");
        tree(&root);
        let job = start(Kind::Copy, vec![fsys::join(&root, b"src")], fsys::join(&root, b"dst"), noop());
        wait(&job);
        assert!(job.errors.lock().unwrap().is_empty(), "{:?}", job.errors.lock().unwrap());
        assert_eq!(fs::read(p(&root, "dst/src/a/mid.bin")).unwrap().len(), 100_000);
        assert_eq!(fs::read(p(&root, "dst/src/a/b/deep.txt")).unwrap(), b"deep");
        // The link is copied as a link, still relative.
        assert_eq!(fs::read_link(p(&root, "dst/src/a/link")).unwrap().as_os_str(), "../top.txt");
        assert_eq!(job.bytes_done.load(Relaxed), job.bytes_total.load(Relaxed));
        assert_eq!(job.files_done.load(Relaxed), 5);

        // Second copy: all 5 conflict; Update skips the unchanged ones.
        fs::write(p(&root, "src/top.txt"), b"changed!").unwrap();
        let job = start(Kind::Copy, vec![fsys::join(&root, b"src")], fsys::join(&root, b"dst"), noop());
        wait(&job);
        assert_eq!(job.conflicts.load(Relaxed), 5);
        assert_eq!(job.files_total.load(Relaxed), 1);
        assert_eq!(fs::read(p(&root, "dst/src/top.txt")).unwrap(), b"changed!");
        let _ = fs::remove_dir_all(sys::os(&root));
    }

    #[test]
    fn move_is_rename_and_copy_into_same_dir_duplicates() {
        let root = scratch("move");
        tree(&root);
        let job = start(Kind::Move, vec![fsys::join(&root, b"src")], fsys::join(&root, b"dst"), noop());
        wait(&job);
        assert!(job.errors.lock().unwrap().is_empty());
        assert!(!p(&root, "src").exists());
        assert!(p(&root, "dst/src/a/b/deep.txt").exists());

        let job = start(Kind::Copy, vec![fsys::join(&root, b"dst/src/top.txt")], fsys::join(&root, b"dst/src"), noop());
        wait(&job);
        assert!(p(&root, "dst/src/top (copy).txt").exists());

        let job = start(Kind::Copy, vec![fsys::join(&root, b"dst/src")], fsys::join(&root, b"dst/src/a"), noop());
        wait(&job);
        assert_eq!(job.error_count(), 1, "copying a folder into itself must be refused");
        let _ = fs::remove_dir_all(sys::os(&root));
    }

    #[test]
    fn records_created_items_and_moves_back_by_pairs() {
        let root = scratch("created");
        tree(&root);
        fs::write(p(&root, "dst/top.txt"), b"already here").unwrap();
        let sources = vec![fsys::join(&root, b"src/a"), fsys::join(&root, b"src/top.txt")];
        let job = start(Kind::Move, sources, fsys::join(&root, b"dst"), noop());
        wait(&job); // Update: top.txt differs, so it replaces the existing file
        let created = job.created.lock().unwrap().clone();
        // The replaced file was not created by the job, so undo must not touch it.
        assert_eq!(created, vec![(fsys::join(&root, b"src/a"), fsys::join(&root, b"dst/a"))]);

        let back: Vec<_> = created.into_iter().map(|(src, dst)| (dst, src)).collect();
        let job = start_pairs(Kind::Move, back, noop());
        wait(&job);
        assert!(p(&root, "src/a/b/deep.txt").exists());
        assert!(!p(&root, "dst/a").exists());
        let _ = fs::remove_dir_all(sys::os(&root));
    }

    #[test]
    fn unique_names() {
        let root = scratch("unique");
        fs::write(p(&root, "a.txt"), b"").unwrap();
        assert_eq!(unique_name(&root, b"a.txt", false), b"a (2).txt");
        assert_eq!(unique_name(&root, b"b.txt", false), b"b.txt");
        assert_eq!(unique_name(&root, b".rc", true), b".rc (copy)");
        let _ = fs::remove_dir_all(sys::os(&root));
    }
}
