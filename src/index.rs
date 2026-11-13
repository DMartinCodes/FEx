//! Parallel filesystem crawler and in-memory name index.
//!
//! Layout is chosen for cache behaviour during queries:
//! * Each crawler thread owns a `Shard`, so building needs no shared writes.
//! * All names of a shard live in one contiguous `String`, separated by `\n`,
//!   plus an ASCII-lowercased twin with the same offsets. A plain keyword is
//!   found in the twin with SIMD `memmem`; other queries run the regex over
//!   the arena directly (one long haystack lets the regex engine use its
//!   literal prefilters) instead of calling it once per tiny name.
//! * Name offsets (4 bytes/entry) are separate from the cold metadata, so the
//!   hot scan touches only the arena and the offset array.
//! * Adjacent folder ranges are scanned as one run, split into 32K-entry
//!   units spread over a persistent thread pool.
//!
//! An index stays fresh while it is in use: a watcher on its whole tree
//! (started when the crawl starts, so nothing during the crawl is missed)
//! re-reads changed folders. The new listing is appended and the old one
//! marked dead, which queries skip; garbage is compacted away once it
//! outweighs the live entries. Indexes are saved under
//! `%LOCALAPPDATA%\FEx\index`, so a later session can search at once while a
//! fresh crawl runs.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

use memchr::memmem;
use regex::{Regex, RegexBuilder};

use crate::fileop;
use crate::fsys::{self, Row};
use crate::walk::{self, Queue};
use crate::{pool, watch, win};

/// Compiled search query. In keyword mode every whitespace-separated term
/// must match (`*`/`?` act as wildcards); in regex mode the whole query is one
/// pattern. Matching is case-insensitive, like Windows filenames.
pub struct Matcher {
    /// Most selective pattern; drives the arena scan.
    primary: Regex,
    /// `primary` as a lowercase literal, when it is a plain ASCII keyword.
    literal: Option<memmem::Finder<'static>>,
    rest: Vec<Regex>,
    extension: Option<String>,
}

impl Matcher {
    pub fn new(query: &str, regex_mode: bool) -> Result<Option<Matcher>, String> {
        let mut terms = Vec::new();
        let mut extension: Option<String> = None;
        for term in query.split_whitespace() {
            if !term.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("type:")) {
                terms.push(term);
                continue;
            }
            let value = term[5..].trim_start_matches('.');
            if value.is_empty() || value.contains(['\\', '/', '*', '?']) {
                return Err("file type filter requires an extension, e.g. type:bat".into());
            }
            if extension.as_ref().is_some_and(|old| !old.eq_ignore_ascii_case(value)) {
                return Err("specify only one file type filter".into());
            }
            extension = Some(value.to_string());
        }
        let pattern_query = terms.join(" ");
        // (pattern, plain lowercase literal it is equivalent to)
        let pats: Vec<(String, Option<String>)> = if regex_mode {
            if pattern_query.is_empty() { vec![] } else { vec![(pattern_query, None)] }
        } else {
            terms
                .into_iter()
                .map(|t| {
                    let plain = (!t.contains(['*', '?']) && t.is_ascii()).then(|| t.to_ascii_lowercase());
                    (keyword_pattern(t), plain)
                })
                .collect()
        };
        if pats.is_empty() && extension.is_none() {
            return Ok(None);
        }
        let mut pats = if pats.is_empty() { vec![(".".to_string(), None)] } else { pats };
        let best = (0..pats.len()).max_by_key(|&i| pats[i].0.len()).unwrap_or(0);
        let literal = pats[best].1.take().map(|t| memmem::Finder::new(t.as_bytes()).into_owned());
        let mut res = pats
            .iter()
            .map(|(p, _)| {
                RegexBuilder::new(p)
                    .case_insensitive(true)
                    .multi_line(true)
                    .build()
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let primary = res.swap_remove(best);
        Ok(Some(Matcher { primary, literal, rest: res, extension }))
    }

    #[inline]
    pub fn is_match(&self, name: &str) -> bool {
        self.primary.is_match(name)
            && self.rest.iter().all(|r| r.is_match(name))
            && self.extension.as_ref().is_none_or(|ext| {
                name.rsplit_once('.').is_some_and(|(_, actual)| actual.eq_ignore_ascii_case(ext))
            })
    }

    pub fn is_match_entry(&self, name: &str, is_dir: bool) -> bool {
        !(is_dir && self.extension.is_some()) && self.is_match(name)
    }
}

fn keyword_pattern(k: &str) -> String {
    if !k.contains(['*', '?']) {
        return regex::escape(k);
    }
    // Glob semantics: anchored, `*` = any run, `?` = any one char.
    let mut p = String::from("^");
    for c in k.chars() {
        match c {
            '*' => p.push_str(".*"),
            '?' => p.push('.'),
            c => p.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    p.push('$');
    p
}

#[derive(Clone, Copy)]
struct Meta {
    dir: u32,
    attrs: u32,
    size: u64,
    mtime: u64,
}

#[inline]
fn is_subdir(attrs: u32) -> bool {
    attrs & win::ATTR_DIR != 0 && attrs & win::ATTR_REPARSE == 0
}

#[derive(Default)]
struct Shard {
    /// `name0\nname1\n...`
    names: String,
    /// `names` with ASCII letters lowercased (same byte offsets).
    lower: String,
    /// Byte offset of each name in `names`.
    starts: Vec<u32>,
    meta: Vec<Meta>,
    dirs: Vec<Arc<str>>,
    ranges: Vec<(usize, usize)>,
    /// Folders whose listing was replaced or removed; queries skip them.
    dead: Vec<bool>,
}

impl Shard {
    fn push(&mut self, name: &str, meta: Meta) -> bool {
        if self.names.len() + name.len() + 1 > u32::MAX as usize {
            return false;
        }
        self.starts.push(self.names.len() as u32);
        let at = self.names.len();
        if name.contains('\n') {
            self.names.push_str(&name.replace('\n', "\u{FFFD}"));
        } else {
            self.names.push_str(name);
        }
        self.names.push('\n');
        self.lower.push_str(&self.names[at..]);
        self.lower[at..].make_ascii_lowercase();
        self.meta.push(meta);
        true
    }

    /// Start the listing of `dir`; entries pushed until `end_dir` belong to it.
    fn begin_dir(&mut self, dir: Arc<str>) -> u32 {
        let id = self.dirs.len() as u32;
        self.dirs.push(dir);
        self.dead.push(false);
        self.ranges.push((self.meta.len(), self.meta.len()));
        id
    }

    fn end_dir(&mut self) {
        let end = self.meta.len();
        if let Some(r) = self.ranges.last_mut() {
            r.1 = end;
        }
    }

    #[inline]
    fn is_dead(&self, dir: usize) -> bool {
        self.dead.get(dir).copied().unwrap_or(false)
    }

    #[inline]
    fn end_of(&self, i: usize) -> usize {
        self.starts.get(i).map_or(self.names.len(), |&s| s as usize)
    }

    #[inline]
    fn name(&self, i: usize) -> &str {
        &self.names[self.starts[i] as usize..self.end_of(i + 1) - 1]
    }

    fn row(&self, i: usize) -> Row {
        let m = self.meta[i];
        Row {
            name: self.name(i).into(),
            dir: Some(self.dirs[m.dir as usize].clone()),
            size: m.size,
            mtime: m.mtime,
            attrs: m.attrs,
            sel: false,
        }
    }

    /// Report every entry in `a..b` whose name matches.
    fn scan(&self, m: &Matcher, a: usize, b: usize, mut hit: impl FnMut(usize)) {
        let end = self.end_of(b);
        // Haystack keeps the preceding bytes so `^` sees the right context.
        let hay = &self.names[..end];
        let lower = &self.lower.as_bytes()[..end];
        let mut pos = self.starts[a] as usize;
        while pos < end {
            let found = match &m.literal {
                Some(f) => match f.find(&lower[pos..]) {
                    Some(at) => pos + at,
                    None => break,
                },
                None => match m.primary.find_at(hay, pos) {
                    Some(found) => found.start(),
                    None => break,
                },
            };
            let i = a + self.starts[a..b].partition_point(|&s| s as usize <= found) - 1;
            // Re-check against the bare name: rejects matches that spanned a
            // `\n` separator and applies the remaining keywords.
            if m.is_match_entry(self.name(i), self.meta[i].attrs & win::ATTR_DIR != 0) {
                hit(i);
            }
            pos = self.end_of(i + 1);
        }
    }
}

/// `path` equals `root` or lies beneath it (ASCII case-insensitive).
pub fn in_scope(path: &str, root: &str) -> bool {
    let (p, r) = (path.trim_end_matches('\\'), root.trim_end_matches('\\'));
    p.len() >= r.len()
        && p.is_char_boundary(r.len())
        && p[..r.len()].eq_ignore_ascii_case(r)
        && (p.len() == r.len() || p.as_bytes()[r.len()] == b'\\')
}

/// Folder lookup key: lowercase, no trailing separator.
fn key(path: &str) -> Box<str> {
    path.trim_end_matches('\\').to_lowercase().into()
}

pub struct Index {
    pub root: Arc<str>,
    shards: Vec<Shard>,
    /// Folder key → (shard, folder id) of its current listing.
    dirs: HashMap<Box<str>, (u32, u32)>,
    /// Shard that receives re-read folders, once there is one.
    overlay: Option<usize>,
    /// Entries in dead folder ranges.
    dead_entries: usize,
    pub entries: usize,
    pub errors: usize,
    pub worker_errors: usize,
    pub built_at: Instant,
    pub build_time: Duration,
    /// Read from disk; may be out of date until a fresh crawl replaces it.
    pub loaded: bool,
    /// Changed since it was last saved.
    pub dirty: bool,
}

/// Which folders' entries a query may report. The index service implements
/// this per client, so users only see names in folders they could list.
pub trait Visibility: Sync {
    /// Runs on each worker thread before it checks anything (to take on the
    /// client's identity); dropping the result undoes it.
    fn enter(&self) -> Option<Box<dyn std::any::Any>> {
        None
    }
    /// Whether entries of folder `dir` (a global id below `dir_slots`, at
    /// `path`) may be reported.
    fn visible(&self, dir: usize, path: &str) -> bool;
    /// False when everything is visible, so queries skip the checks.
    fn filters(&self) -> bool {
        true
    }
}

/// No filtering (the app's own indexes).
pub struct Everything;

impl Visibility for Everything {
    fn visible(&self, _: usize, _: &str) -> bool {
        true
    }
    fn filters(&self) -> bool {
        false
    }
}

/// Entries per query work unit: big enough to amortize dispatch, small
/// enough to balance across cores.
const CHUNK: usize = 32 * 1024;

/// An index shared between queries (readers) and its change watcher (writer).
pub type Shared = Arc<RwLock<Index>>;

pub fn read(index: &RwLock<Index>) -> RwLockReadGuard<'_, Index> {
    index.read().unwrap_or_else(PoisonError::into_inner)
}

/// For the UI thread, which must never wait: `None` while a writer holds or
/// awaits the lock (applying changes, compacting).
pub fn try_read(index: &RwLock<Index>) -> Option<RwLockReadGuard<'_, Index>> {
    match index.try_read() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

pub fn write(index: &RwLock<Index>) -> RwLockWriteGuard<'_, Index> {
    index.write().unwrap_or_else(PoisonError::into_inner)
}

impl Index {
    fn from_shards(root: Arc<str>, shards: Vec<Shard>, errors: usize, worker_errors: usize, build_time: Duration, loaded: bool) -> Index {
        let mut dirs = HashMap::new();
        let mut entries = 0;
        for (s, shard) in shards.iter().enumerate() {
            for (d, path) in shard.dirs.iter().enumerate() {
                dirs.insert(key(path), (s as u32, d as u32));
            }
            entries += shard.meta.len();
        }
        Index {
            root,
            shards,
            dirs,
            overlay: None,
            dead_entries: 0,
            entries,
            errors,
            worker_errors,
            built_at: Instant::now(),
            build_time,
            loaded,
            dirty: false,
        }
    }

    /// Work units `(shard, first, end)`: runs of adjacent live folder ranges
    /// in scope, cut into `CHUNK`-entry pieces.
    fn query_units(&self, scope: &str) -> Vec<(usize, usize, usize)> {
        fn flush(units: &mut Vec<(usize, usize, usize)>, shard: usize, run: Option<(usize, usize)>) {
            if let Some((a, b)) = run {
                units.extend((a..b).step_by(CHUNK).map(|x| (shard, x, (x + CHUNK).min(b))));
            }
        }
        let everything = in_scope(&self.root, scope);
        let mut units = Vec::new();
        for (si, shard) in self.shards.iter().enumerate() {
            let mut run: Option<(usize, usize)> = None;
            for (d, &(start, end)) in shard.ranges.iter().enumerate() {
                if start == end || shard.is_dead(d) || !(everything || in_scope(&shard.dirs[d], scope)) {
                    continue;
                }
                match run {
                    Some((a, b)) if b == start => run = Some((a, end)),
                    _ => {
                        flush(&mut units, si, run);
                        run = Some((start, end));
                    }
                }
            }
            flush(&mut units, si, run);
        }
        units
    }

    /// Search entries under `scope` in parallel. Returns up to `limit` rows
    /// plus the total match count, or `None` if `cancelled` fired.
    pub fn query(
        &self,
        m: &Matcher,
        scope: &str,
        limit: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<(Vec<Row>, usize)> {
        self.query_as(m, scope, limit, cancelled, &Everything)
    }

    /// Paths of every folder with a live listing.
    pub fn folder_paths(&self) -> Vec<Arc<str>> {
        self.dirs.values().map(|&(s, d)| self.shards[s as usize].dirs[d as usize].clone()).collect()
    }

    /// Number of global folder ids (live and dead), for per-query caches.
    pub fn dir_slots(&self) -> usize {
        self.shards.iter().map(|s| s.dirs.len()).sum()
    }

    /// `query` reporting (and counting) only entries of folders `vis`
    /// allows (the index service filters per client this way).
    pub fn query_as(
        &self,
        m: &Matcher,
        scope: &str,
        limit: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
        vis: &dyn Visibility,
    ) -> Option<(Vec<Row>, usize)> {
        let units = self.query_units(scope);
        let mut base = Vec::with_capacity(self.shards.len());
        let mut acc = 0;
        for s in &self.shards {
            base.push(acc);
            acc += s.dirs.len();
        }
        let filters = vis.filters();
        let total = AtomicUsize::new(0);
        let retained = AtomicUsize::new(0);
        let aborted = AtomicBool::new(false);
        let parts: Vec<Mutex<Vec<(usize, Row)>>> = units.iter().map(|_| Mutex::new(Vec::new())).collect();

        pool::for_each(units.len(), &|u| {
            if aborted.load(Relaxed) || cancelled() {
                aborted.store(true, Relaxed);
                return;
            }
            let _guard = if filters { vis.enter() } else { None };
            let (si, a, b) = units[u];
            let shard = &self.shards[si];
            let mut out = Vec::new();
            let mut count = 0;
            let mut last: Option<(u32, bool)> = None;
            shard.scan(m, a, b, |i| {
                if filters {
                    let d = shard.meta[i].dir;
                    let ok = match last {
                        Some((ld, ok)) if ld == d => ok,
                        _ => {
                            let ok = vis.visible(base[si] + d as usize, &shard.dirs[d as usize]);
                            last = Some((d, ok));
                            ok
                        }
                    };
                    if !ok {
                        return;
                    }
                }
                count += 1;
                if retained.load(Relaxed) < limit {
                    let slot = retained.fetch_add(1, Relaxed);
                    if slot < limit {
                        out.push((slot, shard.row(i)));
                    }
                }
            });
            total.fetch_add(count, Relaxed);
            *parts[u].lock().unwrap_or_else(PoisonError::into_inner) = out;
        });
        if aborted.load(Relaxed) {
            return None;
        }
        let mut rows: Vec<(usize, Row)> = parts.into_iter().flat_map(|p| p.into_inner().unwrap_or_else(PoisonError::into_inner)).collect();
        rows.sort_unstable_by_key(|(slot, _)| *slot);
        let rows = rows.into_iter().map(|(_, row)| row).collect();
        Some((rows, total.load(Relaxed)))
    }

    // ------------------------------------------------------------ updates

    fn overlay_shard(&mut self) -> usize {
        *self.overlay.get_or_insert_with(|| {
            self.shards.push(Shard::default());
            self.shards.len() - 1
        })
    }

    fn kill(&mut self, s: u32, d: u32) {
        let shard = &mut self.shards[s as usize];
        if let Some(dead) = shard.dead.get_mut(d as usize).filter(|dead| !**dead) {
            *dead = true;
            let (a, b) = shard.ranges[d as usize];
            self.entries = self.entries.saturating_sub(b - a);
            self.dead_entries += b - a;
        }
    }

    /// The nearest folder at or above `path` that the index has a listing for.
    fn known_ancestor(&self, path: &str) -> Option<String> {
        let mut p = path.to_string();
        loop {
            if !in_scope(&p, &self.root) {
                return None;
            }
            if self.dirs.contains_key(&key(&p)) {
                return Some(p);
            }
            p = fsys::parent(&p)?;
        }
    }

    /// Refresh one entry's size, date and attributes in place. False if it
    /// isn't where the index expects (the caller re-reads its folder).
    fn touch(&mut self, dir: &str, name: &str, st: &fileop::Stat) -> bool {
        let Some(&(s, d)) = self.dirs.get(&key(dir)) else { return false };
        let shard = &mut self.shards[s as usize];
        let (a, b) = shard.ranges[d as usize];
        let Some(i) = (a..b).find(|&i| shard.name(i) == name) else { return false };
        let m = &mut shard.meta[i];
        if (m.attrs ^ st.attrs) & (win::ATTR_DIR | win::ATTR_REPARSE) != 0 {
            return false;
        }
        (m.size, m.mtime, m.attrs) = (st.size, st.mtime, st.attrs);
        self.dirty = true;
        true
    }

    /// Replace the listings of re-read folders. Returns subfolders the index
    /// doesn't know yet, to be read next.
    fn apply(&mut self, scans: Vec<DirScan>) -> Vec<Arc<str>> {
        let mut gone: HashSet<Box<str>> = HashSet::new();
        let mut new_dirs: Vec<Arc<str>> = Vec::new();
        for scan in scans {
            let k = key(&scan.path);
            let mut old_subdirs: HashSet<String> = HashSet::new();
            if let Some((s, d)) = self.dirs.remove(&k) {
                let shard = &self.shards[s as usize];
                let (a, b) = shard.ranges[d as usize];
                old_subdirs.extend((a..b).filter(|&i| is_subdir(shard.meta[i].attrs)).map(|i| shard.name(i).to_lowercase()));
                self.kill(s, d);
            }
            let Some(list) = scan.entries else {
                gone.insert(k);
                continue;
            };
            let o = self.overlay_shard();
            let shard = &mut self.shards[o];
            let id = shard.begin_dir(scan.path.clone());
            let mut present = HashSet::new();
            for (name, meta) in &list {
                if !shard.push(name, Meta { dir: id, ..*meta }) {
                    break;
                }
                if is_subdir(meta.attrs) {
                    let child = fsys::join(&scan.path, name);
                    if !self.dirs.contains_key(&key(&child)) {
                        new_dirs.push(child.into());
                    }
                    present.insert(name.to_lowercase());
                }
            }
            shard.end_dir();
            let (a, b) = shard.ranges[id as usize];
            self.entries += b - a;
            self.dirs.insert(k, (o as u32, id));
            for old in old_subdirs.difference(&present) {
                gone.insert(key(&fsys::join(&scan.path, old)));
            }
        }
        if !gone.is_empty() {
            // Removed folders take their whole subtree with them.
            let doomed: Vec<(u32, u32)> = self
                .dirs
                .iter()
                .filter(|(k, _)| gone.contains(&***k) || k.match_indices('\\').any(|(i, _)| gone.contains(&k[..i])))
                .map(|(_, &v)| v)
                .collect();
            let doomed_set: HashSet<(u32, u32)> = doomed.iter().copied().collect();
            self.dirs.retain(|_, v| !doomed_set.contains(v));
            for (s, d) in doomed {
                self.kill(s, d);
            }
        }
        self.dirty = true;
        if self.dead_entries > 500_000 && self.dead_entries > self.entries {
            self.compact();
        }
        new_dirs
    }

    /// Rebuild the shards from live listings only, dropping dead ranges.
    fn compact(&mut self) {
        let mut shard = Shard::default();
        let mut dirs = HashMap::with_capacity(self.dirs.len());
        for (k, &(s, d)) in &self.dirs {
            let old = &self.shards[s as usize];
            let (a, b) = old.ranges[d as usize];
            let id = shard.begin_dir(old.dirs[d as usize].clone());
            for i in a..b {
                shard.push(old.name(i), Meta { dir: id, ..old.meta[i] });
            }
            shard.end_dir();
            dirs.insert(k.clone(), (0, id));
        }
        self.entries = shard.meta.len();
        self.shards = vec![shard];
        self.dirs = dirs;
        self.overlay = None;
        self.dead_entries = 0;
    }

    // -------------------------------------------------------- persistence

    /// Write the live listings to `%LOCALAPPDATA%\FEx\index` (a temp file,
    /// then a rename, so a crash never leaves half an index).
    pub fn save(&self) -> io::Result<()> {
        // One save at a time in this process (periodic, after a build, on exit);
        // the temp name keeps other FEx processes' saves apart.
        static SAVING: Mutex<()> = Mutex::new(());
        // Run as administrator, a crawl lists folders the user normally can't;
        // saved, a later ordinary session would reveal those names.
        static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ELEVATED.get_or_init(win::is_elevated) {
            return Ok(());
        }
        let _one = SAVING.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = store_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
        let path = self.save_to(&dir)?;
        prune(&dir, &path, &self.root);
        Ok(())
    }

    /// Where an index of `root` is saved in `dir`.
    pub fn saved_path(dir: &Path, root: &str) -> PathBuf {
        file_for(dir, root)
    }

    /// Write the live listings to `dir` (temp file, then rename), without
    /// the per-user checks of `save`: for the index service, whose folder
    /// only SYSTEM and administrators can read. Returns the file written.
    pub fn save_to(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = file_for(dir, &self.root);
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
            w.write_all(MAGIC)?;
            put_str(&mut w, &self.root)?;
            w.write_all(&(self.build_time.as_millis() as u64).to_le_bytes())?;
            w.write_all(&(self.errors as u64).to_le_bytes())?;
            w.write_all(&(self.dirs.len() as u64).to_le_bytes())?;
            for &(s, d) in self.dirs.values() {
                let shard = &self.shards[s as usize];
                let (a, b) = shard.ranges[d as usize];
                put_str(&mut w, &shard.dirs[d as usize])?;
                w.write_all(&((b - a) as u32).to_le_bytes())?;
                for i in a..b {
                    let m = shard.meta[i];
                    put_str(&mut w, shard.name(i))?;
                    w.write_all(&m.attrs.to_le_bytes())?;
                    w.write_all(&m.size.to_le_bytes())?;
                    w.write_all(&m.mtime.to_le_bytes())?;
                }
            }
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Read a saved index. Anything malformed yields `None`, never a panic.
    pub fn load(path: &Path) -> Option<Index> {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path).ok()?);
        let root = read_header(&mut r)?;
        let build_ms = get_u64(&mut r)?;
        let errors = get_u64(&mut r)? as usize;
        let dir_count = get_u64(&mut r)?;
        let mut shard = Shard::default();
        for _ in 0..dir_count {
            let dir: Arc<str> = get_str(&mut r)?.into();
            if !in_scope(&dir, &root) {
                return None;
            }
            let n = get_u32(&mut r)?;
            let id = shard.begin_dir(dir);
            for _ in 0..n {
                let name = get_str(&mut r)?;
                let attrs = get_u32(&mut r)?;
                let (size, mtime) = (get_u64(&mut r)?, get_u64(&mut r)?);
                if name.is_empty() || name.contains('\\') || !shard.push(&name, Meta { dir: id, attrs, size, mtime }) {
                    return None;
                }
            }
            shard.end_dir();
        }
        Some(Index::from_shards(root.into(), vec![shard], errors, 0, Duration::from_millis(build_ms), true))
    }
}

// ------------------------------------------------------------ persistence

const MAGIC: &[u8; 8] = b"FEXIDX02";
/// Saved indexes kept; the least recently written go first.
const MAX_SAVED: usize = 8;

fn store_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("FEx").join("index"))
}

fn file_for(dir: &Path, root: &str) -> PathBuf {
    // FNV-1a of the folder key.
    let hash = key(root).bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    dir.join(format!("{hash:016x}.idx"))
}

fn put_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn get_u32(r: &mut impl Read) -> Option<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}

fn get_u64(r: &mut impl Read) -> Option<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_le_bytes(b))
}

fn get_str(r: &mut impl Read) -> Option<String> {
    let len = get_u32(r)? as usize;
    if len > 1 << 20 {
        return None;
    }
    let mut b = vec![0; len];
    r.read_exact(&mut b).ok()?;
    String::from_utf8(b).ok()
}

fn read_header(r: &mut impl Read) -> Option<String> {
    let mut magic = [0; 8];
    r.read_exact(&mut magic).ok()?;
    (&magic == MAGIC).then_some(())?;
    get_str(r).filter(|root| !root.is_empty())
}

/// Saved indexes: (root, file).
fn saved() -> Vec<(String, PathBuf)> {
    let Some(dir) = store_dir() else { return Vec::new() };
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "idx"))
        .filter_map(|p| Some((read_header(&mut File::open(&p).ok()?)?, p)))
        .collect()
}

/// The saved index with the narrowest root covering `scope`.
pub fn find_saved(scope: &str) -> Option<PathBuf> {
    saved().into_iter().filter(|(root, _)| in_scope(scope, root)).max_by_key(|(root, _)| root.len()).map(|(_, p)| p)
}

/// Drop saved indexes the new one subsumes, then keep only the newest few.
fn prune(dir: &Path, kept: &Path, root: &str) {
    let mut others: Vec<(String, PathBuf)> = saved().into_iter().filter(|(_, p)| p != kept).collect();
    others.retain(|(other, p)| !(in_scope(other, root) && fs::remove_file(p).is_ok()));
    let mut by_age: Vec<(std::time::SystemTime, PathBuf)> =
        others.into_iter().filter_map(|(_, p)| Some((fs::metadata(&p).ok()?.modified().ok()?, p))).collect();
    by_age.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in by_age.into_iter().skip(MAX_SAVED - 1) {
        let _ = fs::remove_file(p);
    }
    // Leftovers of interrupted saves. Recent ones may belong to a save still
    // running in another FEx window.
    let stale = |e: &fs::DirEntry| {
        e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > Duration::from_secs(3600)))
    };
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "tmp") && stale(&e) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

// ------------------------------------------------------------- freshness

/// A folder's current contents, read outside the index lock. `None`: the
/// folder is gone or unreadable.
struct DirScan {
    path: Arc<str>,
    entries: Option<Vec<(Box<str>, Meta)>>,
}

fn read_dir(path: Arc<str>) -> DirScan {
    let mut entries = Vec::new();
    let mut name = String::new();
    let ok = win::scan_dir(&path, |e| {
        name.clear();
        win::push_utf16(&mut name, e.name);
        entries.push((name.as_str().into(), Meta { dir: 0, attrs: e.attrs, size: e.size, mtime: e.mtime }));
    })
    .is_ok();
    DirScan { path, entries: ok.then_some(entries) }
}

pub enum Freshness {
    /// Names under the root changed; the index has been updated.
    Changed,
    /// Changes were lost (too many at once); only a new crawl is accurate.
    Stale,
}

type OnChange = Box<dyn Fn(Freshness) + Send>;

/// Changes beyond this many, waiting for a crawl to finish, count as lost.
const MAX_BACKLOG: usize = 200_000;

struct Pending {
    target: Option<(Shared, OnChange)>,
    backlog: Vec<(u32, String)>,
    overflowed: bool,
}

/// Follows changes below an index's root from the moment its crawl starts,
/// then keeps the finished index up to date.
pub struct Freshener {
    root: Arc<str>,
    state: Arc<Mutex<Pending>>,
    _watcher: watch::Watcher,
}

fn lock_pending(m: &Mutex<Pending>) -> MutexGuard<'_, Pending> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Freshener {
    pub fn start(root: Arc<str>) -> Freshener {
        let state = Arc::new(Mutex::new(Pending { target: None, backlog: Vec::new(), overflowed: false }));
        let (s, r) = (state.clone(), root.clone());
        let watcher = watch::watch_tree(root.to_string(), move |batch| {
            let mut st = lock_pending(&s);
            match batch {
                Some(changes) if st.backlog.len() + changes.len() <= MAX_BACKLOG => st.backlog.extend(changes),
                _ => {
                    st.overflowed = true;
                    st.backlog.clear();
                }
            }
            flush(&r, &mut st);
        });
        Freshener { root, state, _watcher: watcher }
    }

    /// Start applying changes (and those seen so far) to `index`. The backlog
    /// is applied on a worker: it can mean re-reading many folders, and the
    /// caller is the UI thread.
    pub fn attach(&self, index: Shared, on_change: impl Fn(Freshness) + Send + 'static) {
        lock_pending(&self.state).target = Some((index, Box::new(on_change)));
        let (state, root) = (self.state.clone(), self.root.clone());
        thread::spawn(move || {
            // A failure here only leaves the index less fresh.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush(&root, &mut lock_pending(&state))));
        });
    }
}

fn flush(root: &str, st: &mut Pending) {
    let Some((index, on_change)) = &st.target else { return };
    if std::mem::take(&mut st.overflowed) {
        on_change(Freshness::Stale);
    }
    let changes = std::mem::take(&mut st.backlog);
    if !changes.is_empty() && refresh(root, index, changes) {
        on_change(Freshness::Changed);
    }
}

/// Re-read `dirs` (or their nearest indexed ancestors) and crawl what's new
/// below them: for changes learned elsewhere (the NTFS change journal).
/// Returns how many folders were re-read.
pub fn refresh_dirs(index: &Shared, dirs: Vec<String>) -> usize {
    let mut seen: HashSet<Box<str>> = HashSet::new();
    let mut pending: Vec<Arc<str>> = Vec::new();
    {
        let idx = read(index);
        for d in dirs {
            if let Some(dir) = idx.known_ancestor(&d)
                && seen.insert(key(&dir))
            {
                pending.push(dir.into());
            }
        }
    }
    let mut n = 0;
    while !pending.is_empty() {
        n += pending.len();
        let scans: Vec<DirScan> = pending.drain(..).map(read_dir).collect();
        pending = write(index).apply(scans);
    }
    n
}

/// Apply watcher changes: metadata updates in place, everything else by
/// re-reading the folder (and crawling new subfolders). True when names changed.
fn refresh(root: &str, index: &Shared, changes: Vec<(u32, String)>) -> bool {
    let mut seen: HashSet<Box<str>> = HashSet::new();
    let mut dirs: Vec<Arc<str>> = Vec::new();
    let mut touched: Vec<(String, String, String)> = Vec::new(); // (folder, name, full path)
    {
        let idx = read(index);
        for (action, rel) in changes {
            let full = fsys::join(root, &rel);
            let Some(parent) = fsys::parent(&full) else { continue };
            if action == watch::MODIFIED {
                let name = full.rsplit('\\').next().unwrap_or("").to_string();
                touched.push((parent, name, full));
            } else if let Some(dir) = idx.known_ancestor(&parent) {
                if seen.insert(key(&dir)) {
                    dirs.push(dir.into());
                }
            }
        }
    }
    if !touched.is_empty() {
        let stats: Vec<_> = touched.into_iter().map(|(dir, name, full)| (dir, name, fileop::stat(&full))).collect();
        let mut idx = write(index);
        for (dir, name, st) in stats {
            if !st.is_some_and(|st| idx.touch(&dir, &name, &st)) {
                if let Some(d) = idx.known_ancestor(&dir) {
                    if seen.insert(key(&d)) {
                        dirs.push(d.into());
                    }
                }
            }
        }
    }
    let structural = !dirs.is_empty();
    let mut pending = dirs;
    while !pending.is_empty() {
        let scans: Vec<DirScan> = pending.drain(..).map(read_dir).collect();
        pending = write(index).apply(scans);
    }
    structural
}

/// An index in use: queried by the UI, kept fresh by its watcher.
pub struct Live {
    pub root: Arc<str>,
    /// A saved copy, not yet replaced by a fresh crawl.
    pub loaded: bool,
    pub created: Instant,
    pub index: Shared,
    _fresh: Option<Freshener>,
}

impl Live {
    /// `fresh` is the watcher started with the crawl (none for a loaded index,
    /// which a new crawl replaces soon).
    pub fn new(index: Index, fresh: Option<Freshener>, on_change: impl Fn(Freshness) + Send + 'static) -> Arc<Live> {
        let (root, loaded) = (index.root.clone(), index.loaded);
        let index: Shared = Arc::new(RwLock::new(index));
        if let Some(f) = &fresh {
            f.attach(index.clone(), on_change);
        }
        Arc::new(Live { root, loaded, created: Instant::now(), index, _fresh: fresh })
    }

    pub fn covers(&self, path: &str) -> bool {
        in_scope(path, &self.root)
    }
}

// ----------------------------------------------------------------- build

/// An in-progress index build. While crawling, entries are also tested
/// against the current matcher so results stream in before the index is done.
pub struct Build {
    pub root: Arc<str>,
    matcher: RwLock<(u64, Option<Arc<Matcher>>)>,
    cancel: AtomicBool,
    fresh: Mutex<Option<Freshener>>,
    pub dirs_done: AtomicUsize,
    pub entries: AtomicUsize,
    pub started: Instant,
    hooks: Option<Hooks>,
}

/// Crawl hooks for background builds (the index service): `init` runs once
/// on each crawler thread (to lower its priority), `pace` before each folder
/// (to account CPU time and pause while the machine is busy).
pub struct Hooks {
    pub init: Box<dyn Fn() + Send + Sync>,
    pub pace: Box<dyn Fn() + Send + Sync>,
}

impl Build {
    pub fn new(root: &str) -> Arc<Build> {
        Arc::new(Build {
            root: root.into(),
            matcher: RwLock::new((0, None)),
            cancel: AtomicBool::new(false),
            fresh: Mutex::new(None),
            dirs_done: AtomicUsize::new(0),
            entries: AtomicUsize::new(0),
            started: Instant::now(),
            hooks: None,
        })
    }

    /// A build whose crawler threads run `hooks` (see `Hooks`).
    pub fn with_hooks(root: &str, hooks: Hooks) -> Arc<Build> {
        let mut b = Arc::try_unwrap(Build::new(root)).unwrap_or_else(|_| unreachable!());
        b.hooks = Some(hooks);
        Arc::new(b)
    }

    pub fn set_matcher(&self, generation: u64, m: Option<Arc<Matcher>>) {
        *self.matcher.write().unwrap_or_else(PoisonError::into_inner) = (generation, m);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    /// The change watcher started with the crawl, for the finished index.
    pub fn take_freshener(&self) -> Option<Freshener> {
        self.fresh.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Crawl `root` on all cores. Calls `on_hits(generation, rows)` with
    /// batches of streamed matches. Returns `None` if cancelled.
    pub fn run(&self, on_hits: &(dyn Fn(u64, Vec<Row>) + Sync)) -> Option<Index> {
        *self.fresh.lock().unwrap_or_else(PoisonError::into_inner) = Some(Freshener::start(self.root.clone()));
        let cores = walk::cores();
        // Network enumeration is latency-bound, so oversubscribe there.
        let workers = std::env::var("FEX_CRAWL_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(if self.root.starts_with(r"\\") { (cores * 4).min(64) } else { cores.max(4) });
        let queue = Queue::new(vec![self.root.clone()], workers);

        let (results, worker_errors): (Vec<(Shard, usize)>, usize) = thread::scope(|sc| {
            let handles: Vec<_> = (0..workers).map(|_| sc.spawn(|| self.worker(&queue, on_hits))).collect();
            handles.into_iter().fold((Vec::new(), 0), |(mut results, mut errors), handle| {
                match handle.join() {
                    Ok(result) => results.push(result),
                    Err(_) => errors += 1,
                }
                (results, errors)
            })
        });
        if self.cancel.load(Relaxed) {
            return None;
        }
        let errors = results.iter().map(|r| r.1).sum();
        let shards: Vec<Shard> = results.into_iter().map(|r| r.0).filter(|s| !s.dirs.is_empty()).collect();
        Some(Index::from_shards(self.root.clone(), shards, errors, worker_errors, self.started.elapsed(), false))
    }

    fn worker(&self, queue: &Queue, on_hits: &(dyn Fn(u64, Vec<Row>) + Sync)) -> (Shard, usize) {
        let _enlisted = queue.enlist();
        if let Some(h) = &self.hooks {
            (h.init)();
        }
        let mut shard = Shard::default();
        let mut local: Vec<Arc<str>> = Vec::new();
        let mut name = String::new();
        let mut hits: Vec<Row> = Vec::new();
        let mut hits_gen = 0;
        let mut last_flush = Instant::now();
        let mut errors = 0;

        loop {
            let dir = match local.pop() {
                Some(d) => d,
                None => {
                    if !hits.is_empty() {
                        on_hits(hits_gen, std::mem::take(&mut hits));
                    }
                    match queue.take(&self.cancel) {
                        Some(d) => d,
                        None => break,
                    }
                }
            };
            if self.cancel.load(Relaxed) {
                break;
            }
            if let Some(h) = &self.hooks {
                (h.pace)();
            }
            let (generation, matcher) = self.matcher.read().unwrap_or_else(PoisonError::into_inner).clone();
            if generation != hits_gen {
                hits.clear(); // stale query; the UI would discard these anyway
                hits_gen = generation;
            }

            let dir_id = shard.begin_dir(dir.clone());
            let mut n = 0;
            let scanned = win::scan_dir(&dir, |e| {
                name.clear();
                win::push_utf16(&mut name, e.name);
                if !shard.push(&name, Meta { dir: dir_id, attrs: e.attrs, size: e.size, mtime: e.mtime }) {
                    return;
                }
                n += 1;
                if matcher.as_ref().is_some_and(|m| m.is_match_entry(&name, e.attrs & win::ATTR_DIR != 0)) {
                    hits.push(Row {
                        name: name.as_str().into(),
                        dir: Some(dir.clone()),
                        size: e.size,
                        mtime: e.mtime,
                        attrs: e.attrs,
                        sel: false,
                    });
                }
                // Don't follow junctions/symlinks: avoids cycles and double counting.
                if is_subdir(e.attrs) {
                    local.push(fsys::join(&dir, &name).into());
                }
            });
            if scanned.is_err() {
                errors += 1;
            }
            shard.end_dir();
            self.entries.fetch_add(n, Relaxed);
            self.dirs_done.fetch_add(1, Relaxed);

            queue.balance(&mut local);
            if !hits.is_empty() && (hits.len() >= 1024 || last_flush.elapsed() > Duration::from_millis(30)) {
                on_hits(hits_gen, std::mem::take(&mut hits));
                last_flush = Instant::now();
            }
        }
        if !hits.is_empty() {
            on_hits(hits_gen, hits);
        }
        (shard, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard(names: &[&str]) -> Shard {
        let mut s = Shard::default();
        s.dirs.push("C:\\t".into());
        for n in names {
            s.push(n, Meta { dir: 0, attrs: 0, size: 0, mtime: 0 });
        }
        s
    }

    fn hits(s: &Shard, q: &str, regex: bool) -> Vec<String> {
        let m = Matcher::new(q, regex).unwrap().unwrap();
        let mut out = vec![];
        s.scan(&m, 0, s.meta.len(), |i| out.push(s.name(i).to_string()));
        out
    }

    #[test]
    fn arena_scan() {
        let s = shard(&["report.pdf", "Report-2024.PDF", "notes.txt", "a", "pdf"]);
        assert_eq!(hits(&s, "pdf", false), ["report.pdf", "Report-2024.PDF", "pdf"]);
        assert_eq!(hits(&s, "*.pdf", false), ["report.pdf", "Report-2024.PDF"]);
        assert_eq!(hits(&s, "report 2024", false), ["Report-2024.PDF"]);
        assert_eq!(hits(&s, "REPORT", false), ["report.pdf", "Report-2024.PDF"]);
        assert_eq!(hits(&s, r"^\w+\.txt$", true), ["notes.txt"]);
        assert_eq!(hits(&s, "^a$", true), ["a"]);
        // Must not match across the separator ("...txt\na").
        assert_eq!(hits(&s, r"txt\sa", true), Vec::<String>::new());
        assert_eq!(hits(&s, "t[^x]a", true), Vec::<String>::new());
    }

    #[test]
    fn type_filter_combines_with_keywords_and_regex() {
        let mut s = shard(&["matlab.bat", "matlab.cmd", "matlab.bat.txt", "other.bat"]);
        assert_eq!(hits(&s, "matlab type:bat", false), ["matlab.bat"]);
        assert_eq!(hits(&s, r"matlab type:.bat", true), ["matlab.bat"]);
        assert_eq!(hits(&s, "type:bat", false), ["matlab.bat", "other.bat"]);

        s.meta[0].attrs = win::ATTR_DIR;
        assert_eq!(hits(&s, "type:bat", false), ["other.bat"]);
        assert!(Matcher::new("type:", false).is_err());
    }

    /// `cargo test --release -- --ignored --nocapture crawl_real_tree`
    #[test]
    #[ignore]
    fn crawl_real_tree() {
        let root = std::env::var("FEX_BENCH_ROOT").unwrap_or_else(|_| r"C:\Windows".into());
        let build = Build::new(&root);
        build.set_matcher(1, Matcher::new("*.dll", false).unwrap().map(Arc::new));
        let streamed = AtomicUsize::new(0);
        let idx = build.run(&|_, rows| { streamed.fetch_add(rows.len(), Relaxed); }).unwrap();
        println!("indexed {} entries in {:?} ({} unreadable dirs)", idx.entries, idx.build_time, idx.errors);
        for (q, regex) in [("*.dll", false), ("kernel", false), (r"^[a-z]{2}-[A-Z]{2}$", true), (r"\d{4,}", true)] {
            let m = Matcher::new(q, regex).unwrap().unwrap();
            let t = Instant::now();
            let (_, total) = idx.query(&m, &root, MAX, &|| false).unwrap();
            println!("{q:>22}: {total:>8} matches in {:?}", t.elapsed());
            if q == "*.dll" {
                assert_eq!(total, streamed.load(Relaxed), "streamed hits must equal index query");
            }
        }
    }
    const MAX: usize = 250_000;

    #[test]
    fn scope() {
        assert!(in_scope(r"C:\Users\x", r"C:\"));
        assert!(in_scope(r"c:\users", r"C:\Users"));
        assert!(!in_scope(r"C:\UsersX", r"C:\Users"));
        assert!(in_scope(r"\\srv\share\a", r"\\srv\share"));
    }

    fn index_of(shard: Shard, root: &str) -> Index {
        Index::from_shards(root.into(), vec![shard], 0, 0, Duration::ZERO, false)
    }

    #[test]
    fn scoped_queries_merge_adjacent_directory_ranges() {
        let shard = Shard {
            dirs: vec![
                r"C:\root".into(),
                r"C:\root\a".into(),
                r"C:\root\a\nested".into(),
                r"C:\root\b".into(),
            ],
            ranges: vec![(0, 1), (1, 2), (2, 3), (3, 4)],
            ..Shard::default()
        };
        let index = index_of(shard, r"C:\root");
        assert_eq!(index.query_units(r"C:\root\a"), [(0, 1, 3)]);
        assert_eq!(index.query_units(r"C:\root"), [(0, 0, 4)]);
    }

    #[test]
    fn parallel_query_retains_at_most_the_global_limit() {
        let mut shard = shard(&["file-a", "file-b"]);
        shard.ranges.push((0, 2));
        let index = index_of(shard, r"C:\t");
        let matcher = Matcher::new("file", false).unwrap().unwrap();

        let (rows, total) = index.query(&matcher, r"C:\t", 1, &|| false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(total, 2);
        let (rows, total) = index.query(&matcher, r"C:\t", 0, &|| false).unwrap();
        assert!(rows.is_empty());
        assert_eq!(total, 2);
    }

    #[test]
    fn crawler_panics_are_reported() {
        use std::fs;

        let root = std::env::temp_dir().join(format!("fex-index-panic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("trigger.txt"), b"test").unwrap();

        let build = Build::new(&root.to_string_lossy());
        build.set_matcher(1, Matcher::new("trigger", false).unwrap().map(Arc::new));
        let index = build.run(&|_, _| panic!("simulated streaming failure")).unwrap();

        assert_eq!(index.worker_errors, 1);
        let _ = fs::remove_dir_all(root);
    }

    fn names(index: &Index, q: &str) -> Vec<String> {
        let m = Matcher::new(q, false).unwrap().unwrap();
        let mut found: Vec<String> = index.query(&m, &index.root, MAX, &|| false).unwrap().0.into_iter().map(|r| r.name.to_string()).collect();
        found.sort();
        found
    }

    #[test]
    fn rereading_folders_replaces_their_listings() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("fex-index-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(r"sub\deep")).unwrap();
        fs::write(root.join("one.txt"), b"1").unwrap();
        fs::write(root.join(r"sub\deep\two.txt"), b"2").unwrap();
        let r = root.to_string_lossy().into_owned();
        let mut index = Build::new(&r).run(&|_, _| {}).unwrap();
        assert_eq!(names(&index, "txt"), ["one.txt", "two.txt"]);

        // A new file, and a removed folder whose subtree must disappear too.
        fs::write(root.join("three.txt"), b"3").unwrap();
        fs::remove_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join(r"fresh\inner")).unwrap();
        fs::write(root.join(r"fresh\inner\four.txt"), b"4").unwrap();
        let mut pending = index.apply(vec![read_dir(r.as_str().into())]);
        while !pending.is_empty() {
            pending = index.apply(pending.drain(..).map(read_dir).collect());
        }
        assert_eq!(names(&index, "txt"), ["four.txt", "one.txt", "three.txt"]);
        index.compact();
        assert_eq!(names(&index, "txt"), ["four.txt", "one.txt", "three.txt"]);
        let _ = fs::remove_dir_all(root);
    }
}
