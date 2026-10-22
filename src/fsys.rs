//! Directory listing model and path utilities.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::win;

/// One row in the file table. Kept small: the name is a single allocation and
/// search results share their parent directory string via `Arc`.
#[derive(Clone)]
pub struct Row {
    pub name: Box<str>,
    /// Parent directory for search results; `None` means the current directory.
    pub dir: Option<Arc<str>>,
    pub size: u64,
    pub mtime: u64,
    pub attrs: u32,
    /// Selection lives on the row so it survives sorting and live updates.
    pub sel: bool,
}

impl Row {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attrs & win::ATTR_DIR != 0
    }

    #[inline]
    pub fn is_hidden(&self) -> bool {
        self.attrs & win::ATTR_HIDDEN != 0
    }

    pub fn full_path(&self, cwd: &str) -> String {
        join(self.dir.as_deref().unwrap_or(cwd), &self.name)
    }

    /// Explorer-style type description, derived from the extension.
    pub fn type_name(&self) -> String {
        if self.is_dir() {
            return "File folder".into();
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 && i + 1 < self.name.len() => format!("{} File", self.name[i + 1..].to_uppercase()),
            _ => "File".into(),
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

/// Case-insensitive comparison without allocating. ASCII names (the common
/// case) compare bytewise; anything else falls back to Unicode lowercase.
pub fn cmp_names(a: &str, b: &str) -> Ordering {
    for (&x, &y) in a.as_bytes().iter().zip(b.as_bytes()) {
        if x >= 0x80 || y >= 0x80 {
            return cmp_names_unicode(a, b);
        }
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    let (rest_a, rest_b) = (&a.as_bytes()[b.len().min(a.len())..], &b.as_bytes()[a.len().min(b.len())..]);
    if rest_a.iter().chain(rest_b).any(|&c| c >= 0x80) {
        return cmp_names_unicode(a, b);
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_names_unicode(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
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

fn ext(name: &str) -> &str {
    name.rfind('.').map_or("", |i| &name[i + 1..])
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

/// `sort_rows` for big result sets: chunks sorted on the thread pool, then
/// merged pairwise (each round's merges in parallel too).
pub fn sort_rows_par(rows: &mut Vec<Row>, key: SortKey, descending: bool) {
    use std::sync::{Mutex, PoisonError};
    let threads = crate::pool::threads();
    if rows.len() < 16 * 1024 || threads < 2 {
        return sort_rows(rows, key, descending);
    }
    let cmp = |a: &Row, b: &Row| cmp_rows(a, b, key, descending);
    let size = rows.len().div_ceil(threads);
    let mut all = std::mem::take(rows);
    let mut parts: Vec<Vec<Row>> = Vec::with_capacity(threads);
    while all.len() > size {
        let tail = all.split_off(all.len() - size);
        parts.push(tail);
    }
    parts.push(all);
    let chunks: Vec<Mutex<Vec<Row>>> = parts.into_iter().map(Mutex::new).collect();
    crate::pool::for_each(chunks.len(), &|i| chunks[i].lock().unwrap_or_else(PoisonError::into_inner).sort_unstable_by(cmp));
    let mut parts: Vec<Vec<Row>> = chunks.into_iter().map(|c| c.into_inner().unwrap_or_else(PoisonError::into_inner)).collect();
    while parts.len() > 1 {
        let odd = (parts.len() % 2 == 1).then(|| parts.pop()).flatten();
        let pairs: Vec<Mutex<(Vec<Row>, Vec<Row>)>> = {
            let mut it = parts.into_iter();
            std::iter::from_fn(|| Some(Mutex::new((it.next()?, it.next()?)))).collect()
        };
        crate::pool::for_each(pairs.len(), &|i| {
            let mut pair = pairs[i].lock().unwrap_or_else(PoisonError::into_inner);
            let (a, b) = std::mem::take(&mut *pair);
            pair.0 = merge(a, b, cmp);
        });
        parts = pairs.into_iter().map(|p| p.into_inner().unwrap_or_else(PoisonError::into_inner).0).collect();
        parts.extend(odd);
    }
    *rows = parts.pop().unwrap_or_default();
}

fn merge(a: Vec<Row>, b: Vec<Row>, cmp: impl Fn(&Row, &Row) -> Ordering) -> Vec<Row> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    loop {
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => out.extend(if cmp(y, x).is_lt() { b.next() } else { a.next() }),
            (Some(_), None) => {
                out.extend(a);
                return out;
            }
            (None, _) => {
                out.extend(b);
                return out;
            }
        }
    }
}

/// Whether `path` is `dir` joined with `name` (exactly, as `join` builds it),
/// without allocating the joined path.
pub fn is_path(path: &str, dir: &str, name: &str) -> bool {
    let sep = usize::from(!dir.ends_with('\\'));
    path.len() == dir.len() + sep + name.len()
        && path.starts_with(dir)
        && path.ends_with(name)
        && (sep == 0 || path.as_bytes()[dir.len()] == b'\\')
}

/// List one directory. `\\server` lists that server's shares.
pub fn list_dir(path: &str) -> Result<Vec<Row>, String> {
    if is_server_root(path) {
        let server = path.trim_end_matches('\\');
        return win::net_shares(server)
            .map(|shares| {
                shares
                    .into_iter()
                    .map(|s| Row { name: s.into(), dir: None, size: 0, mtime: 0, attrs: win::ATTR_DIR, sel: false })
                    .collect()
            })
            .map_err(|e| format!("{server}: {}", win::error_text(e)));
    }
    let mut rows = Vec::new();
    let mut name = String::new();
    win::scan_dir(path, |e| {
        name.clear();
        win::push_utf16(&mut name, e.name);
        rows.push(Row { name: name.as_str().into(), dir: None, size: e.size, mtime: e.mtime, attrs: e.attrs, sel: false });
    })
    .map_err(|e| format!("{path}: {}", win::error_text(e)))?;
    Ok(rows)
}

/// Whether `name` is safe as one component of a path FEx builds from
/// untrusted input (archive members, dropped files): not `.` or `..`; no
/// drive, alternate-stream (`:`) or wildcard syntax; no control characters;
/// no trailing dot or space (Windows strips them, so the file would land
/// under a different name than shown); not a device name such as `NUL` or
/// `COM1.txt`.
pub fn safe_component(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
        && !name.chars().any(char::is_control)
        && !name.ends_with([' ', '.'])
        && !device
}

/// `\\server` (no share component).
pub fn is_server_root(path: &str) -> bool {
    path.strip_prefix(r"\\")
        .map(|rest| rest.trim_end_matches('\\'))
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('\\') && rest != "?" && rest != ".")
}

pub fn join(dir: &str, name: &str) -> String {
    let mut s = String::with_capacity(dir.len() + name.len() + 1);
    s.push_str(dir);
    if !dir.ends_with('\\') {
        s.push('\\');
    }
    s.push_str(name);
    s
}

/// Parent directory. `C:\` and `\\server` have none; `\\server\share` → `\\server`.
pub fn parent(path: &str) -> Option<String> {
    let p = path.trim_end_matches('\\');
    if p.ends_with(':') {
        return None;
    }
    let i = p.rfind('\\')?;
    if i <= 1 {
        return None;
    }
    let head = &p[..i];
    Some(if head.ends_with(':') { format!("{head}\\") } else { head.to_string() })
}

/// Normalize user-typed input relative to `cwd`: strips quotes, accepts `/`,
/// expands `%VAR%`, resolves `..`, turns `C:` into `C:\`.
pub fn resolve_input(input: &str, cwd: &str) -> Option<String> {
    let s = expand_env(input.trim().trim_matches('"')).replace('/', "\\");
    if s.is_empty() {
        return None;
    }
    if s.starts_with(r"\\") {
        // UNC: keep as typed (GetFullPathName would mangle a bare \\server).
        let t = s.trim_end_matches('\\');
        return Some(if t.len() > 2 { t.to_string() } else { s });
    }
    let candidate = if s.len() == 2 && s.ends_with(':') {
        format!("{s}\\")
    } else if s.as_bytes().get(1) == Some(&b':') {
        s
    } else {
        join(cwd, &s)
    };
    let full = win::full_path(&candidate)?;
    Some(if full.len() > 3 { full.trim_end_matches('\\').to_string() } else { full })
}

pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => match std::env::var(&after[..end]) {
                Ok(v) => {
                    out.push_str(&v);
                    rest = &after[end + 1..];
                }
                Err(_) => {
                    out.push('%');
                    rest = after;
                }
            },
            None => {
                out.push_str(&rest[start..]);
                rest = "";
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
        assert_eq!(parent(r"C:\a\b").as_deref(), Some(r"C:\a"));
        assert_eq!(parent(r"C:\a").as_deref(), Some(r"C:\"));
        assert_eq!(parent(r"C:\"), None);
        assert_eq!(parent(r"\\srv\share\x").as_deref(), Some(r"\\srv\share"));
        assert_eq!(parent(r"\\srv\share").as_deref(), Some(r"\\srv"));
        assert_eq!(parent(r"\\srv"), None);
    }

    #[test]
    fn server_roots() {
        assert!(is_server_root(r"\\srv"));
        assert!(is_server_root(r"\\srv\"));
        assert!(!is_server_root(r"\\srv\share"));
        assert!(!is_server_root(r"C:\"));
    }

    #[test]
    fn resolve() {
        assert_eq!(resolve_input(r"..", r"C:\Windows\System32").as_deref(), Some(r"C:\Windows"));
        assert_eq!(resolve_input("c:/windows/", r"D:\").as_deref(), Some(r"c:\windows"));
        assert_eq!(resolve_input("C:", r"D:\").as_deref(), Some(r"C:\"));
        assert_eq!(resolve_input(r"\\srv\share\", r"C:\").as_deref(), Some(r"\\srv\share"));
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(50 * 1024 * 1024), "50 MB");
    }
}
