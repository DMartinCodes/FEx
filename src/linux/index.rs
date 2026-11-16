//! Parallel crawler and in-memory name index (the Linux side of `index.rs`).
//!
//! Same layout and query strategy as the Windows index: names in contiguous
//! arenas plus an ASCII-lowercased twin, SIMD `memmem` for plain keywords,
//! the regex run over the whole arena otherwise, and 32K-entry work units on
//! the pool.
//!
//! Linux-specific choices:
//! * Names are bytes separated by NUL, the one byte no Linux name can hold,
//!   so every name is stored exactly. Regexes use NUL as line terminator.
//! * The crawl records names and types only (getdents gives those for
//!   free); results get sizes and dates after the query. `with_meta`
//!   records them during the crawl instead, for benchmarks.
//! * Every folder is recorded with its ctime. Adding, removing or renaming
//!   an entry changes it, so comparing ctimes (`sweep`) finds every folder
//!   whose part of a names-only index is stale, without reading listings.
//! * Arrays hold no pointers, so an index saves as a snapshot that loads by
//!   memory-mapping (`snapshot.rs`). Updates append re-read folders to an
//!   in-memory overlay and mark the old listing dead; saving compacts.
//! * Folder identity is exact: `Foo/` and `foo/` are different folders.
//! * Queries take a `Visibility`, so the daemon can show each user only
//!   folders they could list themselves.

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};

use super::fsys::{self, Row};
use super::snapshot::{self, Buf, Loaded};
use super::sys::{self, Dir, DIR, LINK};
use super::walk::{self, Visit};
use crate::pool;

/// Compiled search query: whitespace-separated keywords that must all match
/// (`*`/`?` are wildcards; `type:ext` filters by extension), or one regex.
/// Matching is case-insensitive, as in the Windows build.
pub struct Matcher {
    primary: Regex,
    literal: Option<memmem::Finder<'static>>,
    rest: Vec<Regex>,
    extension: Option<Vec<u8>>,
}

fn build_regex(p: &str) -> Result<Regex, String> {
    RegexBuilder::new(p)
        .case_insensitive(true)
        .multi_line(true)
        .line_terminator(0)
        .build()
        .map_err(|e| e.to_string())
}

impl Matcher {
    pub fn new(query: &str, regex_mode: bool) -> Result<Option<Matcher>, String> {
        let mut terms = Vec::new();
        let mut extension: Option<String> = None;
        for term in query.split_whitespace() {
            if !term.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("type:")) {
                terms.push(term);
                continue;
            }
            let value = term[5..].trim_start_matches('.');
            if value.is_empty() || value.contains(['/', '*', '?']) {
                return Err("file type filter requires an extension, e.g. type:sh".into());
            }
            if extension.as_ref().is_some_and(|old| !old.eq_ignore_ascii_case(value)) {
                return Err("specify only one file type filter".into());
            }
            extension = Some(value.to_string());
        }
        let pattern_query = terms.join(" ");
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
        // A bare `type:ext` scans for `.ext` followed by the NUL separator:
        // one SIMD pass over the lowercased arena instead of testing every name.
        let type_only = pats.is_empty();
        let mut pats = if type_only { vec![(".".to_string(), None)] } else { pats };
        if let (true, Some(e)) = (type_only, extension.as_ref().filter(|e| e.is_ascii())) {
            pats[0].1 = Some(format!(".{}\0", e.to_ascii_lowercase()));
        }
        let best = (0..pats.len()).max_by_key(|&i| pats[i].0.len()).unwrap_or(0);
        let literal = pats[best].1.take().map(|t| memmem::Finder::new(t.as_bytes()).into_owned());
        let mut res = pats.iter().map(|(p, _)| build_regex(p)).collect::<Result<Vec<_>, _>>()?;
        let primary = res.swap_remove(best);
        Ok(Some(Matcher { primary, literal, rest: res, extension: extension.map(String::into_bytes) }))
    }

    pub fn is_match(&self, name: &[u8]) -> bool {
        self.primary.is_match(name)
            && self.rest.iter().all(|r| r.is_match(name))
            && self.extension.as_ref().is_none_or(|e| fsys::ext(name).eq_ignore_ascii_case(e))
    }

    pub fn is_match_entry(&self, name: &[u8], is_dir: bool) -> bool {
        !(is_dir && self.extension.is_some()) && self.is_match(name)
    }
}

fn keyword_pattern(k: &str) -> String {
    if !k.contains(['*', '?']) {
        return regex::escape(k);
    }
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

/// One crawler thread's (or one snapshot's, or the update overlay's) share
/// of the index. Folder ids are local to the shard.
#[derive(Default)]
pub struct Shard {
    /// `name0\0name1\0...`
    names: Buf<u8>,
    /// `names` with ASCII letters lowercased (same offsets).
    lower: Buf<u8>,
    starts: Buf<u32>,
    entry_dir: Buf<u32>,
    entry_flags: Buf<u8>,
    /// All folder paths, back to back; `dir_starts` holds where each begins.
    dir_paths: Buf<u8>,
    dir_starts: Buf<u32>,
    /// `[first, end)` entry of each folder, two values per folder.
    ranges: Buf<u32>,
    dir_ctime: Buf<i64>,
    /// (size, mtime) per entry; empty unless built `with_meta`.
    meta: Vec<(u64, i64)>,
}

impl Shard {
    /// Entries in this shard.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    #[inline]
    fn ndirs(&self) -> usize {
        self.dir_starts.len()
    }

    #[inline]
    fn dir_path(&self, d: usize) -> &[u8] {
        let a = self.dir_starts[d] as usize;
        let b = self.dir_starts.get(d + 1).map_or(self.dir_paths.len(), |&x| x as usize);
        &self.dir_paths[a..b]
    }

    #[inline]
    fn range(&self, d: usize) -> (usize, usize) {
        (self.ranges[2 * d] as usize, self.ranges[2 * d + 1] as usize)
    }

    /// Start the listing of a folder; entries pushed next belong to it.
    fn begin_dir(&mut self, path: &[u8], ctime: i64) -> bool {
        if self.dir_paths.len() + path.len() > u32::MAX as usize || self.len() >= u32::MAX as usize {
            return false;
        }
        let n = self.len() as u32;
        let at = self.dir_paths.len() as u32;
        self.dir_starts.vec().push(at);
        self.dir_paths.vec().extend_from_slice(path);
        self.ranges.vec().extend_from_slice(&[n, n]);
        self.dir_ctime.vec().push(ctime);
        true
    }

    fn push(&mut self, name: &[u8], flags: u8) -> bool {
        if self.ndirs() == 0 || self.names.len() + name.len() + 1 > u32::MAX as usize {
            return false;
        }
        let d = (self.ndirs() - 1) as u32;
        let at = self.names.len() as u32;
        self.starts.vec().push(at);
        let names = self.names.vec();
        names.extend_from_slice(name);
        names.push(0);
        let lower = self.lower.vec();
        lower.extend(name.iter().map(u8::to_ascii_lowercase));
        lower.push(0);
        self.entry_dir.vec().push(d);
        self.entry_flags.vec().push(flags);
        let end = self.len() as u32;
        if let Some(last) = self.ranges.vec().last_mut() {
            *last = end;
        }
        true
    }

    #[inline]
    fn end_of(&self, i: usize) -> usize {
        self.starts.get(i).map_or(self.names.len(), |&s| s as usize)
    }

    #[inline]
    fn name(&self, i: usize) -> &[u8] {
        &self.names[self.starts[i] as usize..self.end_of(i + 1) - 1]
    }

    /// A result row; `cache` reuses the folder path across a unit's hits.
    fn row(&self, i: usize, cache: &mut Option<(u32, Arc<[u8]>)>) -> Row {
        let d = self.entry_dir[i];
        let dir = match cache {
            Some((cd, a)) if *cd == d => a.clone(),
            _ => {
                let a: Arc<[u8]> = self.dir_path(d as usize).into();
                *cache = Some((d, a.clone()));
                a
            }
        };
        let (size, mtime) = self.meta.get(i).copied().unwrap_or((0, 0));
        Row { name: self.name(i).into(), dir: Some(dir), size, mtime, flags: self.entry_flags[i], sel: false, id: 0 }
    }

    fn scan(&self, m: &Matcher, a: usize, b: usize, mut hit: impl FnMut(usize)) {
        let end = self.end_of(b);
        let hay = &self.names[..end];
        let lower = &self.lower[..end];
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
            if m.is_match_entry(self.name(i), self.entry_flags[i] & DIR != 0) {
                hit(i);
            }
            pos = self.end_of(i + 1);
        }
    }

    fn heap_bytes(&self) -> usize {
        self.names.heap_bytes()
            + self.lower.heap_bytes()
            + self.starts.heap_bytes()
            + self.entry_dir.heap_bytes()
            + self.entry_flags.heap_bytes()
            + self.dir_paths.heap_bytes()
            + self.dir_starts.heap_bytes()
            + self.ranges.heap_bytes()
            + self.dir_ctime.heap_bytes()
            + self.meta.capacity() * size_of::<(u64, i64)>()
    }

    fn mapped_bytes(&self) -> usize {
        self.names.mapped_bytes()
            + self.lower.mapped_bytes()
            + self.starts.mapped_bytes()
            + self.entry_dir.mapped_bytes()
            + self.entry_flags.mapped_bytes()
            + self.dir_paths.mapped_bytes()
            + self.dir_starts.mapped_bytes()
            + self.ranges.mapped_bytes()
            + self.dir_ctime.mapped_bytes()
    }

    /// Check every offset of a shard read from disk, so slicing can't go out
    /// of bounds later.
    fn validate(&self) -> Result<(), String> {
        let (n, nd) = (self.len(), self.ndirs());
        let bad = |why: &str| Err(format!("snapshot is damaged ({why})"));
        if self.entry_dir.len() != n || self.entry_flags.len() != n || self.lower.len() != self.names.len() {
            return bad("array lengths");
        }
        if n > 0 && self.names.last() != Some(&0) {
            return bad("name arena");
        }
        for i in 0..n {
            let (s, e) = (self.starts[i] as usize, self.end_of(i + 1));
            if s >= e || e > self.names.len() || self.names[e - 1] != 0 || self.entry_dir[i] as usize >= nd {
                return bad("entry offsets");
            }
        }
        if self.ranges.len() != 2 * nd || self.dir_ctime.len() != nd {
            return bad("folder arrays");
        }
        let mut prev = 0;
        for d in 0..nd {
            let s = self.dir_starts[d] as usize;
            let (a, b) = self.range(d);
            if s < prev || s > self.dir_paths.len() || a > b || b > n {
                return bad("folder offsets");
            }
            prev = s;
        }
        Ok(())
    }
}

const CHUNK: usize = 32 * 1024;

/// (shard, folder) within an index.
type DirRef = (u32, u32);
/// Folder path -> its live listing. Ordered, so a folder's whole subtree is
/// one range of keys (see `subtree`).
pub type FolderMap = BTreeMap<Box<[u8]>, DirRef>;
/// (shard, entry): one search match.
pub type Hit = (u32, u32);
/// A re-read folder: its ctime and (name, flags) entries.
type Listing = (i64, Vec<(Box<[u8]>, u8)>);

/// Which folders' entries a query may report. The daemon implements this
/// per client, so users only see names in folders they could list.
pub trait Visibility: Sync {
    /// Runs on each worker thread before it checks anything (for example,
    /// to take on the client's identity); dropping the result undoes it.
    fn enter(&self) -> Option<Box<dyn std::any::Any>> {
        None
    }
    /// Whether entries of folder `dir` (a global folder id below
    /// `Index::dir_slots`, at `path`) may be reported.
    fn visible(&self, dir: usize, path: &[u8]) -> bool;
    /// False when everything is visible, so queries skip the checks.
    fn filters(&self) -> bool {
        true
    }
}

/// No filtering (single-user processes, root clients).
pub struct Everything;

impl Visibility for Everything {
    fn visible(&self, _: usize, _: &[u8]) -> bool {
        true
    }
    fn filters(&self) -> bool {
        false
    }
}

pub struct Index {
    pub root: Arc<[u8]>,
    shards: Vec<Shard>,
    /// Folders whose listing was replaced or removed; queries skip them.
    dead: Vec<Vec<bool>>,
    /// Folder path -> (shard, folder) of its live listing. Built when the
    /// first update needs it, so loading a snapshot stays instant.
    map: Option<FolderMap>,
    overlay: Option<usize>,
    /// Live entries and folders.
    pub entries: usize,
    pub dirs: usize,
    dead_entries: usize,
    pub errors: usize,
    pub build_time: Duration,
    /// When the data was crawled, or the snapshot written (ns since epoch).
    pub created_ns: u64,
    /// Bumped by every change.
    pub generation: u64,
    pub from_snapshot: bool,
}

/// A folder's current listing, read outside the index lock. `listing` is
/// `None` when the folder is gone or unreadable.
pub struct DirScan {
    pub path: Arc<[u8]>,
    listing: Option<Listing>,
}

pub fn read_dir(path: Arc<[u8]>) -> DirScan {
    let listing = Dir::open(&path).ok().and_then(|dir| {
        let ctime = dir.stat_self()?.ctime;
        let mut entries = Vec::new();
        dir.scan(&mut Vec::new(), |e| entries.push((e.name.into(), e.flags))).ok()?;
        Some((ctime, entries))
    });
    DirScan { path, listing }
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

impl Index {
    fn from_shards(root: Arc<[u8]>, shards: Vec<Shard>, errors: usize, build_time: Duration) -> Index {
        let dead = shards.iter().map(|s| vec![false; s.ndirs()]).collect();
        Index {
            root,
            entries: shards.iter().map(Shard::len).sum(),
            dirs: shards.iter().map(Shard::ndirs).sum(),
            shards,
            dead,
            map: None,
            overlay: None,
            dead_entries: 0,
            errors,
            build_time,
            created_ns: now_ns(),
            generation: 0,
            from_snapshot: false,
        }
    }

    pub fn covers(&self, path: &[u8]) -> bool {
        fsys::in_scope(path, &self.root)
    }

    /// Heap bytes; a loaded snapshot's arrays live in the page cache instead
    /// (`mapped_bytes`).
    pub fn bytes(&self) -> usize {
        self.shards.iter().map(Shard::heap_bytes).sum()
    }

    pub fn mapped_bytes(&self) -> usize {
        self.shards.iter().map(Shard::mapped_bytes).sum()
    }

    /// Number of global folder ids (live and dead), for per-query caches.
    pub fn dir_slots(&self) -> usize {
        self.shards.iter().map(Shard::ndirs).sum()
    }

    #[inline]
    fn is_dead(&self, s: usize, d: usize) -> bool {
        self.dead[s][d]
    }

    /// The folder with the most entries, and how many (for benchmarks).
    pub fn largest_dir(&self) -> Option<(Arc<[u8]>, usize)> {
        let mut best: Option<(usize, usize, usize)> = None;
        for (s, shard) in self.shards.iter().enumerate() {
            for d in 0..shard.ndirs() {
                let (a, b) = shard.range(d);
                if !self.is_dead(s, d) && best.is_none_or(|(_, _, n)| b - a > n) {
                    best = Some((s, d, b - a));
                }
            }
        }
        best.map(|(s, d, n)| (self.shards[s].dir_path(d).into(), n))
    }

    fn query_units(&self, scope: &[u8]) -> Vec<(usize, usize, usize)> {
        fn flush(units: &mut Vec<(usize, usize, usize)>, shard: usize, run: Option<(usize, usize)>) {
            if let Some((a, b)) = run {
                units.extend((a..b).step_by(CHUNK).map(|x| (shard, x, (x + CHUNK).min(b))));
            }
        }
        let everything = fsys::in_scope(&self.root, scope);
        let mut units = Vec::new();
        for (si, shard) in self.shards.iter().enumerate() {
            let mut run: Option<(usize, usize)> = None;
            for d in 0..shard.ndirs() {
                let (start, end) = shard.range(d);
                if start == end || self.is_dead(si, d) || !(everything || fsys::in_scope(shard.dir_path(d), scope)) {
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

    /// Search entries under `scope` in parallel: up to `limit` rows plus the
    /// exact match count, or `None` if `cancelled` fired.
    pub fn query(&self, m: &Matcher, scope: &[u8], limit: usize, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<(Vec<Row>, usize)> {
        self.query_as(m, scope, limit, cancelled, &Everything)
    }

    /// `query` reporting (and counting) only entries of folders `vis`
    /// allows. When there are more than `limit` matches, the rows kept are
    /// the first `limit` in name order.
    pub fn query_as(
        &self,
        m: &Matcher,
        scope: &[u8],
        limit: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
        vis: &dyn Visibility,
    ) -> Option<(Vec<Row>, usize)> {
        let mut hits = self.query_hits(m, scope, cancelled, vis)?;
        let total = hits.len();
        self.top_hits(&mut hits, limit, false);
        Some((self.hit_rows(&hits), total))
    }

    /// Every match under `scope` that `vis` allows, as (shard, entry) pairs:
    /// 8 bytes each, no names copied. Deterministic order.
    pub fn query_hits(&self, m: &Matcher, scope: &[u8], cancelled: &(dyn Fn() -> bool + Sync), vis: &dyn Visibility) -> Option<Vec<Hit>> {
        let units = self.query_units(scope);
        let mut base = Vec::with_capacity(self.shards.len());
        let mut acc = 0;
        for s in &self.shards {
            base.push(acc);
            acc += s.ndirs();
        }
        let filters = vis.filters();
        let aborted = AtomicBool::new(false);
        let parts: Vec<Mutex<Vec<Hit>>> = units.iter().map(|_| Mutex::new(Vec::new())).collect();
        pool::for_each(units.len(), &|u| {
            if aborted.load(Relaxed) || cancelled() {
                aborted.store(true, Relaxed);
                return;
            }
            let _guard = if filters { vis.enter() } else { None };
            let (si, a, b) = units[u];
            let shard = &self.shards[si];
            let mut out = Vec::new();
            let mut last: Option<(u32, bool)> = None;
            shard.scan(m, a, b, |i| {
                if filters {
                    let d = shard.entry_dir[i];
                    let ok = match last {
                        Some((ld, ok)) if ld == d => ok,
                        _ => {
                            let ok = vis.visible(base[si] + d as usize, shard.dir_path(d as usize));
                            last = Some((d, ok));
                            ok
                        }
                    };
                    if !ok {
                        return;
                    }
                }
                out.push((si as u32, i as u32));
            });
            *parts[u].lock().unwrap_or_else(PoisonError::into_inner) = out;
        });
        if aborted.load(Relaxed) {
            return None;
        }
        Some(parts.into_iter().flat_map(|p| p.into_inner().unwrap_or_else(PoisonError::into_inner)).collect())
    }

    /// Order of two hits as the UI sorts by name: folders first, then
    /// case-insensitive name (`fsys::cmp_rows` with `SortKey::Name`),
    /// compared in place in the arenas.
    pub fn hit_cmp(&self, a: &Hit, b: &Hit, desc: bool) -> std::cmp::Ordering {
        let (sa, sb) = (&self.shards[a.0 as usize], &self.shards[b.0 as usize]);
        let (ia, ib) = (a.1 as usize, b.1 as usize);
        let (da, db) = (sa.entry_flags[ia] & DIR != 0, sb.entry_flags[ib] & DIR != 0);
        db.cmp(&da).then_with(|| {
            let o = fsys::cmp_names(sa.name(ia), sb.name(ib));
            if desc { o.reverse() } else { o }
        })
    }

    /// Keep only the `n` hits that come first in name order (in no
    /// particular order among themselves): linear time, no sorting.
    pub fn top_hits(&self, hits: &mut Vec<Hit>, n: usize, desc: bool) {
        if hits.len() > n {
            if n > 0 {
                hits.select_nth_unstable_by(n - 1, |a, b| self.hit_cmp(a, b, desc));
            }
            hits.truncate(n);
        }
    }

    /// Result rows for hits (names and folders copied out of the index).
    pub fn hit_rows(&self, hits: &[Hit]) -> Vec<Row> {
        let mut caches: Vec<Option<(u32, Arc<[u8]>)>> = vec![None; self.shards.len()];
        hits.iter().map(|&(s, i)| self.shards[s as usize].row(i as usize, &mut caches[s as usize])).collect()
    }

    // ------------------------------------------------------------ freshness

    /// Folders whose ctime differs from the one recorded, or that are gone:
    /// the only places a names-only index can be stale. One `statx` per
    /// folder, on `workers` threads; `init` runs on each (to lower its
    /// priority) and `pace` every few hundred folders (to pause).
    pub fn sweep(&self, workers: usize, cancel: &AtomicBool, init: &(dyn Fn() + Sync), pace: &(dyn Fn() + Sync)) -> Vec<Arc<[u8]>> {
        let live: Vec<(u32, u32)> = (0..self.shards.len())
            .flat_map(|s| (0..self.shards[s].ndirs()).filter(move |&d| !self.dead[s][d]).map(move |d| (s as u32, d as u32)))
            .collect();
        let next = AtomicUsize::new(0);
        let found = Mutex::new(Vec::new());
        const BATCH: usize = 256;
        std::thread::scope(|sc| {
            for _ in 0..workers.max(1) {
                sc.spawn(|| {
                    init();
                    let mut mine = Vec::new();
                    loop {
                        let at = next.fetch_add(BATCH, Relaxed);
                        if at >= live.len() || cancel.load(Relaxed) {
                            break;
                        }
                        pace();
                        for &(s, d) in &live[at..(at + BATCH).min(live.len())] {
                            let shard = &self.shards[s as usize];
                            let path = shard.dir_path(d as usize);
                            let current = sys::lstat(path).filter(|m| m.flags & DIR != 0).map(|m| m.ctime);
                            if current != Some(shard.dir_ctime[d as usize]) {
                                mine.push(Arc::from(path));
                            }
                        }
                    }
                    found.lock().unwrap_or_else(PoisonError::into_inner).extend(mine);
                });
            }
        });
        found.into_inner().unwrap_or_else(PoisonError::into_inner)
    }

    /// The folder map, computed from a shared reference: build it under a
    /// read lock and `install_map` it with a brief write lock, rather than
    /// letting the first update build it while holding the write lock.
    pub fn build_map(&self) -> FolderMap {
        let mut map = FolderMap::new();
        for (s, shard) in self.shards.iter().enumerate() {
            for d in 0..shard.ndirs() {
                if !self.dead[s][d] {
                    map.insert(shard.dir_path(d).into(), (s as u32, d as u32));
                }
            }
        }
        map
    }

    /// Use a map from `build_map` (of this same index, unchanged since).
    pub fn install_map(&mut self, map: FolderMap) {
        self.map = Some(map);
    }

    pub fn has_map(&self) -> bool {
        self.map.is_some()
    }

    fn map(&mut self) -> &mut FolderMap {
        if self.map.is_none() {
            self.map = Some(self.build_map());
        }
        self.map.as_mut().unwrap()
    }

    fn kill(&mut self, s: u32, d: u32) {
        let (s, d) = (s as usize, d as usize);
        if !self.dead[s][d] {
            self.dead[s][d] = true;
            let (a, b) = self.shards[s].range(d);
            self.entries -= b - a;
            self.dead_entries += b - a;
            self.dirs -= 1;
        }
    }

    fn overlay_shard(&mut self) -> usize {
        *self.overlay.get_or_insert_with(|| {
            self.shards.push(Shard::default());
            self.dead.push(Vec::new());
            self.shards.len() - 1
        })
    }

    /// Replace the listings of re-read folders. Folders that vanished take
    /// their subtrees with them; subfolders that are new are returned, to be
    /// crawled (`crawl`) and added (`add_shards`).
    pub fn apply(&mut self, scans: Vec<DirScan>, skip: &HashSet<Vec<u8>>) -> Vec<Arc<[u8]>> {
        let mut pending = Vec::new();
        let mut doomed: HashSet<Vec<u8>> = HashSet::new();
        for scan in scans {
            let old = self.map().get(&*scan.path).copied();
            let old_subdirs: HashSet<Box<[u8]>> = match old {
                Some((s, d)) => {
                    let shard = &self.shards[s as usize];
                    let (a, b) = shard.range(d as usize);
                    let subs = (a..b).filter(|&i| shard.entry_flags[i] & (DIR | LINK) == DIR).map(|i| shard.name(i).into()).collect();
                    self.kill(s, d);
                    self.map().remove(&*scan.path);
                    subs
                }
                None => HashSet::new(),
            };
            let Some((ctime, entries)) = scan.listing else {
                doomed.insert(scan.path.to_vec());
                continue;
            };
            let mut o = self.overlay_shard();
            if !self.shards[o].begin_dir(&scan.path, ctime) {
                self.overlay = None;
                o = self.overlay_shard();
                self.shards[o].begin_dir(&scan.path, ctime);
            }
            let mut new_subdirs = HashSet::new();
            for (name, flags) in &entries {
                self.shards[o].push(name, *flags);
                if flags & (DIR | LINK) == DIR {
                    new_subdirs.insert(name.clone());
                }
            }
            let d = self.shards[o].ndirs() - 1;
            self.dead[o].push(false);
            self.entries += entries.len();
            self.dirs += 1;
            self.map().insert(scan.path.to_vec().into(), (o as u32, d as u32));
            for name in old_subdirs.difference(&new_subdirs) {
                doomed.insert(fsys::join(&scan.path, name));
            }
            for name in new_subdirs.difference(&old_subdirs) {
                let full = fsys::join(&scan.path, name);
                if !skip.contains(&full) && !self.map().contains_key(&*full) {
                    pending.push(Arc::from(full));
                }
            }
        }
        // Each doomed subtree is a range of the ordered map: cost in
        // proportion to what goes, not to the size of the index.
        for root in &doomed {
            let victims = subtree(self.map(), root);
            for (k, (s, d)) in victims {
                self.kill(s, d);
                self.map().remove(&k);
            }
        }
        self.generation += 1;
        pending
    }

    /// Add freshly crawled subtrees (from `apply`'s pending folders).
    pub fn add_shards(&mut self, shards: Vec<Shard>, errors: usize) {
        for shard in shards {
            let s = self.shards.len() as u32;
            for d in 0..shard.ndirs() {
                let path: Box<[u8]> = shard.dir_path(d).into();
                if let Some((os, od)) = self.map().insert(path, (s, d as u32)) {
                    self.kill(os, od);
                }
            }
            self.entries += shard.len();
            self.dirs += shard.ndirs();
            self.dead.push(vec![false; shard.ndirs()]);
            self.shards.push(shard);
        }
        self.errors += errors;
        self.generation += 1;
    }

    /// Whether dead listings take up enough room to be worth compacting
    /// (saving a snapshot compacts, and reloading maps the compact copy).
    pub fn needs_compaction(&self) -> bool {
        self.dead_entries > 64 * 1024 && self.dead_entries > self.entries / 4
    }

    // ------------------------------------------------------------ snapshots

    /// All live listings in one shard, in folder order.
    fn compacted(&self) -> Shard {
        let mut out = Shard::default();
        for (s, shard) in self.shards.iter().enumerate() {
            for d in 0..shard.ndirs() {
                if self.dead[s][d] {
                    continue;
                }
                out.begin_dir(shard.dir_path(d), shard.dir_ctime[d]);
                let (a, b) = shard.range(d);
                for i in a..b {
                    out.push(shard.name(i), shard.entry_flags[i]);
                }
            }
        }
        out
    }

    /// Write the live index to `path` as a snapshot (compacting it).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        self.save_image().write(path)
    }

    /// The compacted snapshot contents, in memory: take this under the index
    /// lock, then `write` it with the lock released (disk I/O and fsync
    /// shouldn't hold up updates, or the queries queued behind them).
    pub fn save_image(&self) -> SaveImage {
        let c = self.compacted();
        let info = snapshot::Info {
            created_ns: now_ns(),
            entries: c.len() as u64,
            dirs: c.ndirs() as u64,
            errors: self.errors as u64,
            build_ns: self.build_time.as_nanos() as u64,
        };
        SaveImage { root: self.root.clone(), shard: c, info }
    }

    /// Map a snapshot. Its data is as fresh as when it was written; `sweep`
    /// finds what changed since.
    pub fn load(path: &Path) -> Result<Index, String> {
        let l = Loaded::open(path)?;
        let root = l.buf::<u8>(0)?;
        let shard = Shard {
            names: l.buf(1)?,
            lower: l.buf(2)?,
            starts: l.buf(3)?,
            entry_dir: l.buf(4)?,
            entry_flags: l.buf(5)?,
            dir_paths: l.buf(6)?,
            dir_starts: l.buf(7)?,
            ranges: l.buf(8)?,
            dir_ctime: l.buf(9)?,
            meta: Vec::new(),
        };
        shard.validate()?;
        if shard.len() as u64 != l.info.entries || shard.ndirs() as u64 != l.info.dirs {
            return Err("snapshot is damaged (counts)".into());
        }
        let mut index = Index::from_shards(root.to_vec().into(), vec![shard], l.info.errors as usize, Duration::from_nanos(l.info.build_ns));
        index.created_ns = l.info.created_ns;
        index.from_snapshot = true;
        Ok(index)
    }
}

/// The folder at `root` and every folder beneath it. Keys under `root/` sort
/// together between `root/` and `root0` ('0' follows '/'), apart from names
/// like `root b` that sort between `root` and `root/`.
fn subtree(map: &FolderMap, root: &[u8]) -> Vec<(Box<[u8]>, DirRef)> {
    // `/` (the only key ending in a separator) falls inside its own range.
    let (trimmed, exact) = match root.strip_suffix(b"/") {
        Some(t) => (t, None),
        None => (root, map.get(root)),
    };
    let mut out: Vec<(Box<[u8]>, DirRef)> = exact.map(|&v| (Box::from(root), v)).into_iter().collect();
    let lo = [trimmed, b"/"].concat();
    let hi = [trimmed, b"0"].concat();
    out.extend(map.range::<[u8], _>((Bound::Included(&lo[..]), Bound::Excluded(&hi[..]))).map(|(k, v)| (k.clone(), *v)));
    out
}

/// A snapshot ready to be written (see `Index::save_image`).
pub struct SaveImage {
    root: Arc<[u8]>,
    shard: Shard,
    info: snapshot::Info,
}

impl SaveImage {
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let c = &self.shard;
        let secs: [&[u8]; snapshot::SECTIONS] = [
            &self.root,
            &c.names,
            &c.lower,
            snapshot::section(&c.starts),
            snapshot::section(&c.entry_dir),
            &c.entry_flags,
            &c.dir_paths,
            snapshot::section(&c.dir_starts),
            snapshot::section(&c.ranges),
            snapshot::section(&c.dir_ctime),
        ];
        snapshot::write(path, self.info, &secs)
    }
}

// ------------------------------------------------------------ crawling

/// How a crawl runs.
pub struct Crawl<'a> {
    pub workers: usize,
    pub cancel: &'a AtomicBool,
    pub skip: &'a HashSet<Vec<u8>>,
    pub with_meta: bool,
    /// Entries seen so far, for progress display.
    pub seen: &'a AtomicUsize,
    /// Runs once on each crawler thread (to lower its priority).
    pub init: &'a (dyn Fn() + Sync),
    /// Runs before each folder (to pause while the machine is busy).
    pub pace: &'a (dyn Fn() + Sync),
}

struct Worker {
    shard: Shard,
    errors: usize,
    unflushed: usize,
}

/// Crawl everything under `roots`; one shard per worker thread.
pub fn crawl(roots: &[Arc<[u8]>], c: &Crawl) -> (Vec<Shard>, usize) {
    let (workers, failed) = walk::walk(
        roots,
        c.workers.max(1),
        c.cancel,
        c.skip,
        c.pace,
        || {
            (c.init)();
            Worker { shard: Shard::default(), errors: 0, unflushed: 0 }
        },
        |w, path, visit| match visit {
            Visit::Unreadable => w.errors += 1,
            Visit::Begin(dir) => {
                let ctime = dir.stat_self().map_or(0, |m| m.ctime);
                if !w.shard.begin_dir(path, ctime) {
                    w.errors += 1;
                }
            }
            Visit::Entry(dir, e) => {
                if w.shard.push(e.name, e.flags) && c.with_meta {
                    let m = dir.stat(e.name).unwrap_or_default();
                    w.shard.meta.push((m.size, m.mtime));
                }
                w.unflushed += 1;
                if w.unflushed == 4096 {
                    c.seen.fetch_add(4096, Relaxed);
                    w.unflushed = 0;
                }
            }
        },
    );
    let mut errors = failed;
    let shards = workers
        .into_iter()
        .map(|w| {
            errors += w.errors;
            w.shard
        })
        .collect();
    (shards, errors)
}

/// Crawl `root` into a new index. `None` if cancelled.
pub fn build(root: &[u8], c: &Crawl) -> Option<Index> {
    let started = Instant::now();
    let root: Arc<[u8]> = root.into();
    let (shards, errors) = crawl(std::slice::from_ref(&root), c);
    if c.cancel.load(Relaxed) {
        return None;
    }
    Some(Index::from_shards(root, shards, errors, started.elapsed()))
}

/// An in-progress crawl started by the UI, on all cores at normal priority.
pub struct Build {
    pub root: Arc<[u8]>,
    cancel: AtomicBool,
    pub seen: AtomicUsize,
    with_meta: bool,
}

impl Build {
    pub fn new(root: &[u8], with_meta: bool) -> Arc<Build> {
        Arc::new(Build { root: root.into(), cancel: AtomicBool::new(false), seen: AtomicUsize::new(0), with_meta })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn run(&self) -> Option<Index> {
        let skip = sys::crawl_skips();
        build(
            &self.root,
            &Crawl {
                workers: walk::cores(),
                cancel: &self.cancel,
                skip: &skip,
                with_meta: self.with_meta,
                seen: &self.seen,
                init: &|| {},
                pace: &|| {},
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    fn names(index: &Index, query: &str, regex: bool, scope: &[u8]) -> Vec<Vec<u8>> {
        let m = Matcher::new(query, regex).unwrap().unwrap();
        let (rows, total) = index.query(&m, scope, usize::MAX, &|| false).unwrap();
        assert_eq!(rows.len(), total);
        let mut found: Vec<Vec<u8>> = rows.into_iter().map(|r| r.name.into_vec()).collect();
        found.sort();
        found
    }

    fn scratch(tag: &str) -> (std::path::PathBuf, Vec<u8>) {
        let root = std::env::temp_dir().join(format!("fex-index-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let r = root.as_os_str().as_bytes().to_vec();
        (root, r)
    }

    #[test]
    fn finds_exact_names_in_case_sensitive_trees() {
        let (root, r) = scratch("names");
        for d in ["Foo/deep", "foo"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("Foo/deep/Report.TXT"), b"1").unwrap();
        fs::write(root.join("foo/report.txt"), b"22").unwrap();
        fs::write(root.join(sys::os(b"two\nlines.txt")), b"").unwrap();
        fs::write(root.join(sys::os(b"latin\xe9.txt")), b"").unwrap();
        fs::write(root.join("libz.so.1"), b"").unwrap();

        for with_meta in [false, true] {
            let index = Build::new(&r, with_meta).run().unwrap();
            assert_eq!(index.entries, 8);
            assert_eq!(index.dirs, 4);
            assert_eq!(names(&index, "report", false, &r), [b"Report.TXT".to_vec(), b"report.txt".to_vec()]);
            assert_eq!(names(&index, "report", false, &fsys::join(&r, b"foo")), [b"report.txt".to_vec()]);
            assert_eq!(names(&index, "lines", false, &r), [b"two\nlines.txt".to_vec()]);
            assert_eq!(names(&index, "latin", false, &r), [b"latin\xe9.txt".to_vec()]);
            assert_eq!(names(&index, "lib*.so.?", false, &r), [b"libz.so.1".to_vec()]);
            assert_eq!(names(&index, "^lib.*\\.so\\.[0-9]$", true, &r), [b"libz.so.1".to_vec()]);
            assert_eq!(names(&index, "type:txt", false, &r).len(), 4);
            assert_eq!(names(&index, "type:TXT", false, &r).len(), 4);
            assert_eq!(names(&index, "type:1", false, &r), [b"libz.so.1".to_vec()]);
            assert_eq!(names(&index, "foo", false, &r), [b"Foo".to_vec(), b"foo".to_vec()]);
            if with_meta {
                let m = Matcher::new("report.txt", false).unwrap().unwrap();
                let (rows, _) = index.query(&m, &fsys::join(&r, b"foo"), 10, &|| false).unwrap();
                assert_eq!(rows[0].size, 2);
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn subtrees_are_exact_ranges() {
        let mut map = FolderMap::new();
        for (i, k) in [&b"/a/b"[..], b"/a/b c", b"/a/b-x", b"/a/b/x", b"/a/b/x/y", b"/a/b0", b"/a/bc", b"/a"].iter().enumerate() {
            map.insert(Box::from(*k), (0, i as u32));
        }
        let mut got: Vec<Vec<u8>> = subtree(&map, b"/a/b").into_iter().map(|(k, _)| k.into_vec()).collect();
        got.sort();
        assert_eq!(got, [b"/a/b".to_vec(), b"/a/b/x".to_vec(), b"/a/b/x/y".to_vec()]);
        assert_eq!(subtree(&map, b"/").len(), map.len());
        map.insert(Box::from(&b"/"[..]), (1, 0));
        assert_eq!(subtree(&map, b"/").len(), map.len());
        assert!(subtree(&map, b"/nope").is_empty());
    }

    /// Every name under `root` per a fresh crawl, for comparisons.
    fn all(index: &Index, r: &[u8]) -> Vec<Vec<u8>> {
        let m = Matcher::new("*", false).unwrap().unwrap();
        let (rows, _) = index.query(&m, r, usize::MAX, &|| false).unwrap();
        let mut v: Vec<Vec<u8>> = rows.iter().map(|row| row.full_path(b"")).collect();
        v.sort();
        v
    }

    fn refresh(index: &mut Index) -> usize {
        let cancel = AtomicBool::new(false);
        let skip = HashSet::new();
        let changed = index.sweep(2, &cancel, &|| {}, &|| {});
        let n = changed.len();
        let pending = index.apply(changed.into_iter().map(read_dir).collect(), &skip);
        let seen = AtomicUsize::new(0);
        let (shards, errors) = crawl(&pending, &Crawl { workers: 2, cancel: &cancel, skip: &skip, with_meta: false, seen: &seen, init: &|| {}, pace: &|| {} });
        index.add_shards(shards, errors);
        n
    }

    #[test]
    fn snapshot_round_trip_then_sweep_catches_every_change() {
        let (root, r) = scratch("fresh");
        fs::create_dir_all(root.join("a/empty")).unwrap();
        fs::create_dir_all(root.join("gone/deep/deeper")).unwrap();
        fs::write(root.join("a/one.txt"), b"1").unwrap();
        fs::write(root.join("gone/deep/deeper/x.txt"), b"1").unwrap();
        let built = Build::new(&r, false).run().unwrap();
        let snap = root.with_extension("snap");
        built.save(&snap).unwrap();
        let mut index = Index::load(&snap).unwrap();
        assert_eq!(all(&index, &r), all(&built, &r));
        assert_eq!((index.entries, index.dirs), (built.entries, built.dirs));
        assert!(index.mapped_bytes() > 0 && index.bytes() == 0);

        // Nothing changed: nothing to re-read.
        assert_eq!(refresh(&mut index), 0);

        // A file in an empty folder, a removed subtree, a new subtree, a rename.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.join("a/empty/new.txt"), b"").unwrap();
        fs::remove_dir_all(root.join("gone")).unwrap();
        fs::create_dir_all(root.join("fresh/inner")).unwrap();
        fs::write(root.join("fresh/inner/four.txt"), b"").unwrap();
        fs::rename(root.join("a/one.txt"), root.join("a/uno.txt")).unwrap();
        assert!(refresh(&mut index) >= 3);
        let truth = Build::new(&r, false).run().unwrap();
        assert_eq!(all(&index, &r), all(&truth, &r));
        assert_eq!((index.entries, index.dirs), (truth.entries, truth.dirs));

        // Saving compacts; the reloaded copy matches too.
        index.save(&snap).unwrap();
        let reloaded = Index::load(&snap).unwrap();
        assert_eq!(all(&reloaded, &r), all(&truth, &r));
        assert_eq!(reloaded.dirs, truth.dirs);

        // A damaged snapshot is refused, not trusted.
        let mut raw = fs::read(&snap).unwrap();
        let n = raw.len();
        raw[n - 20] ^= 0xFF;
        raw[200] ^= 0x7F;
        fs::write(&snap, &raw).unwrap();
        let _ = Index::load(&snap).map(|i| all(&i, &r));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(snap);
    }
}
