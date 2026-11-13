//! Directory listing model and path utilities (the Linux side of `fsys.rs`).
//!
//! Names are bytes and paths are compared exactly: Linux filesystems are
//! case-sensitive, so `Foo` and `foo` are two different entries.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use super::sys::{self, Dir, DIR, LINK};
use crate::pool;

#[derive(Clone)]
pub struct Row {
    pub name: Box<[u8]>,
    /// Parent folder for search results; `None` means the current folder.
    pub dir: Option<Arc<[u8]>>,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime: i64,
    pub flags: u8,
    pub sel: bool,
    /// Position in a result set as it arrived, so metadata filled in later
    /// finds its row after sorting. 0 outside search results.
    pub id: u32,
}

impl Row {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & DIR != 0
    }

    #[inline]
    pub fn is_link(&self) -> bool {
        self.flags & LINK != 0
    }

    /// Dotfiles are hidden by convention.
    #[inline]
    pub fn is_hidden(&self) -> bool {
        self.name.first() == Some(&b'.')
    }

    pub fn display(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.name)
    }

    pub fn full_path(&self, cwd: &[u8]) -> Vec<u8> {
        join(self.dir.as_deref().unwrap_or(cwd), &self.name)
    }

    pub fn type_name(&self) -> String {
        if self.is_dir() {
            return if self.is_link() { "Folder link" } else { "Folder" }.into();
        }
        if self.is_link() {
            return "Link".into();
        }
        if self.flags & sys::SPECIAL != 0 {
            return "Special file".into();
        }
        match ext(&self.name) {
            b"" => "File".into(),
            e => format!("{} file", String::from_utf8_lossy(e).to_uppercase()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Modified,
    Type,
    Size,
}

/// Case-insensitive name order without allocating for ASCII names (the
/// common case); anything else compares Unicode-lowercased text.
pub fn cmp_names(a: &[u8], b: &[u8]) -> Ordering {
    for (&x, &y) in a.iter().zip(b) {
        if x >= 0x80 || y >= 0x80 {
            return cmp_names_unicode(a, b);
        }
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    let n = a.len().min(b.len());
    if a[n..].iter().chain(&b[n..]).any(|&c| c >= 0x80) {
        return cmp_names_unicode(a, b);
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_names_unicode(a: &[u8], b: &[u8]) -> Ordering {
    let (sa, sb) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    let mut ai = sa.chars().flat_map(char::to_lowercase);
    let mut bi = sb.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x != y => return x.cmp(&y),
            _ => {}
        }
    }
}

/// Extension without the dot; a leading dot (`.bashrc`) is not one.
pub fn ext(name: &[u8]) -> &[u8] {
    match memchr::memrchr(b'.', name) {
        Some(i) if i > 0 => &name[i + 1..],
        _ => b"",
    }
}

/// Folders first, then by `key`, ties broken by name.
pub fn cmp_rows(a: &Row, b: &Row, key: SortKey, descending: bool) -> Ordering {
    let dirs = b.is_dir().cmp(&a.is_dir());
    if dirs != Ordering::Equal {
        return dirs;
    }
    let ord = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Modified => a.mtime.cmp(&b.mtime),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Type => cmp_names(ext(&a.name), ext(&b.name)),
    }
    .then_with(|| cmp_names(&a.name, &b.name));
    if descending { ord.reverse() } else { ord }
}

pub fn sort_rows(rows: &mut [Row], key: SortKey, descending: bool) {
    rows.sort_unstable_by(|a, b| cmp_rows(a, b, key, descending));
}

/// Folders above this size get their entries statted on the thread pool.
const PARALLEL_STAT: usize = 2048;

/// List one folder with sizes and dates. Names come from one getdents pass;
/// each entry then costs a `statx` relative to the open folder, spread over
/// the pool for big folders. Links to folders count as folders.
pub fn list_dir(path: &[u8]) -> Result<Vec<Row>, String> {
    let fail = |e: i32| format!("{}: {}", String::from_utf8_lossy(path), sys::error_text(e));
    let dir = Dir::open(path).map_err(fail)?;
    let mut rows = Vec::new();
    dir.scan(&mut Vec::new(), |e| {
        rows.push(Row { name: e.name.into(), dir: None, size: 0, mtime: 0, flags: e.flags, sel: false, id: 0 });
    })
    .map_err(fail)?;
    let fill = |row: &mut Row| {
        if let Some(m) = dir.stat(&row.name) {
            row.size = if m.flags & DIR != 0 { 0 } else { m.size };
            row.mtime = m.mtime;
        }
        if row.is_link() && dir.link_is_dir(&row.name) {
            row.flags |= DIR;
        }
    };
    if rows.len() < PARALLEL_STAT || pool::threads() < 2 {
        rows.iter_mut().for_each(fill);
    } else {
        let chunks: Vec<Mutex<&mut [Row]>> = rows.chunks_mut(512).map(Mutex::new).collect();
        pool::for_each(chunks.len(), &|i| {
            chunks[i].lock().unwrap_or_else(PoisonError::into_inner).iter_mut().for_each(fill);
        });
    }
    Ok(rows)
}

/// Fill in sizes and dates for rows that carry their own folder (search
/// results), on the pool. Consecutive rows from the same folder share one
/// open handle, so most lookups are relative.
pub fn fill_meta(rows: &mut [Row]) {
    let chunks: Vec<Mutex<&mut [Row]>> = rows.chunks_mut(1024).map(Mutex::new).collect();
    pool::for_each(chunks.len(), &|i| {
        let mut chunk = chunks[i].lock().unwrap_or_else(PoisonError::into_inner);
        let mut open: Option<(Arc<[u8]>, Option<Dir>)> = None;
        for row in chunk.iter_mut() {
            let Some(parent) = row.dir.clone() else { continue };
            if !open.as_ref().is_some_and(|(p, _)| Arc::ptr_eq(p, &parent)) {
                let d = Dir::open(&parent).ok();
                open = Some((parent, d));
            }
            if let Some((_, Some(d))) = &open
                && let Some(m) = d.stat(&row.name)
            {
                row.size = if m.flags & DIR != 0 { 0 } else { m.size };
                row.mtime = m.mtime;
            }
        }
    });
}

pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(dir.len() + name.len() + 1);
    s.extend_from_slice(dir);
    if !dir.ends_with(b"/") {
        s.push(b'/');
    }
    s.extend_from_slice(name);
    s
}

/// Parent folder; `/` has none.
pub fn parent(path: &[u8]) -> Option<Vec<u8>> {
    let p = trim_slashes(path);
    if p.is_empty() {
        return None;
    }
    let i = memchr::memrchr(b'/', p)?;
    Some(if i == 0 { b"/".to_vec() } else { p[..i].to_vec() })
}

fn trim_slashes(p: &[u8]) -> &[u8] {
    let mut end = p.len();
    while end > 0 && p[end - 1] == b'/' {
        end -= 1;
    }
    &p[..end]
}

/// Last component of a path, for labels (`/` stays `/`).
pub fn base_name(path: &[u8]) -> &[u8] {
    let p = trim_slashes(path);
    match memchr::memrchr(b'/', p) {
        Some(i) => &p[i + 1..],
        None if p.is_empty() => b"/",
        None => p,
    }
}

/// Whether `path` is `root` or lies beneath it (exact, case-sensitive).
pub fn in_scope(path: &[u8], root: &[u8]) -> bool {
    let (p, r) = (trim_slashes(path), trim_slashes(root));
    p.starts_with(r) && (p.len() == r.len() || p[r.len()] == b'/')
}

/// Normalize user-typed input relative to `cwd`: strips quotes, expands `~`
/// and `$VAR`, resolves `.` and `..` lexically (as the shell's `cd` does).
pub fn resolve_input(input: &str, cwd: &[u8]) -> Vec<u8> {
    let s = input.trim().trim_matches(|c| c == '"' || c == '\'');
    let mut s = expand_env(s).into_bytes();
    if s == b"~" || s.starts_with(b"~/") {
        s = [sys::home().as_slice(), &s[1..]].concat();
    }
    let abs = if s.starts_with(b"/") { s } else { join(cwd, &s) };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in abs.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = Vec::with_capacity(abs.len());
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// Expand `$VAR` and `${VAR}`; unknown variables are left as typed.
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, used) = if let Some(inner) = after.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => (&inner[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
            (&after[..end], end)
        };
        match std::env::var(name).ok().filter(|_| !name.is_empty()) {
            Some(v) => {
                out.push_str(&v);
                rest = &after[used..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        assert_eq!(parent(b"/a/b").as_deref(), Some(&b"/a"[..]));
        assert_eq!(parent(b"/a/").as_deref(), Some(&b"/"[..]));
        assert_eq!(parent(b"/"), None);
        assert_eq!(base_name(b"/a/b/"), b"b");
        assert_eq!(base_name(b"/"), b"/");
    }

    #[test]
    fn scope_is_case_sensitive() {
        assert!(in_scope(b"/data/Foo/x", b"/data/Foo"));
        assert!(!in_scope(b"/data/foo/x", b"/data/Foo"));
        assert!(!in_scope(b"/data/Foobar", b"/data/Foo"));
        assert!(in_scope(b"/anything", b"/"));
    }

    #[test]
    fn resolve() {
        assert_eq!(resolve_input("..", b"/usr/lib"), b"/usr");
        assert_eq!(resolve_input("../../..", b"/usr/lib"), b"/");
        assert_eq!(resolve_input("share/./doc/", b"/usr"), b"/usr/share/doc");
        assert_eq!(resolve_input("'/tmp'", b"/"), b"/tmp");
    }

    #[test]
    fn names_sort_case_insensitively() {
        let mut v: Vec<&[u8]> = vec![b"b", b"A", b"a", b"\xc3\x89t\xc3\xa9", b"e"];
        v.sort_by(|a, b| cmp_names(a, b));
        assert_eq!(v, vec![&b"A"[..], b"a", b"b", b"e", b"\xc3\x89t\xc3\xa9"]);
    }

    #[test]
    fn extensions() {
        assert_eq!(ext(b"a.tar.gz"), b"gz");
        assert_eq!(ext(b".bashrc"), b"");
        assert_eq!(ext(b"Makefile"), b"");
    }
}
