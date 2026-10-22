//! Read-only ZIP archive browsing and safe extraction.
//!
//! An archive is browsed through a pseudo-location `zip:<archive>|<inner>`.
//! Extraction never trusts the archive: member names must stay inside the
//! destination, symbolic links are refused, nothing is overwritten, and each
//! member may only produce as many bytes as it declares (zip bombs stop there).
//! Extracted files carry the archive's Mark of the Web, as Explorer's do, so
//! SmartScreen and Protected View still warn about downloaded content.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::LocalFileTimeToFileTime;
use windows_sys::Win32::System::Time::SystemTimeToFileTime;
use zip::ZipArchive;

use crate::fsys::{self, Row};
use crate::{ops, win};

static STAGE_ID: AtomicU64 = AtomicU64::new(0);

/// Opened members are extracted here; files older than this are cleaned up.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

pub fn is_location(path: &str) -> bool {
    path.starts_with("zip:")
}

pub fn location(archive: &str, inner: &str) -> String {
    format!("zip:{archive}|{}", inner.trim_matches(['/', '\\']))
}

/// (archive path, member path). The member path has no leading separator,
/// so locations built with `fsys::join` (`zip:a.zip|\name`) parse cleanly.
pub fn parse_location(path: &str) -> Option<(&str, &str)> {
    let (archive, inner) = path.strip_prefix("zip:")?.split_once('|')?;
    Some((archive, inner.trim_matches(['/', '\\'])))
}

pub fn is_archive_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "zip" | "jar" | "cbz" | "epub"))
}

pub fn root_location(path: &str) -> String {
    location(path, "")
}

pub fn parent_location(path: &str) -> Option<String> {
    let (archive, inner) = parse_location(path)?;
    let inner = inner.replace('\\', "/");
    if inner.is_empty() {
        return fsys::parent(archive);
    }
    Some(location(archive, inner.rsplit_once('/').map_or("", |(parent, _)| parent)))
}

fn open(archive_path: &str) -> Result<ZipArchive<File>, String> {
    let file = File::open(archive_path).map_err(|e| format!("{archive_path}: {e}"))?;
    ZipArchive::new(file).map_err(|e| format!("{archive_path}: {e}"))
}

/// Member name with `/` separators and no trailing slash, if it is safe:
/// inside the archive root, and every component a plain file name (no
/// alternate-stream `:` syntax, device names or trailing dots; see
/// `fsys::safe_component`). Unsafe members are neither listed nor extracted.
fn member_name<R: Read>(entry: &zip::read::ZipFile<'_, R>) -> Option<String> {
    let name = entry.enclosed_name()?.to_string_lossy().replace('\\', "/");
    let name = name.trim_end_matches('/');
    name.split('/').all(fsys::safe_component).then(|| name.to_string())
}

/// ZIP timestamps are local time; rows and files use UTC FILETIMEs.
fn filetime(dt: Option<zip::DateTime>) -> u64 {
    let Some(dt) = dt else { return 0 };
    let st = SYSTEMTIME {
        wYear: dt.year(),
        wMonth: dt.month() as u16,
        wDayOfWeek: 0,
        wDay: dt.day() as u16,
        wHour: dt.hour() as u16,
        wMinute: dt.minute() as u16,
        wSecond: dt.second() as u16,
        wMilliseconds: 0,
    };
    let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut utc = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    if unsafe { SystemTimeToFileTime(&st, &mut local) } == 0 || unsafe { LocalFileTimeToFileTime(&local, &mut utc) } == 0 {
        return 0;
    }
    win::ft_u64(utc)
}

/// One archive member, as listed in the central directory.
struct Member {
    /// `/`-separated, no trailing slash.
    name: String,
    is_dir: bool,
    size: u64,
    mtime: u64,
}

type Listing = (String, u64, SystemTime, Arc<Vec<Member>>);

/// Archives listed recently: (path, length, modified, members). Browsing
/// folders inside an archive then doesn't re-read its directory each time.
static LISTINGS: Mutex<Vec<Listing>> = Mutex::new(Vec::new());
const LISTINGS_KEPT: usize = 4;

fn listing(archive_path: &str) -> Result<Arc<Vec<Member>>, String> {
    let meta = fs::metadata(archive_path).map_err(|e| format!("{archive_path}: {e}"))?;
    let (len, modified) = (meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let same = |(p, l, m, _): &&Listing| p.eq_ignore_ascii_case(archive_path) && *l == len && *m == modified;
    if let Some(hit) = LISTINGS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(same) {
        return Ok(hit.3.clone());
    }
    let mut archive = open(archive_path)?;
    let mut members = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| format!("{archive_path}: {e}"))?;
        let Some(name) = member_name(&entry) else { continue };
        let is_dir = entry.is_dir();
        members.push(Member { name, is_dir, size: if is_dir { 0 } else { entry.size() }, mtime: filetime(entry.last_modified()) });
    }
    let members = Arc::new(members);
    let mut cache = LISTINGS.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|(p, ..)| !p.eq_ignore_ascii_case(archive_path));
    cache.insert(0, (archive_path.to_string(), len, modified, members.clone()));
    cache.truncate(LISTINGS_KEPT);
    Ok(members)
}

pub fn rows(path: &str) -> Result<Vec<Row>, String> {
    let (archive_path, inner) = parse_location(path).ok_or_else(|| "Invalid archive location".to_string())?;
    let members = listing(archive_path)?;
    let prefix = inner.replace('\\', "/");
    // Keyed by lowercase name: (name, is_dir, size, mtime)
    let mut children: HashMap<String, (String, bool, u64, u64)> = HashMap::new();

    for member in members.iter() {
        let tail = if prefix.is_empty() {
            member.name.as_str()
        } else {
            let Some(tail) = member.name.strip_prefix(prefix.as_str()).and_then(|t| t.strip_prefix('/')) else { continue };
            tail
        };
        if tail.is_empty() {
            continue;
        }
        let (name, nested) = tail.split_once('/').map_or((tail, false), |(name, _)| (name, true));
        if name.is_empty() {
            continue;
        }
        let is_dir = nested || member.is_dir;
        let size = if is_dir { 0 } else { member.size };
        // A folder only known from its children's paths gets no date.
        let mtime = if nested { 0 } else { member.mtime };
        children
            .entry(name.to_lowercase())
            .and_modify(|child| {
                if is_dir && !child.1 {
                    *child = (name.to_string(), true, 0, mtime);
                } else if child.1 && child.3 == 0 {
                    child.3 = mtime;
                }
            })
            .or_insert_with(|| (name.to_string(), is_dir, size, mtime));
    }

    Ok(children
        .into_values()
        .map(|(name, is_dir, size, mtime)| Row {
            name: name.into(),
            dir: None,
            size,
            mtime,
            attrs: if is_dir { win::ATTR_DIR } else { 0 },
            sel: false,
        })
        .collect())
}

/// Refuse an extraction that cannot fit on the destination volume.
fn check_space(destination: &Path, needed: u64) -> Result<(), String> {
    match win::drive_space(&destination.to_string_lossy()) {
        Some((free, _)) if needed > free => Err(format!(
            "Not enough space: extracting needs {}, but only {} is free",
            fsys::format_size(needed),
            fsys::format_size(free)
        )),
        _ => Ok(()),
    }
}

/// Write one member to a new file, stopping if it inflates past its declared
/// size. `zone` is the archive's Mark of the Web, passed on to the file.
fn write_entry(entry: &mut impl Read, declared: u64, mtime: u64, zone: Option<&[u8]>, target: &Path) -> Result<(), String> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| format!("{}: {e}", target.display()))?;
    let written = io::copy(&mut entry.take(declared.saturating_add(1)), &mut output);
    let result = match written {
        Ok(n) if n > declared => Err(format!("{}: archive entry is larger than it declares", target.display())),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{}: {e}", target.display())),
    };
    match result {
        Ok(()) => {
            // Before the date: writing the stream would bump it.
            if let Some(zone) = zone {
                win::set_zone(target, zone);
            }
            if let Some(time) = win::system_time(mtime) {
                let _ = output.set_modified(time);
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

/// Extract the whole archive next to it, into a new folder named after it.
/// Work happens in a hidden staging folder that is renamed into place only
/// when everything succeeded, so a failure never leaves a half-extracted tree.
pub fn extract_archive(path: &str) -> Result<String, String> {
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    let stem = Path::new(path).file_stem().and_then(|name| name.to_str()).unwrap_or("Archive");
    let stage = create_stage(parent, stem)?;
    let result = extract_into(path, &stage);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let dir = parent.to_string_lossy();
    for _ in 0..16 {
        let destination = parent.join(ops::unique_name(&dir, stem, false));
        match fs::rename(&stage, &destination) {
            Ok(()) => return Ok(destination.to_string_lossy().into_owned()),
            // Someone took the name between choosing and renaming: pick again.
            Err(_) if destination.exists() => continue,
            Err(e) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("Could not finish extraction: {e}"));
            }
        }
    }
    let _ = fs::remove_dir_all(&stage);
    Err("Could not choose a name for the extracted folder".into())
}

fn create_stage(parent: &Path, stem: &str) -> Result<PathBuf, String> {
    for _ in 0..16 {
        let id = STAGE_ID.fetch_add(1, Relaxed);
        let path = parent.join(format!(".{stem}.fex-extract-{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not prepare extraction: {e}")),
        }
    }
    Err("Could not reserve a temporary extraction folder".into())
}

fn extract_into(path: &str, destination: &Path) -> Result<(), String> {
    let mut archive = open(path)?;
    let zone = win::zone_of(path);
    let total = (0..archive.len()).filter_map(|i| archive.by_index_raw(i).ok().map(|e| e.size())).fold(0u64, u64::saturating_add);
    check_space(destination, total)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("{path}: {e}"))?;
        if entry.is_symlink() {
            return Err(format!("Archive contains a symbolic link: {}", entry.name()));
        }
        if member_name(&entry).is_none() {
            return Err(format!("Unsafe archive path: {}", entry.name()));
        }
        let Some(relative) = entry.enclosed_name() else { continue };
        let target = destination.join(relative);
        if !target.starts_with(destination) {
            return Err(format!("Unsafe archive path: {}", entry.name()));
        }
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            let parent = target.parent().ok_or_else(|| format!("Invalid archive path: {}", entry.name()))?;
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            let (size, mtime) = (entry.size(), filetime(entry.last_modified()));
            write_entry(&mut entry, size, mtime, zone.as_deref(), &target)?;
        }
    }
    Ok(())
}

/// Extract members (files or folders, as `zip:` locations) into the folder
/// `destination`. A name that is already taken gets a numbered one instead.
/// Returns the top-level paths created and the errors met.
pub fn extract_members(locations: &[String], destination: &str) -> (Vec<String>, Vec<String>) {
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

fn extract_member_to(location: &str, destination: &str) -> Result<Option<String>, String> {
    let (archive_path, inner) = parse_location(location).ok_or_else(|| "Invalid archive location".to_string())?;
    let inner = inner.replace('\\', "/");
    let Some(name) = inner.rsplit('/').next().filter(|n| fsys::safe_component(n)) else {
        return Err("Choose items inside the archive to extract".into());
    };
    let mut archive = open(archive_path)?;
    let zone = win::zone_of(archive_path);
    let mut total = 0u64;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        if member_name(&entry).is_some_and(|m| m == inner || m.starts_with(&format!("{inner}/"))) {
            total = total.saturating_add(entry.size());
        }
    }
    check_space(Path::new(destination), total)?;

    let top = Path::new(destination).join(ops::unique_name(destination, name, false));
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("{archive_path}: {e}"))?;
        let Some(member) = member_name(&entry) else { continue };
        let rest = if member == inner {
            ""
        } else if let Some(rest) = member.strip_prefix(&format!("{inner}/")) {
            rest
        } else {
            continue;
        };
        if entry.is_symlink() {
            return Err(format!("Archive contains a symbolic link: {}", entry.name()));
        }
        let target = if rest.is_empty() { top.clone() } else { top.join(rest) };
        if !target.starts_with(&top) {
            return Err(format!("Unsafe archive path: {}", entry.name()));
        }
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            let (size, mtime) = (entry.size(), filetime(entry.last_modified()));
            write_entry(&mut entry, size, mtime, zone.as_deref(), &target)?;
        }
    }
    Ok(top.exists().then(|| top.to_string_lossy().into_owned()))
}

/// Extract one member to a private temporary file so it can be opened.
pub fn extract_member(location: &str) -> Result<PathBuf, String> {
    let (archive_path, member_path) = parse_location(location).ok_or_else(|| "Invalid archive location".to_string())?;
    let member_path = member_path.replace('\\', "/");
    if !member_path.split('/').all(fsys::safe_component) {
        return Err(format!("Unsafe archive path: {member_path}"));
    }
    let zone = win::zone_of(archive_path);
    let mut archive = open(archive_path)?;
    let mut entry = archive.by_name(&member_path).map_err(|e| format!("{member_path}: {e}"))?;
    if entry.is_dir() || entry.is_symlink() {
        return Err("Cannot open this archive entry as a file".into());
    }
    let name = entry
        .enclosed_name()
        .and_then(|path| path.file_name().map(|name| name.to_os_string()))
        .ok_or_else(|| format!("Unsafe archive path: {member_path}"))?;
    let cache = cache_dir();
    fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    archive_path.hash(&mut hasher);
    member_path.hash(&mut hasher);
    let key = hasher.finish();
    let (size, mtime) = (entry.size(), filetime(entry.last_modified()));
    for _ in 0..32 {
        let id = STAGE_ID.fetch_add(1, Relaxed);
        let target = cache.join(format!("{key:016x}-{id}-{}", name.to_string_lossy()));
        if target.exists() {
            continue;
        }
        write_entry(&mut entry, size, mtime, zone.as_deref(), &target)?;
        return Ok(target);
    }
    Err("Could not reserve a temporary file for the archive entry".into())
}

fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("FEx").join("archive-open")
}

/// Remove members opened more than a day ago. Files still in use are skipped.
pub fn clean_cache() {
    let Ok(entries) = fs::read_dir(cache_dir()) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.created())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > CACHE_MAX_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
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
        let options = zip::write::SimpleFileOptions::default()
            .last_modified_time(zip::DateTime::from_date_and_time(2024, 5, 6, 7, 8, 10).unwrap());
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
    fn archive_location_round_trips_windows_paths() {
        let location = location(r"C:\data\files.zip", r"folder\nested");
        assert_eq!(parse_location(&location), Some((r"C:\data\files.zip", r"folder\nested")));
        assert_eq!(parent_location(&location).as_deref(), Some(r"zip:C:\data\files.zip|folder"));
        assert!(is_archive_file(r"C:\data\files.zip"));
        assert!(!is_archive_file(r"C:\data\files.txt"));
        // Rows are joined onto the location like any folder.
        let joined = fsys::join(&root_location(r"C:\data\files.zip"), "root.txt");
        assert_eq!(parse_location(&joined), Some((r"C:\data\files.zip", "root.txt")));
    }

    #[test]
    fn browses_and_extracts_zip_contents() {
        let root = std::env::temp_dir().join(format!("fex-archive-test-{}", std::process::id()));
        let archive_path = sample(&root);
        let path = archive_path.to_string_lossy();
        let root_uri = root_location(&path);
        let rows = rows(&root_uri).unwrap();
        assert!(rows.iter().any(|row| row.name.as_ref() == "folder" && row.is_dir()));
        let file = rows.iter().find(|row| row.name.as_ref() == "root.txt").unwrap();
        assert!(!file.is_dir() && file.mtime > 0 && file.dir.is_none());

        // A root-level member opens (its location carries a leading separator).
        let opened = extract_member(&fsys::join(&root_uri, "root.txt")).unwrap();
        assert_eq!(fs::read(&opened).unwrap(), b"root data");
        let _ = fs::remove_file(opened);

        let destination = extract_archive(&path).unwrap();
        assert_eq!(fs::read(Path::new(&destination).join("folder/note.txt")).unwrap(), b"archive data");
        // A second extraction picks a new folder instead of failing.
        let second = extract_archive(&path).unwrap();
        assert!(second.ends_with("sample (2)"), "{second}");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(destination);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn extracts_selected_members() {
        let root = std::env::temp_dir().join(format!("fex-archive-members-{}", std::process::id()));
        let archive_path = sample(&root);
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let base = root_location(&archive_path.to_string_lossy());
        let picks = vec![fsys::join(&base, "folder"), fsys::join(&base, "root.txt")];
        let out_s = out.to_string_lossy().into_owned();
        let (created, errors) = extract_members(&picks, &out_s);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(created.len(), 2);
        assert_eq!(fs::read(out.join("folder/deeper/more.txt")).unwrap(), b"more");
        let (again, _) = extract_members(&picks[1..], &out_s);
        assert!(again[0].ends_with("root (2).txt"), "{again:?}");
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
        assert!(!target.exists(), "a refused entry leaves nothing behind");
        let _ = fs::remove_dir_all(&dir);
    }
}
