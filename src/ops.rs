//! Multithreaded copy/move engine.
//!
//! A job runs in three steps:
//! 1. **Plan:** expand the sources into directories to create and files to
//!    transfer (a parallel walk). A move within one volume that doesn't
//!    collide with anything is a single rename, however large the tree.
//! 2. **Resolve conflicts:** if destination files already exist, the user picks
//!    once per job: replace, skip, update (rsync's quick check: skip when
//!    size and modification time match), or keep both.
//! 3. **Transfer:** files go to a worker pool largest-first so the slowest
//!    files start early. Rotational disks get few workers, SSDs more, and
//!    network paths the most (per-file latency dominates there).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::fileop;
use crate::fsys;
use crate::index::in_scope;
use crate::walk;
use crate::win;

/// Files at least this large are copied unbuffered, so they don't evict the
/// whole file cache.
const UNBUFFERED_MIN: u64 = 256 * 1024 * 1024;

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
    pub dest: String,
    pub sources: Vec<String>,
    /// Each source and the full path it goes to.
    targets: Vec<(String, String)>,
    /// Top-level items this job created, as (source, destination). Only items
    /// that didn't exist before are listed, so undo never touches files that
    /// were merged into or replaced.
    pub created: Mutex<Vec<(String, String)>>,
    phase: AtomicU8,
    pub files_total: AtomicUsize,
    pub files_done: AtomicUsize,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub conflicts: AtomicUsize,
    pub threads: AtomicUsize,
    cancel: AtomicI32, // BOOL read by CopyFileEx mid-file
    cancel_walk: AtomicBool,
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
        self.cancel.store(1, Relaxed);
        self.cancel_walk.store(true, Relaxed);
        self.resolve(Conflict::Cancel);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed) != 0
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

    fn record(&self, src: &str, dst: &str) {
        self.created.lock().unwrap_or_else(PoisonError::into_inner).push((src.to_string(), dst.to_string()));
    }
}

/// One file to transfer.
struct FileItem {
    src: String,
    dst: String,
    size: u64,
    mtime: u64,
    /// Destination already exists: (size, mtime).
    existing: Option<(u64, u64)>,
    /// Same-volume move: rename instead of copy.
    rename: bool,
    /// One of the job's sources itself, not something inside a folder.
    top: bool,
}

#[derive(Default)]
struct Plan {
    /// Whole-tree renames that collide with nothing.
    renames: Vec<(String, String)>,
    dirs: Vec<String>,
    files: Vec<FileItem>,
    /// Source dirs to remove after a move, children before parents.
    cleanup: Vec<String>,
    /// Top-level folders created fresh at the destination: (source, destination).
    trees: Vec<(String, String)>,
}

/// Start a job on a background thread. `notify` is called when the job needs
/// attention (decision required, finished).
pub fn start(kind: Kind, sources: Vec<String>, dest: String, notify: Arc<dyn Fn() + Send + Sync>) -> Arc<Job> {
    let targets = sources.iter().map(|s| (s.clone(), fsys::join(&dest, file_name(s)))).collect();
    start_pairs(kind, targets, notify)
}

/// Like `start`, but each source names its own full destination path (used
/// by undo to put items back where they came from, under their old names).
pub fn start_pairs(kind: Kind, targets: Vec<(String, String)>, notify: Arc<dyn Fn() + Send + Sync>) -> Arc<Job> {
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
        cancel: AtomicI32::new(0),
        cancel_walk: AtomicBool::new(false),
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
            j.errors.lock().unwrap_or_else(|e| e.into_inner()).push(format!("Internal error, job stopped: {msg}"));
            j.cancel.store(1, Relaxed);
        }
        *j.elapsed.lock().unwrap_or_else(PoisonError::into_inner) = Some(j.started.elapsed());
        j.set_phase(if j.cancelled() { Phase::Cancelled } else { Phase::Done });
        notify();
    });
    job
}

fn file_name(path: &str) -> &str {
    let t = path.trim_end_matches('\\');
    t.rsplit('\\').next().unwrap_or(t).trim_end_matches(':')
}

/// `name (2).ext`, `name (3).ext`, ... (or Explorer's `name - Copy.ext`
/// style when `copy_suffix` is set), first one not present in `dir`.
pub fn unique_name(dir: &str, name: &str, copy_suffix: bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| match (copy_suffix, n) {
            (true, 1) => format!("{stem} - Copy{ext}"),
            (true, n) => format!("{stem} - Copy ({n}){ext}"),
            (false, 1) => name.to_string(),
            (false, n) => format!("{stem} ({n}){ext}"),
        })
        .find(|candidate| fileop::stat(&fsys::join(dir, candidate)).is_none())
        .unwrap()
}

fn transfer_threads(src: &str, dst: &str) -> usize {
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
        job.cancel.store(1, Relaxed);
        return;
    }
    let Plan { renames, dirs, mut files, mut cleanup, trees } = plan;
    files.retain_mut(|f| match (f.existing, policy) {
        (None, _) => true,
        (Some(_), Conflict::Skip) => false,
        (Some((size, mtime)), Conflict::Update) => !(size == f.size && mtime == f.mtime),
        (Some(_), Conflict::KeepBoth) => {
            let (dir, name) = f.dst.rsplit_once('\\').unwrap_or((&job.dest, &f.dst));
            f.dst = fsys::join(dir, &unique_name(dir, name, false));
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
            Err(e) => job.error(format!("{src}: {}", win::error_text(e))),
        }
        job.files_done.fetch_add(1, Relaxed);
    }

    let tree_roots: HashSet<String> = trees.iter().map(|(_, d)| d.to_lowercase()).collect();
    let mut dirs = dirs;
    dirs.sort_by_key(|d| d.len()); // parents before children
    for d in &dirs {
        if job.cancelled() {
            return;
        }
        match fileop::create_dir(d) {
            // Recorded as soon as it exists, so even a cancelled copy can be undone.
            Ok(()) if tree_roots.contains(&d.to_lowercase()) => {
                if let Some((src, dst)) = trees.iter().find(|(_, t)| t.eq_ignore_ascii_case(d)) {
                    job.record(src, dst);
                }
            }
            Ok(()) => {}
            Err(e) => job.error(format!("{d}: {}", win::error_text(e))),
        }
    }

    files.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    let workers = transfer_threads(job.sources.first().map_or("", String::as_str), &job.dest).min(files.len()).max(1);
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
        // Non-empty dirs (skipped or failed files) are left in place.
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
                job.error(format!("{}: {}", f.src, win::error_text(e)));
                false
            }
        };
    }
    let mut reported = 0u64;
    let result = fileop::copy_file(&f.src, &f.dst, f.size >= UNBUFFERED_MIN, f.existing.is_none(), &job.cancel, &mut |done| {
        if done > reported {
            job.bytes_done.fetch_add(done - reported, Relaxed);
            reported = done;
        }
    });
    match result {
        Ok(()) => {
            // Count files CopyFileEx finished without reporting progress (empty files).
            job.bytes_done.fetch_add(f.size.saturating_sub(reported), Relaxed);
            if delete_source {
                if let Err(e) = fileop::delete_file(&f.src) {
                    job.error(format!("{} (copied, but source not removed): {}", f.src, win::error_text(e)));
                }
            }
            true
        }
        Err(e) => {
            job.bytes_done.fetch_sub(reported, Relaxed);
            if !job.cancelled() {
                job.error(format!("{}: {}", f.src, win::error_text(e)));
            }
            false
        }
    }
}

fn plan(job: &Job) -> Option<Plan> {
    let mut plan = Plan::default();
    let network = fileop::is_network(&job.dest) || job.targets.iter().any(|(s, d)| s.starts_with(r"\\") || d.starts_with(r"\\"));
    let workers = if network { 16 } else { walk::cores() };

    for (src, target) in &job.targets {
        if job.cancelled() {
            return None;
        }
        let Some(st) = fileop::stat(src) else {
            job.error(format!("{src}: not found"));
            continue;
        };
        let Some(dest) = fsys::parent(target) else {
            job.error(format!("{target}: not a valid destination"));
            continue;
        };
        let dest = dest.as_str();
        let is_dir = st.attrs & win::ATTR_DIR != 0;
        let same_dir = src.trim_end_matches('\\').eq_ignore_ascii_case(target.trim_end_matches('\\'));
        let name = file_name(target);

        if same_dir && job.kind == Kind::Move {
            continue; // moving into its own folder is a no-op
        }
        if is_dir && in_scope(dest, src) {
            job.error(format!("Cannot {} '{name}' into itself", if job.kind == Kind::Move { "move" } else { "copy" }));
            continue;
        }
        let dst = if same_dir {
            fsys::join(dest, &unique_name(dest, name, true)) // Explorer-style "name - Copy"
        } else {
            target.clone()
        };
        let existing = fileop::stat(&dst);
        let rename = job.kind == Kind::Move && fileop::same_volume(src, dest);

        if rename && existing.is_none() {
            plan.renames.push((src.clone(), dst));
            continue;
        }
        if !is_dir {
            plan.files.push(FileItem {
                src: src.clone(),
                dst,
                size: st.size,
                mtime: st.mtime,
                existing: existing.map(|e| (e.size, e.mtime)),
                rename,
                top: true,
            });
            continue;
        }
        if existing.as_ref().is_some_and(|e| e.attrs & win::ATTR_DIR == 0) {
            job.error(format!("Cannot replace file '{dst}' with a folder"));
            continue;
        }
        // A junction or folder symlink is only ever moved as the link itself
        // (a rename, above). Walking into it would move, and so delete, the
        // files of the folder it points to.
        if job.kind == Kind::Move && st.attrs & win::ATTR_REPARSE != 0 {
            job.error(format!(
                "{src}: a junction or symbolic link can only be moved within its drive, to a name that isn't taken"
            ));
            continue;
        }
        plan_tree(job, &mut plan, src, &dst, existing.is_some(), rename, workers);
    }
    Some(plan)
}

/// A planning worker's share of the plan, plus the last destination folder
/// it listed (for merges: one listing per folder instead of one lookup per file).
#[derive(Default)]
struct PlanPart {
    plan: Plan,
    listed: Option<(String, HashMap<String, (u64, u64)>)>,
}

impl PlanPart {
    /// Size and date of `name` in `dir`, if it exists there.
    fn existing(&mut self, dir: &str, name: &str) -> Option<(u64, u64)> {
        if self.listed.as_ref().is_none_or(|(d, _)| d != dir) {
            let mut found = HashMap::new();
            let mut entry_name = String::new();
            let _ = win::scan_dir(dir, |e| {
                entry_name.clear();
                win::push_utf16(&mut entry_name, e.name);
                found.insert(entry_name.to_lowercase(), (e.size, e.mtime));
            });
            self.listed = Some((dir.to_string(), found));
        }
        self.listed.as_ref().and_then(|(_, found)| found.get(&name.to_lowercase()).copied())
    }
}

/// Expand directory `src` → `dst` in parallel. Existence checks are only
/// needed when merging into a directory that already existed.
fn plan_tree(job: &Job, plan: &mut Plan, src: &str, dst: &str, merging: bool, rename: bool, workers: usize) {
    plan.dirs.push(dst.to_string());
    if !merging {
        plan.trees.push((src.to_string(), dst.to_string()));
    }
    if job.kind == Kind::Move {
        plan.cleanup.push(src.to_string());
    }
    let src_root = src.trim_end_matches('\\');
    let (parts, failed) = walk::walk(
        &[Arc::from(src)],
        workers,
        &job.cancel_walk,
        PlanPart::default,
        |part: &mut PlanPart, dir, entry| {
            let (name, e) = match entry {
                Ok(x) => x,
                Err(code) => {
                    job.error(format!("{dir}: {}", win::error_text(code)));
                    return;
                }
            };
            let rel = dir.get(src_root.len()..).unwrap_or("");
            let dst_dir = format!("{dst}{rel}");
            let s = fsys::join(dir, name);
            let d = fsys::join(&dst_dir, name);
            if e.attrs & win::ATTR_DIR != 0 {
                if e.attrs & win::ATTR_REPARSE != 0 {
                    job.error(format!("{s}: skipped junction/symbolic link"));
                    return;
                }
                if job.kind == Kind::Move {
                    part.plan.cleanup.push(s);
                }
                part.plan.dirs.push(d);
            } else {
                let existing = if merging { part.existing(&dst_dir, name) } else { None };
                part.plan.files.push(FileItem { src: s, dst: d, size: e.size, mtime: e.mtime, existing, rename, top: false });
            }
        },
    );
    if failed > 0 {
        // Part of the tree was never listed: transferring the rest would
        // leave a silently incomplete copy (or half-moved folder).
        job.error(format!("Internal error while listing {src}; nothing was transferred"));
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

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("fex-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn wait(job: &Arc<Job>) {
        while !job.finished() {
            if job.phase() == Phase::AwaitingDecision {
                job.resolve(Conflict::Update);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn tree(root: &str) {
        fs::create_dir_all(format!(r"{root}\src\a\b")).unwrap();
        fs::write(format!(r"{root}\src\top.txt"), b"top").unwrap();
        fs::write(format!(r"{root}\src\a\mid.bin"), vec![7u8; 100_000]).unwrap();
        fs::write(format!(r"{root}\src\a\b\deep.txt"), b"deep").unwrap();
        fs::write(format!(r"{root}\src\a\b\empty"), b"").unwrap();
        fs::create_dir_all(format!(r"{root}\dst")).unwrap();
    }

    #[test]
    fn copy_tree_then_update() {
        let root = scratch("copy");
        tree(&root);
        let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let job = start(Kind::Copy, vec![format!(r"{root}\src")], format!(r"{root}\dst"), noop.clone());
        wait(&job);
        assert!(job.errors.lock().unwrap().is_empty(), "{:?}", job.errors.lock().unwrap());
        assert_eq!(fs::read(format!(r"{root}\dst\src\a\mid.bin")).unwrap().len(), 100_000);
        assert_eq!(fs::read(format!(r"{root}\dst\src\a\b\deep.txt")).unwrap(), b"deep");
        assert_eq!(job.bytes_done.load(Relaxed), job.bytes_total.load(Relaxed));
        assert_eq!(job.files_done.load(Relaxed), 4);

        // Second copy: all 4 files conflict; Update skips the unchanged ones.
        fs::write(format!(r"{root}\src\top.txt"), b"changed!").unwrap();
        let job = start(Kind::Copy, vec![format!(r"{root}\src")], format!(r"{root}\dst"), noop);
        wait(&job);
        assert_eq!(job.conflicts.load(Relaxed), 4);
        assert_eq!(job.files_total.load(Relaxed), 1);
        assert_eq!(fs::read(format!(r"{root}\dst\src\top.txt")).unwrap(), b"changed!");
        let _ = fs::remove_dir_all(&root);
    }

    /// `cargo test --release -- --ignored --nocapture copy_throughput`
    /// Mixed workload: 4000 x 16 KB files + 4 x 64 MB files, copied with 1
    /// thread and with the automatic thread count.
    #[test]
    #[ignore]
    fn copy_throughput() {
        let root = scratch("bench");
        fs::create_dir_all(format!(r"{root}\src\small")).unwrap();
        let small = vec![1u8; 16 * 1024];
        for i in 0..4000 {
            fs::write(format!(r"{root}\src\small\f{i}.bin"), &small).unwrap();
        }
        let big = vec![2u8; 64 * 1024 * 1024];
        for i in 0..4 {
            fs::write(format!(r"{root}\src\big{i}.bin"), &big).unwrap();
        }
        let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        for (label, threads) in [("1 thread", Some("1")), ("auto", None)] {
            match threads {
                Some(n) => unsafe { std::env::set_var("FEX_COPY_THREADS", n) },
                None => unsafe { std::env::remove_var("FEX_COPY_THREADS") },
            }
            let dst = format!(r"{root}\dst-{}", label.replace(' ', ""));
            fs::create_dir_all(&dst).unwrap();
            let job = start(Kind::Copy, vec![format!(r"{root}\src")], dst, noop.clone());
            wait(&job);
            let t = job.elapsed.lock().unwrap().unwrap();
            let mb = job.bytes_done.load(Relaxed) as f64 / 1048576.0;
            println!(
                "{label:>9}: {} files, {mb:.0} MB in {t:.2?} ({:.0} MB/s, {:.0} files/s, {} threads)",
                job.files_done.load(Relaxed),
                mb / t.as_secs_f64(),
                job.files_done.load(Relaxed) as f64 / t.as_secs_f64(),
                job.threads.load(Relaxed)
            );
            assert_eq!(job.error_count(), 0);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_is_rename_and_copy_into_same_dir_duplicates() {
        let root = scratch("move");
        tree(&root);
        let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let job = start(Kind::Move, vec![format!(r"{root}\src")], format!(r"{root}\dst"), noop.clone());
        wait(&job);
        assert!(job.errors.lock().unwrap().is_empty());
        assert!(!fs::exists(format!(r"{root}\src")).unwrap());
        assert!(fs::exists(format!(r"{root}\dst\src\a\b\deep.txt")).unwrap());

        let job = start(Kind::Copy, vec![format!(r"{root}\dst\src\top.txt")], format!(r"{root}\dst\src"), noop.clone());
        wait(&job);
        assert!(fs::exists(format!(r"{root}\dst\src\top - Copy.txt")).unwrap());

        let job = start(Kind::Copy, vec![format!(r"{root}\dst\src")], format!(r"{root}\dst\src\a"), noop);
        wait(&job);
        assert_eq!(job.error_count(), 1, "copying a folder into itself must be refused");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn records_created_items_and_moves_back_by_pairs() {
        let root = scratch("created");
        tree(&root);
        fs::write(format!(r"{root}\dst\top.txt"), b"already here").unwrap();
        let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let sources = vec![format!(r"{root}\src\a"), format!(r"{root}\src\top.txt")];
        let job = start(Kind::Move, sources, format!(r"{root}\dst"), noop.clone());
        wait(&job); // Update: top.txt differs, so it replaces the existing file
        let created = job.created.lock().unwrap().clone();
        // The replaced file was not created by the job, so undo must not touch it.
        assert_eq!(created, vec![(format!(r"{root}\src\a"), format!(r"{root}\dst\a"))]);

        let back: Vec<_> = created.into_iter().map(|(src, dst)| (dst, src)).collect();
        let job = start_pairs(Kind::Move, back, noop);
        wait(&job);
        assert!(fs::exists(format!(r"{root}\src\a\b\deep.txt")).unwrap());
        assert!(!fs::exists(format!(r"{root}\dst\a")).unwrap());
        let _ = fs::remove_dir_all(&root);
    }
}
