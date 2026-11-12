//! Parallel crawler and in-memory name index (the Linux side of `index.rs`).
//!
//! Same layout and query strategy as the Windows index: one shard per
//! crawler thread, all names of a shard in one contiguous arena plus an
//! ASCII-lowercased twin, SIMD `memmem` for plain keywords, the regex run
//! over the whole arena otherwise, and 32K-entry work units on the pool.
//!
//! Linux-specific choices:
//! * Names are bytes separated by NUL, the one byte no Linux name can hold,
//!   so every name is stored exactly (newlines and invalid UTF-8 included).
//!   Regexes use NUL as their line terminator, so `^`/`$` anchor per name.
//! * By default the crawl records names and types only: getdents gives
//!   those for free, while sizes and dates would cost a `statx` per entry
//!   (about 3x the crawl). Results get their metadata after the query
//!   (`fsys::fill_meta`), which touches only what is shown. `with_meta`
//!   records it during the crawl instead, for comparison (`fex --bench`).
//! * Folder identity is exact: `Foo/` and `foo/` are different folders.
//!
//! Not yet ported: live freshness (needs inotify per folder or fanotify,
//! see the discovery notes) and saving indexes to disk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};

use super::fsys::{self, Row};
use super::sys::{self, DIR};
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

#[derive(Clone, Copy)]
struct Entry {
    dir: u32,
    flags: u8,
}

#[derive(Default)]
struct Shard {
    /// `name0\0name1\0...`
    names: Vec<u8>,
    /// `names` with ASCII letters lowercased (same offsets).
    lower: Vec<u8>,
    starts: Vec<u32>,
    entries: Vec<Entry>,
    /// (size, mtime) per entry; empty unless built `with_meta`.
    meta: Vec<(u64, i64)>,
    dirs: Vec<Arc<[u8]>>,
    ranges: Vec<(usize, usize)>,
}

impl Shard {
    fn push(&mut self, name: &[u8], e: Entry) -> bool {
        if self.names.len() + name.len() + 1 > u32::MAX as usize {
            return false;
        }
        self.starts.push(self.names.len() as u32);
        self.names.extend_from_slice(name);
        self.names.push(0);
        self.lower.extend(name.iter().map(u8::to_ascii_lowercase));
        self.lower.push(0);
        self.entries.push(e);
        true
    }

    fn begin_dir(&mut self, dir: Arc<[u8]>) -> u32 {
        self.end_dir();
        self.dirs.push(dir);
        self.ranges.push((self.entries.len(), self.entries.len()));
        (self.dirs.len() - 1) as u32
    }

    fn end_dir(&mut self) {
        let end = self.entries.len();
        if let Some(r) = self.ranges.last_mut() {
            r.1 = end;
        }
    }

    #[inline]
    fn end_of(&self, i: usize) -> usize {
        self.starts.get(i).map_or(self.names.len(), |&s| s as usize)
    }

    #[inline]
    fn name(&self, i: usize) -> &[u8] {
        &self.names[self.starts[i] as usize..self.end_of(i + 1) - 1]
    }

    fn row(&self, i: usize) -> Row {
        let e = self.entries[i];
        let (size, mtime) = self.meta.get(i).copied().unwrap_or((0, 0));
        Row { name: self.name(i).into(), dir: Some(self.dirs[e.dir as usize].clone()), size, mtime, flags: e.flags, sel: false }
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
            if m.is_match_entry(self.name(i), self.entries[i].flags & DIR != 0) {
                hit(i);
            }
            pos = self.end_of(i + 1);
        }
    }

    fn bytes(&self) -> usize {
        self.names.capacity()
            + self.lower.capacity()
            + self.starts.capacity() * 4
            + self.entries.capacity() * size_of::<Entry>()
            + self.meta.capacity() * size_of::<(u64, i64)>()
            + self.ranges.capacity() * size_of::<(usize, usize)>()
            + self.dirs.iter().map(|d| d.len() + 16).sum::<usize>()
    }
}

const CHUNK: usize = 32 * 1024;

pub struct Index {
    pub root: Arc<[u8]>,
    shards: Vec<Shard>,
    pub entries: usize,
    pub dirs: usize,
    pub errors: usize,
    pub build_time: Duration,
    pub with_meta: bool,
}

impl Index {
    pub fn covers(&self, path: &[u8]) -> bool {
        fsys::in_scope(path, &self.root)
    }

    /// Approximate heap use.
    pub fn bytes(&self) -> usize {
        self.shards.iter().map(Shard::bytes).sum()
    }

    /// The folder with the most entries, and how many (for benchmarks).
    pub fn largest_dir(&self) -> Option<(Arc<[u8]>, usize)> {
        self.shards
            .iter()
            .flat_map(|s| s.ranges.iter().enumerate().map(move |(d, &(a, b))| (&s.dirs[d], b - a)))
            .max_by_key(|&(_, n)| n)
            .map(|(d, n)| (d.clone(), n))
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
            for (d, &(start, end)) in shard.ranges.iter().enumerate() {
                if start == end || !(everything || fsys::in_scope(&shard.dirs[d], scope)) {
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
        let units = self.query_units(scope);
        let total = AtomicUsize::new(0);
        let retained = AtomicUsize::new(0);
        let aborted = AtomicBool::new(false);
        let parts: Vec<Mutex<Vec<(usize, Row)>>> = units.iter().map(|_| Mutex::new(Vec::new())).collect();
        pool::for_each(units.len(), &|u| {
            if aborted.load(Relaxed) || cancelled() {
                aborted.store(true, Relaxed);
                return;
            }
            let (si, a, b) = units[u];
            let shard = &self.shards[si];
            let mut out = Vec::new();
            let mut count = 0;
            shard.scan(m, a, b, |i| {
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
        Some((rows.into_iter().map(|(_, r)| r).collect(), total.load(Relaxed)))
    }
}

/// An in-progress crawl. `seen` counts entries so far, for progress display.
pub struct Build {
    pub root: Arc<[u8]>,
    cancel: AtomicBool,
    pub seen: AtomicUsize,
    with_meta: bool,
}

struct Worker {
    shard: Shard,
    errors: usize,
    unflushed: usize,
}

impl Build {
    pub fn new(root: &[u8], with_meta: bool) -> Arc<Build> {
        Arc::new(Build { root: root.into(), cancel: AtomicBool::new(false), seen: AtomicUsize::new(0), with_meta })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    /// Crawl the root on all cores. `None` if cancelled.
    pub fn run(&self) -> Option<Index> {
        let started = Instant::now();
        let skip = sys::crawl_skips();
        let (workers, failed) = walk::walk(
            std::slice::from_ref(&self.root),
            walk::cores(),
            &self.cancel,
            &skip,
            || Worker { shard: Shard::default(), errors: 0, unflushed: 0 },
            |w, path, visit| match visit {
                Visit::Unreadable => w.errors += 1,
                Visit::Entry(dir, e) => {
                    if !w.shard.dirs.last().is_some_and(|d| Arc::ptr_eq(d, path)) {
                        w.shard.begin_dir(path.clone());
                    }
                    let id = (w.shard.dirs.len() - 1) as u32;
                    if w.shard.push(e.name, Entry { dir: id, flags: e.flags }) && self.with_meta {
                        let m = dir.stat(e.name).unwrap_or_default();
                        w.shard.meta.push((m.size, m.mtime));
                    }
                    w.unflushed += 1;
                    if w.unflushed == 4096 {
                        self.seen.fetch_add(4096, Relaxed);
                        w.unflushed = 0;
                    }
                }
            },
        );
        if self.cancel.load(Relaxed) {
            return None;
        }
        let mut shards = Vec::with_capacity(workers.len());
        let mut errors = failed;
        for mut w in workers {
            w.shard.end_dir();
            errors += w.errors;
            shards.push(w.shard);
        }
        let entries = shards.iter().map(|s| s.entries.len()).sum();
        let dirs = shards.iter().map(|s| s.dirs.len()).sum();
        Some(Index { root: self.root.clone(), shards, entries, dirs, errors, build_time: started.elapsed(), with_meta: self.with_meta })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn names(index: &Index, query: &str, regex: bool, scope: &[u8]) -> Vec<Vec<u8>> {
        let m = Matcher::new(query, regex).unwrap().unwrap();
        let (rows, total) = index.query(&m, scope, usize::MAX, &|| false).unwrap();
        assert_eq!(rows.len(), total);
        let mut found: Vec<Vec<u8>> = rows.into_iter().map(|r| r.name.into_vec()).collect();
        found.sort();
        found
    }

    #[test]
    fn finds_exact_names_in_case_sensitive_trees() {
        let root = std::env::temp_dir().join(format!("fex-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["Foo/deep", "foo"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("Foo/deep/Report.TXT"), b"1").unwrap();
        std::fs::write(root.join("foo/report.txt"), b"22").unwrap();
        std::fs::write(root.join(sys::os(b"two\nlines.txt")), b"").unwrap();
        std::fs::write(root.join(sys::os(b"latin\xe9.txt")), b"").unwrap();
        std::fs::write(root.join("libz.so.1"), b"").unwrap();
        let r = root.as_os_str().as_bytes().to_vec();

        for with_meta in [false, true] {
            let index = Build::new(&r, with_meta).run().unwrap();
            assert_eq!(index.entries, 8);
            assert_eq!(names(&index, "report", false, &r), [b"Report.TXT".to_vec(), b"report.txt".to_vec()]);
            // Scope is exact: only the lowercase folder.
            assert_eq!(names(&index, "report", false, &fsys::join(&r, b"foo")), [b"report.txt".to_vec()]);
            // Names are kept byte for byte.
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
        let _ = std::fs::remove_dir_all(root);
    }
}
