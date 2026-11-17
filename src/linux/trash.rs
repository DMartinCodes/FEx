//! The desktop trash (freedesktop.org Trash specification), the Linux side
//! of the Recycle Bin: what FEx deletes can be restored from the file
//! manager's Trash, and FEx's own undo restores it the same way.
//!
//! * Files in the home filesystem go to `$XDG_DATA_HOME/Trash`.
//! * Files on other filesystems go to that volume's `$top/.Trash/$uid` (when
//!   an administrator made a sticky, non-link `.Trash`), else `$top/.Trash-$uid`;
//!   never across filesystems, which would mean copying.
//! * Each item is renamed into `files/` next to an `info/<name>.trashinfo`
//!   recording where it came from (written first, with O_EXCL, so two
//!   deletions never claim one name).

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::fileop;
use super::fsys;
use super::sys;

/// A trash directory and the folder its `Path=` entries are relative to
/// (`None`: absolute paths, the home trash).
struct TrashDir {
    root: Vec<u8>,
    top: Option<Vec<u8>>,
}

fn home_trash() -> Vec<u8> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(|d| std::os::unix::ffi::OsStrExt::as_bytes(d.as_os_str()).to_vec())
        .filter(|d| d.starts_with(b"/"))
        .unwrap_or_else(|| fsys::join(&sys::home(), b".local/share"));
    fsys::join(&data, b"Trash")
}

fn dev_of(path: &[u8]) -> Option<u64> {
    std::fs::symlink_metadata(sys::os(path)).ok().map(|m| m.dev())
}

/// The trash directories that may hold items deleted from `path`'s volume,
/// in the order the specification prefers them.
fn candidates(path: &[u8]) -> Vec<TrashDir> {
    let home = home_trash();
    let home_dev = dev_of(&fsys::parent(&home).unwrap_or_default()).or_else(|| dev_of(&sys::home()));
    let dir_dev = fsys::parent(path).and_then(|p| dev_of(&p));
    if dir_dev.is_some() && dir_dev == home_dev {
        return vec![TrashDir { root: home, top: None }];
    }
    let Some(top) = fileop::mount_of(path).map(|m| m.point) else { return Vec::new() };
    let uid = sys::uid().to_string();
    let mut out = Vec::new();
    let admin = fsys::join(&top, b".Trash");
    let sticky_dir = std::fs::symlink_metadata(sys::os(&admin)).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0);
    if sticky_dir {
        out.push(TrashDir { root: fsys::join(&admin, uid.as_bytes()), top: Some(top.clone()) });
    }
    out.push(TrashDir { root: fsys::join(&top, format!(".Trash-{uid}").as_bytes()), top: Some(top) });
    out
}

/// Create `files/` and `info/` (and the trash itself), private to the user.
fn prepare(t: &TrashDir) -> Result<(), String> {
    for sub in [&b""[..], b"files", b"info"] {
        let p = if sub.is_empty() { t.root.clone() } else { fsys::join(&t.root, sub) };
        match fileop::create_dir_mode(&p, 0o700) {
            Ok(()) | Err(libc::EEXIST) => {}
            Err(e) => return Err(format!("{}: {}", String::from_utf8_lossy(&p), sys::error_text(e))),
        }
    }
    // Refuse a trash that is a link or someone else's (a planted .Trash-uid).
    let m = std::fs::symlink_metadata(sys::os(&t.root)).map_err(|e| e.to_string())?;
    if !m.is_dir() || m.uid() != sys::uid() {
        return Err(format!("{}: not a usable trash folder", String::from_utf8_lossy(&t.root)));
    }
    Ok(())
}

/// Percent-encode a path for `Path=` (RFC 2396 path characters stay).
fn encode(path: &[u8]) -> String {
    let mut s = String::with_capacity(path.len());
    for &b in path {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if s[i] == b'%'
            && i + 2 < s.len()
            && let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

fn now_local() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: tm is plain old data; localtime_r is thread-safe.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Move one item to the trash.
fn trash_one(path: &[u8]) -> Result<(), String> {
    let shown = String::from_utf8_lossy(path).into_owned();
    if fileop::stat(path).is_none() {
        return Err(format!("{shown}: not found"));
    }
    let mut last = format!("{shown}: no trash folder on this volume");
    for t in candidates(path) {
        if let Err(e) = prepare(&t) {
            last = e;
            continue;
        }
        let recorded = match &t.top {
            Some(top) => path.strip_prefix(top.as_slice()).map(|r| r.strip_prefix(b"/").unwrap_or(r).to_vec()).unwrap_or_else(|| path.to_vec()),
            None => path.to_vec(),
        };
        let base = fsys::base_name(path).to_vec();
        for n in 1..1000u32 {
            let name = if n == 1 { base.clone() } else { [base.as_slice(), format!(".{n}").as_bytes()].concat() };
            let info = fsys::join(&fsys::join(&t.root, b"info"), &[name.as_slice(), b".trashinfo"].concat());
            let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(sys::os(&info));
            let mut file = match file {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    last = format!("{shown}: {e}");
                    break;
                }
            };
            let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode(&recorded), now_local());
            if let Err(e) = file.write_all(body.as_bytes()) {
                drop(file);
                let _ = fileop::delete_file(&info);
                last = format!("{shown}: {e}");
                break;
            }
            let dest = fsys::join(&fsys::join(&t.root, b"files"), &name);
            match fileop::rename(path, &dest, false) {
                Ok(()) => return Ok(()),
                Err(libc::EEXIST) => {
                    let _ = fileop::delete_file(&info);
                    continue;
                }
                Err(e) => {
                    let _ = fileop::delete_file(&info);
                    last = format!("{shown}: {}", sys::error_text(e));
                    break;
                }
            }
        }
    }
    Err(last)
}

/// Move items to the trash. Returns the ones that went (undo restores them)
/// and an error for each that couldn't, such as items on a volume with no
/// usable trash (read-only, or a network share that refuses it).
pub fn trash(paths: &[Vec<u8>]) -> (Vec<Vec<u8>>, Vec<String>) {
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        match trash_one(p) {
            Ok(()) => done.push(p.clone()),
            Err(e) => errors.push(e),
        }
    }
    (done, errors)
}

/// Put trashed items back where they came from (the most recent deletion of
/// each path). Fails for an item whose original place is taken again.
pub fn restore(paths: &[Vec<u8>]) -> Result<(), String> {
    let mut errors = Vec::new();
    for path in paths {
        if let Err(e) = restore_one(path) {
            errors.push(e);
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

fn restore_one(path: &[u8]) -> Result<(), String> {
    let shown = String::from_utf8_lossy(path).into_owned();
    if fileop::exists(path) {
        return Err(format!("{shown}: something else is there now"));
    }
    // ((deletion date, name), info file, trashed file). Deletions within one
    // second tie on the date; the later one has the higher-numbered name.
    type Key = (String, usize, Vec<u8>);
    let mut best: Option<(Key, Vec<u8>, Vec<u8>)> = None;
    for t in candidates(path) {
        let info_dir = fsys::join(&t.root, b"info");
        let Ok(dir) = sys::Dir::open(&info_dir) else { continue };
        let mut names = Vec::new();
        let _ = dir.scan(&mut Vec::new(), |e| {
            if e.name.ends_with(b".trashinfo") {
                names.push(e.name.to_vec());
            }
        });
        for n in names {
            let info = fsys::join(&info_dir, &n);
            let Ok(text) = std::fs::read(sys::os(&info)) else { continue };
            let field = |key: &[u8]| text.split(|&b| b == b'\n').find_map(|l| l.strip_prefix(key)).map(|v| v.trim_ascii_end().to_vec());
            let Some(recorded) = field(b"Path=").map(|p| decode(&p)) else { continue };
            let original = match (&t.top, recorded.starts_with(b"/")) {
                (Some(top), false) => fsys::join(top, &recorded),
                _ => recorded,
            };
            if original != path {
                continue;
            }
            let date = String::from_utf8_lossy(&field(b"DeletionDate=").unwrap_or_default()).into_owned();
            let key = (date, n.len(), n.clone());
            if best.as_ref().is_none_or(|(k, ..)| key > *k) {
                let file = fsys::join(&fsys::join(&t.root, b"files"), &n[..n.len() - b".trashinfo".len()]);
                best = Some((key, info, file));
            }
        }
    }
    let Some((_, info, file)) = best else { return Err(format!("{shown}: not found in the trash")) };
    if let Some(parent) = fsys::parent(path) {
        let _ = std::fs::create_dir_all(sys::os(&parent));
    }
    fileop::rename(&file, path, false).map_err(|e| format!("{shown}: {}", sys::error_text(e)))?;
    let _ = fileop::delete_file(&info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn encodes_paths() {
        assert_eq!(encode(b"/home/me/a b%.txt"), "/home/me/a%20b%25.txt");
        assert_eq!(decode(encode(b"/x/caf\xe9 \n").as_bytes()), b"/x/caf\xe9 \n");
    }

    #[test]
    fn trashes_and_restores_in_a_private_home_trash() {
        // A home inside the temp dir, so the user's real trash is untouched.
        let root = std::env::temp_dir().join(format!("fex-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        // SAFETY: tests touching XDG_DATA_HOME run in this one test only.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
        let file = root.join("doomed.txt");
        std::fs::write(&file, b"bye").unwrap();
        let path = file.as_os_str().as_bytes().to_vec();
        let (done, errors) = trash(std::slice::from_ref(&path));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(done, vec![path.clone()]);
        assert!(!file.exists());
        assert!(root.join("data/Trash/files/doomed.txt").exists());
        let info = std::fs::read_to_string(root.join("data/Trash/info/doomed.txt.trashinfo")).unwrap();
        assert!(info.contains("Path=") && info.contains("DeletionDate="));
        // The same name again gets a numbered slot.
        std::fs::write(&file, b"again").unwrap();
        trash(std::slice::from_ref(&path));
        assert!(root.join("data/Trash/files/doomed.txt.2").exists());
        // Restore brings back the most recent one.
        restore(std::slice::from_ref(&path)).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"again");
        assert!(restore(std::slice::from_ref(&path)).is_err(), "the place is taken");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        let _ = std::fs::remove_dir_all(&root);
    }
}
