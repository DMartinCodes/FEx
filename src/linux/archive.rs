//! Read-only ZIP archive browsing and safe extraction (the Linux side of
//! `archive.rs`, same rules).
//!
//! An archive is browsed through a pseudo-location `zip:<archive>|<inner>`;
//! the archive part ends at the first `|` that follows an archive's name, so
//! folders with `|` in their names still work. Extraction never trusts the
//! archive: member names must stay inside the destination, symbolic links
//! are refused, nothing is overwritten, and each member may only produce as
//! many bytes as it declares (zip bombs stop there).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use zip::ZipArchive;

use super::fsys::{self, Row};
use super::ops;
use super::sys::{self, DIR};

static STAGE_ID: AtomicU64 = AtomicU64::new(0);

/// Opened members are extracted here; files older than this are cleaned up.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

const PREFIX: &[u8] = b"zip:";

pub fn is_location(path: &[u8]) -> bool {
    path.starts_with(PREFIX)
}

pub fn location(archive: &[u8], inner: &[u8]) -> Vec<u8> {
    [PREFIX, archive, b"|", trim(inner)].concat()
}

fn trim(inner: &[u8]) -> &[u8] {
    let mut s = inner;
    while let Some(r) = s.strip_prefix(b"/") {
        s = r;
    }
    while let Some(r) = s.strip_suffix(b"/") {
        s = r;
    }
    s
}

/// (archive path, member path without leading or trailing `/`).
pub fn parse_location(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = path.strip_prefix(PREFIX)?;
    let at = memchr::memchr_iter(b'|', rest).find(|&i| is_archive_file(&rest[..i]))?;
    Some((&rest[..at], trim(&rest[at + 1..])))
}

pub fn is_archive_file(path: &[u8]) -> bool {
    let ext = fsys::ext(fsys::base_name(path)).to_ascii_lowercase();
    matches!(ext.as_slice(), b"zip" | b"jar" | b"cbz" | b"epub")
}

pub fn root_location(path: &[u8]) -> Vec<u8> {
    location(path, b"")
}

pub fn parent_location(path: &[u8]) -> Option<Vec<u8>> {
    let (archive, inner) = parse_location(path)?;
    if inner.is_empty() {
        return fsys::parent(archive);
    }
    Some(location(archive, memchr::memrchr(b'/', inner).map_or(&b""[..], |i| &inner[..i])))
}

fn open(archive_path: &[u8]) -> Result<ZipArchive<File>, String> {
    let shown = String::from_utf8_lossy(archive_path);
    let file = File::open(sys::os(archive_path)).map_err(|e| format!("{shown}: {e}"))?;
    ZipArchive::new(file).map_err(|e| format!("{shown}: {e}"))
}

/// A plain name: not empty, `.` or `..`.
fn safe_component(c: &[u8]) -> bool {
    !c.is_empty() && c != b"." && c != b".." && !c.contains(&0)
}

/// Member name with `/` separators and no trailing slash, if it is safe
/// (inside the archive root, every component a plain name). Unsafe members
/// are neither listed nor extracted.
fn member_name<R: Read>(entry: &zip::read::ZipFile<'_, R>) -> Option<Vec<u8>> {
    let name = entry.enclosed_name()?;
    let name = trim(name.as_os_str().as_bytes()).to_vec();
    name.split(|&b| b == b'/').all(safe_component).then_some(name)
}

/// ZIP timestamps are local time; rows use nanoseconds since the epoch.
fn mtime_ns(dt: Option<zip::DateTime>) -> i64 {
    let Some(dt) = dt else { return 0 };
    // SAFETY: tm is plain old data; mktime reads it as local time.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = dt.year() as i32 - 1900;
    tm.tm_mon = dt.month() as i32 - 1;
    tm.tm_mday = dt.day() as i32;
    tm.tm_hour = dt.hour() as i32;
    tm.tm_min = dt.minute() as i32;
    tm.tm_sec = dt.second() as i32;
    tm.tm_isdst = -1;
    let t = unsafe { libc::mktime(&mut tm) };
    if t < 0 { 0 } else { t as i64 * 1_000_000_000 }
}

/// One archive member, as listed in the central directory.
struct Member {
    name: Vec<u8>,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

type Listing = (Vec<u8>, u64, SystemTime, Arc<Vec<Member>>);

/// Archives listed recently: browsing folders inside one doesn't re-read its
/// directory each time.
static LISTINGS: Mutex<Vec<Listing>> = Mutex::new(Vec::new());
const LISTINGS_KEPT: usize = 4;

fn listing(archive_path: &[u8]) -> Result<Arc<Vec<Member>>, String> {
    let meta = fs::metadata(sys::os(archive_path)).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(archive_path)))?;
    let (len, modified) = (meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let same = |(p, l, m, _): &&Listing| p == archive_path && *l == len && *m == modified;
    if let Some(hit) = LISTINGS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(same) {
        return Ok(hit.3.clone());
    }
    let mut archive = open(archive_path)?;
    let mut members = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(archive_path)))?;
        let Some(name) = member_name(&entry) else { continue };
        let is_dir = entry.is_dir();
        members.push(Member { name, is_dir, size: if is_dir { 0 } else { entry.size() }, mtime: mtime_ns(entry.last_modified()) });
    }
    let members = Arc::new(members);
    let mut cache = LISTINGS.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|(p, ..)| p != archive_path);
    cache.insert(0, (archive_path.to_vec(), len, modified, members.clone()));
    cache.truncate(LISTINGS_KEPT);
    Ok(members)
}

pub fn rows(path: &[u8]) -> Result<Vec<Row>, String> {
    let (archive_path, prefix) = parse_location(path).ok_or_else(|| "Invalid archive location".to_string())?;
    let members = listing(archive_path)?;
    // name -> (is_dir, size, mtime)
    let mut children: HashMap<Vec<u8>, (bool, u64, i64)> = HashMap::new();
    for member in members.iter() {
        let tail = if prefix.is_empty() {
            member.name.as_slice()
        } else {
            let Some(tail) = member.name.strip_prefix(prefix).and_then(|t| t.strip_prefix(b"/")) else { continue };
            tail
        };
        if tail.is_empty() {
            continue;
        }
        let (name, nested) = match memchr::memchr(b'/', tail) {
            Some(i) => (&tail[..i], true),
            None => (tail, false),
        };
        let is_dir = nested || member.is_dir;
        let size = if is_dir { 0 } else { member.size };
        // A folder only known from its children's paths gets no date.
        let mtime = if nested { 0 } else { member.mtime };
        children
            .entry(name.to_vec())
            .and_modify(|child| {
                if is_dir && !child.0 {
                    *child = (true, 0, mtime);
                } else if child.0 && child.2 == 0 {
                    child.2 = mtime;
                }
            })
            .or_insert((is_dir, size, mtime));
    }
    Ok(children
        .into_iter()
        .map(|(name, (is_dir, size, mtime))| Row { name: name.into(), dir: None, size, mtime, flags: if is_dir { DIR } else { 0 }, sel: false, id: 0 })
        .collect())
}

/// Refuse an extraction that cannot fit on the destination volume.
fn check_space(destination: &Path, needed: u64) -> Result<(), String> {
    match sys::space(destination.as_os_str().as_bytes()) {
        Some((free, _)) if needed > free => Err(format!(
            "Not enough space: extracting needs {}, but only {} is free",
            fsys::format_size(needed),
            fsys::format_size(free)
        )),
        _ => Ok(()),
    }
}

/// Write one member to a new file, stopping if it inflates past its declared
/// size. Executable bits recorded in the archive are kept.
fn write_entry(entry: &mut impl Read, declared: u64, mtime: i64, mode: Option<u32>, target: &Path) -> Result<(), String> {
    let mode = mode.map_or(0o644, |m| m & 0o777);
    let mut output = OpenOptions::new().write(true).create_new(true).mode(mode).open(target).map_err(|e| format!("{}: {e}", target.display()))?;
    let written = io::copy(&mut entry.take(declared.saturating_add(1)), &mut output);
    let result = match written {
        Ok(n) if n > declared => Err(format!("{}: archive entry is larger than it declares", target.display())),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{}: {e}", target.display())),
    };
    match result {
        Ok(()) => {
            if mtime > 0 {
                let _ = output.set_modified(SystemTime::UNIX_EPOCH + Duration::from_nanos(mtime as u64));
            }
            Ok(())
        }
        Err(error) => {
            drop(output);
            let _ = fs::remove_file(target);
            Err(error)
        }
    }
}

fn pathbuf(b: &[u8]) -> PathBuf {
    PathBuf::from(sys::os(b))
}

/// Extract the whole archive next to it, into a new folder named after it.
/// Work happens in a hidden staging folder that is renamed into place only
/// when everything succeeded, so a failure never leaves a half-extracted tree.
pub fn extract_archive(path: &[u8]) -> Result<Vec<u8>, String> {
    let parent = fsys::parent(path).unwrap_or_else(|| b".".to_vec());
    let name = fsys::base_name(path);
    let ext = fsys::ext(name);
    let stem = if ext.is_empty() { name } else { &name[..name.len() - ext.len() - 1] };
    let stage = create_stage(&parent, stem)?;
    if let Err(error) = extract_into(path, &stage) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    for _ in 0..16 {
        let destination = fsys::join(&parent, &ops::unique_name(&parent, stem, false));
        match super::fileop::rename(stage.as_os_str().as_bytes(), &destination, false) {
            Ok(()) => return Ok(destination),
            // Someone took the name between choosing and renaming: pick again.
            Err(libc::EEXIST) => continue,
            Err(e) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("Could not finish extraction: {}", sys::error_text(e)));
            }
        }
    }
    let _ = fs::remove_dir_all(&stage);
    Err("Could not choose a name for the extracted folder".into())
}

fn create_stage(parent: &[u8], stem: &[u8]) -> Result<PathBuf, String> {
    for _ in 0..16 {
        let id = STAGE_ID.fetch_add(1, Relaxed);
        let name = [b".", stem, format!(".fex-extract-{}-{id}", std::process::id()).as_bytes()].concat();
        let path = pathbuf(&fsys::join(parent, &name));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not prepare extraction: {e}")),
        }
    }
    Err("Could not reserve a temporary extraction folder".into())
}

fn extract_into(path: &[u8], destination: &Path) -> Result<(), String> {
    let mut archive = open(path)?;
    let shown = String::from_utf8_lossy(path).into_owned();
    let total = (0..archive.len()).filter_map(|i| archive.by_index_raw(i).ok().map(|e| e.size())).fold(0u64, u64::saturating_add);
    check_space(destination, total)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("{shown}: {e}"))?;
        if entry.is_symlink() {
            return Err(format!("Archive contains a symbolic link: {}", entry.name()));
        }
        let Some(relative) = member_name(&entry) else { return Err(format!("Unsafe archive path: {}", entry.name())) };
        let target = destination.join(sys::os(&relative));
        if !target.starts_with(destination) {
            return Err(format!("Unsafe archive path: {}", entry.name()));
        }
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            let parent = target.parent().ok_or_else(|| format!("Invalid archive path: {}", entry.name()))?;
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            let (size, mtime, mode) = (entry.size(), mtime_ns(entry.last_modified()), entry.unix_mode());
            write_entry(&mut entry, size, mtime, mode, &target)?;
        }
    }
    Ok(())
}

/// Extract members (files or folders, as `zip:` locations) into the folder
/// `destination`. A name that is already taken gets a numbered one instead.
/// Returns the top-level paths created and the errors met.
pub fn extract_members(locations: &[Vec<u8>], destination: &[u8]) -> (Vec<Vec<u8>>, Vec<String>) {
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for location in locations {
        match extract_member_to(location, destination) {
            Ok(Some(path)) => created.push(path),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    (created, errors)
}

fn extract_member_to(location: &[u8], destination: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let (archive_path, inner) = parse_location(location).ok_or_else(|| "Invalid archive location".to_string())?;
    let name = fsys::base_name(inner);
    if inner.is_empty() || !safe_component(name) {
        return Err("Choose items inside the archive to extract".into());
    }
    let mut archive = open(archive_path)?;
    let shown = String::from_utf8_lossy(archive_path).into_owned();
    let below = [inner, b"/"].concat();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        if member_name(&entry).is_some_and(|m| m == inner || m.starts_with(&below)) {
            total = total.saturating_add(entry.size());
        }
    }
    check_space(&pathbuf(destination), total)?;

    let top_bytes = fsys::join(destination, &ops::unique_name(destination, name, false));
    let top = pathbuf(&top_bytes);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("{shown}: {e}"))?;
        let Some(member) = member_name(&entry) else { continue };
        let rest: &[u8] = if member == inner {
            b""
        } else if let Some(rest) = member.strip_prefix(below.as_slice()) {
            rest
        } else {
            continue;
        };
        if entry.is_symlink() {
            return Err(format!("Archive contains a symbolic link: {}", entry.name()));
        }
        let target = if rest.is_empty() { top.clone() } else { top.join(sys::os(rest)) };
        if !target.starts_with(&top) {
            return Err(format!("Unsafe archive path: {}", entry.name()));
        }
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            let (size, mtime, mode) = (entry.size(), mtime_ns(entry.last_modified()), entry.unix_mode());
            write_entry(&mut entry, size, mtime, mode, &target)?;
        }
    }
    Ok(top.exists().then_some(top_bytes))
}

/// Extract one member to a private temporary file so it can be opened.
pub fn extract_member(location: &[u8]) -> Result<Vec<u8>, String> {
    let (archive_path, member_path) = parse_location(location).ok_or_else(|| "Invalid archive location".to_string())?;
    if !member_path.split(|&b| b == b'/').all(safe_component) {
        return Err(format!("Unsafe archive path: {}", String::from_utf8_lossy(member_path)));
    }
    let mut archive = open(archive_path)?;
    let index = (0..archive.len())
        .find(|&i| archive.by_index_raw(i).ok().and_then(|e| member_name(&e)).is_some_and(|m| m == member_path))
        .ok_or_else(|| format!("{}: not in the archive", String::from_utf8_lossy(member_path)))?;
    let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
    if entry.is_dir() || entry.is_symlink() {
        return Err("Cannot open this archive entry as a file".into());
    }
    let cache = cache_dir();
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    archive_path.hash(&mut hasher);
    member_path.hash(&mut hasher);
    let key = hasher.finish();
    let (size, mtime, mode) = (entry.size(), mtime_ns(entry.last_modified()), entry.unix_mode());
    let name = fsys::base_name(member_path);
    for _ in 0..32 {
        let id = STAGE_ID.fetch_add(1, Relaxed);
        let target = cache.join(format!("{key:016x}-{id}")).join(sys::os(name));
        if target.parent().is_some_and(|p| fs::create_dir(p).is_err()) {
            continue;
        }
        write_entry(&mut entry, size, mtime, mode, &target)?;
        return Ok(target.as_os_str().as_bytes().to_vec());
    }
    Err("Could not reserve a temporary file for the archive entry".into())
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(sys::os(&sys::home())).join(".cache"))
        .join("fex")
        .join("archive-open")
}

/// Remove members opened more than a day ago.
pub fn clean_cache() {
    let Ok(entries) = fs::read_dir(cache_dir()) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > CACHE_MAX_AGE);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample(root: &Path) -> PathBuf {
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root).unwrap();
        let archive_path = root.join("sample.zip");
        let file = File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().last_modified_time(zip::DateTime::from_date_and_time(2024, 5, 6, 7, 8, 10).unwrap());
        writer.start_file("folder/note.txt", options).unwrap();
        writer.write_all(b"archive data").unwrap();
        writer.start_file("folder/deeper/more.txt", options).unwrap();
        writer.write_all(b"more").unwrap();
        writer.start_file("root.txt", options).unwrap();
        writer.write_all(b"root data").unwrap();
        writer.finish().unwrap();
        archive_path
    }

    #[test]
    fn locations_round_trip() {
        let l = location(b"/data/a|b/files.zip", b"folder/nested");
        assert_eq!(parse_location(&l), Some((&b"/data/a|b/files.zip"[..], &b"folder/nested"[..])));
        assert_eq!(parent_location(&l).as_deref(), Some(&b"zip:/data/a|b/files.zip|folder"[..]));
        let joined = fsys::join(&root_location(b"/d/files.zip"), b"root.txt");
        assert_eq!(parse_location(&joined), Some((&b"/d/files.zip"[..], &b"root.txt"[..])));
        assert_eq!(parent_location(&root_location(b"/d/files.zip")).as_deref(), Some(&b"/d"[..]));
    }

    #[test]
    fn browses_and_extracts_zip_contents() {
        let root = std::env::temp_dir().join(format!("fex-archive-test-{}", std::process::id()));
        let archive_path = sample(&root);
        let path = archive_path.as_os_str().as_bytes();
        let root_uri = root_location(path);
        let rows = rows(&root_uri).unwrap();
        assert!(rows.iter().any(|row| &*row.name == b"folder" && row.is_dir()));
        let file = rows.iter().find(|row| &*row.name == b"root.txt").unwrap();
        assert!(!file.is_dir() && file.mtime > 0);

        let opened = extract_member(&fsys::join(&root_uri, b"root.txt")).unwrap();
        assert_eq!(fs::read(sys::os(&opened)).unwrap(), b"root data");

        let destination = extract_archive(path).unwrap();
        assert_eq!(fs::read(pathbuf(&destination).join("folder/note.txt")).unwrap(), b"archive data");
        let second = extract_archive(path).unwrap();
        assert!(second.ends_with(b"sample (2)"));

        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let picks = vec![fsys::join(&root_uri, b"folder"), fsys::join(&root_uri, b"root.txt")];
        let (created, errors) = extract_members(&picks, out.as_os_str().as_bytes());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(created.len(), 2);
        assert_eq!(fs::read(out.join("folder/deeper/more.txt")).unwrap(), b"more");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_entries_that_inflate_past_their_size() {
        let dir = std::env::temp_dir().join(format!("fex-archive-bomb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.bin");
        let mut data: &[u8] = &[0u8; 64];
        assert!(write_entry(&mut data, 16, 0, None, &target).is_err());
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
