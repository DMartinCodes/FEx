//! Thin wrappers over the Linux calls the prototype needs, mirroring
//! `win.rs`: directory scans hand out borrowed names and let the caller
//! decide what (if anything) to keep.
//!
//! Linux names are bytes (anything but `/` and NUL), not text, so names and
//! paths stay `[u8]` end to end and are only decoded for display. A name that
//! isn't valid UTF-8 can still be opened, because nothing is lost on the way.
//!
//! The big difference from Windows: `getdents64` returns only a name and a
//! type, while `FindFirstFileExW` also returns size and times. Metadata costs
//! one `statx` per entry, made relative to the open directory (no path
//! resolution), and only where it is actually needed.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

/// Entry flags (the Linux counterpart of the Win32 attribute bits FEx uses).
pub const DIR: u8 = 1;
/// A symbolic link; with `DIR` set too, a link to a folder (like a junction).
pub const LINK: u8 = 2;
/// Not a regular file, folder or link: device, socket, FIFO.
pub const SPECIAL: u8 = 4;

/// One directory entry, borrowed from the kernel's getdents buffer.
pub struct RawEntry<'a> {
    pub name: &'a [u8],
    pub flags: u8,
}

#[derive(Clone, Copy, Default)]
pub struct Meta {
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime: i64,
    pub flags: u8,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// NUL-terminated copy of a path or name. Fails (EINVAL) on an embedded NUL,
/// which no real path contains.
pub fn cstr(path: &[u8]) -> Result<CString, i32> {
    CString::new(path).map_err(|_| libc::EINVAL)
}

pub fn os(path: &[u8]) -> &std::ffi::OsStr {
    std::ffi::OsStr::from_bytes(path)
}

fn flags_of_mode(mode: u32) -> u8 {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => DIR,
        libc::S_IFLNK => LINK,
        libc::S_IFREG => 0,
        _ => SPECIAL,
    }
}

/// An open directory, closed on drop. Lookups relative to it skip resolving
/// the path again, which matters when statting every entry.
pub struct Dir {
    fd: i32,
}

impl Drop for Dir {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Bytes per getdents64 call. Larger buffers mean fewer syscalls on big
/// folders (the analogue of FIND_FIRST_EX_LARGE_FETCH).
pub const SCAN_BUF: usize = 64 * 1024;

impl Dir {
    pub fn open(path: &[u8]) -> Result<Dir, i32> {
        let c = cstr(path)?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY) };
        if fd < 0 { Err(errno()) } else { Ok(Dir { fd }) }
    }

    /// Call `f` for every entry except `.` and `..`. `buf` is the caller's
    /// reusable getdents buffer (kept per worker thread by the walker).
    /// Filesystems that don't report entry types get one `statx` for those.
    pub fn scan(&self, buf: &mut Vec<u64>, mut f: impl FnMut(RawEntry<'_>)) -> Result<(), i32> {
        if buf.len() < SCAN_BUF / 8 {
            buf.resize(SCAN_BUF / 8, 0);
        }
        loop {
            // SAFETY: the kernel writes at most `len` bytes into `buf`.
            let n = unsafe { libc::syscall(libc::SYS_getdents64, self.fd, buf.as_mut_ptr(), buf.len() * 8) };
            if n < 0 {
                return Err(errno());
            }
            if n == 0 {
                return Ok(());
            }
            // SAFETY: plain bytes, `n` of them filled in by the kernel.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
            let mut off = 0;
            // struct linux_dirent64 { u64 d_ino; i64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
            while off + 19 <= bytes.len() {
                let reclen = u16::from_ne_bytes([bytes[off + 16], bytes[off + 17]]) as usize;
                if reclen == 0 || off + reclen > bytes.len() {
                    break;
                }
                let d_type = bytes[off + 18];
                let raw = &bytes[off + 19..off + reclen];
                let name = &raw[..memchr::memchr(0, raw).unwrap_or(raw.len())];
                off += reclen;
                if name == b"." || name == b".." {
                    continue;
                }
                let flags = match d_type {
                    libc::DT_DIR => DIR,
                    libc::DT_REG => 0,
                    libc::DT_LNK => LINK,
                    libc::DT_UNKNOWN => self.stat(name).map_or(0, |m| m.flags),
                    _ => SPECIAL,
                };
                f(RawEntry { name, flags });
            }
        }
    }

    /// Metadata of `name` inside this folder, without following a link.
    pub fn stat(&self, name: &[u8]) -> Option<Meta> {
        statx(self.fd, name, libc::AT_SYMLINK_NOFOLLOW)
    }

    /// Whether `name` (a link) points at a folder.
    pub fn link_is_dir(&self, name: &[u8]) -> bool {
        statx(self.fd, name, 0).is_some_and(|m| m.flags & DIR != 0)
    }
}

fn statx(dirfd: i32, name: &[u8], follow: i32) -> Option<Meta> {
    let c = cstr(name).ok()?;
    // SAFETY: statx is plain old data.
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    let mask = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_SIZE | libc::STATX_MTIME;
    let rc = unsafe { libc::statx(dirfd, c.as_ptr(), follow | libc::AT_NO_AUTOMOUNT, mask, &mut st) };
    (rc == 0).then(|| Meta {
        size: st.stx_size,
        mtime: st.stx_mtime.tv_sec * 1_000_000_000 + st.stx_mtime.tv_nsec as i64,
        flags: flags_of_mode(st.stx_mode as u32),
    })
}

/// Whether `path` exists and is (or links to) a folder.
pub fn is_dir(path: &[u8]) -> bool {
    statx(libc::AT_FDCWD, path, 0).is_some_and(|m| m.flags & DIR != 0)
}

pub fn error_text(code: i32) -> String {
    std::io::Error::from_raw_os_error(code).to_string()
}

/// Local time `YYYY-MM-DD HH:MM` for nanoseconds since the epoch.
pub fn format_time(ns: i64) -> String {
    if ns == 0 {
        return String::new();
    }
    let t: libc::time_t = ns.div_euclid(1_000_000_000) as libc::time_t;
    // SAFETY: tm is plain old data; localtime_r is the thread-safe variant.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub fn home() -> Vec<u8> {
    std::env::var_os("HOME").map(|h| h.as_bytes().to_vec()).filter(|h| !h.is_empty()).unwrap_or_else(|| b"/".to_vec())
}

/// The user's standard folders, as XDG `user-dirs.dirs` names them (they are
/// localized, so the file is the source of truth), with English fallbacks.
pub fn quick_access() -> Vec<(String, Vec<u8>)> {
    let home = home();
    let mut out = vec![("Home".to_string(), home.clone())];
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(|c| c.as_bytes().to_vec())
        .unwrap_or_else(|| [home.as_slice(), b"/.config"].concat());
    let file = std::fs::read(os(&[config.as_slice(), b"/user-dirs.dirs"].concat())).unwrap_or_default();
    for (key, label) in [("DESKTOP", "Desktop"), ("DOCUMENTS", "Documents"), ("DOWNLOAD", "Downloads"), ("PICTURES", "Pictures")] {
        let prefix = format!("XDG_{key}_DIR=");
        let configured = file
            .split(|&b| b == b'\n')
            .find_map(|line| line.strip_prefix(prefix.as_bytes()))
            .map(|v| {
                let v = v.trim_ascii();
                let v = v.strip_prefix(b"\"").unwrap_or(v);
                v.strip_suffix(b"\"").unwrap_or(v).to_vec()
            })
            .map(|v| match v.strip_prefix(b"$HOME") {
                Some(rest) => [home.as_slice(), rest].concat(),
                None => v,
            });
        let path = configured.unwrap_or_else(|| [home.as_slice(), b"/", label.as_bytes()].concat());
        // XDG points unused folders at $HOME itself.
        if path != home && is_dir(&path) {
            out.push((label.to_string(), path));
        }
    }
    out
}

/// One line of /proc/self/mountinfo that matters here.
pub struct Mount {
    pub point: Vec<u8>,
    pub fstype: String,
}

/// Undo mountinfo's octal escapes (`\040` for a space, ...).
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' && i + 3 < field.len() && field[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((field[i + 1] - b'0') * 64 + (field[i + 2] - b'0') * 8 + (field[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    out
}

pub fn mounts() -> Vec<Mount> {
    let text = std::fs::read("/proc/self/mountinfo").unwrap_or_default();
    text.split(|&b| b == b'\n')
        .filter_map(|line| {
            // id parent major:minor root mountpoint options [optional...] - fstype source superoptions
            let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            let dash = fields.iter().position(|f| *f == b"-")?;
            Some(Mount { point: unescape(fields.get(4)?), fstype: String::from_utf8_lossy(fields.get(dash + 1)?).into_owned() })
        })
        .collect()
}

/// Kernel and runtime filesystems with no user files. A crawl never enters
/// them: /proc and /sys alone would add millions of meaningless entries.
pub fn is_pseudo_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "cgroup" | "cgroup2" | "debugfs" | "tracefs" | "securityfs"
            | "pstore" | "bpf" | "configfs" | "fusectl" | "mqueue" | "hugetlbfs" | "autofs" | "binfmt_misc"
            | "efivarfs" | "rpc_pipefs" | "nsfs" | "ramfs" | "selinuxfs" | "fuse.gvfsd-fuse" | "fuse.portal"
    )
}

/// Mount points a crawl skips (see `is_pseudo_fs`).
pub fn crawl_skips() -> std::collections::HashSet<Vec<u8>> {
    mounts().into_iter().filter(|m| is_pseudo_fs(&m.fstype)).map(|m| m.point).collect()
}

/// Mounted volumes worth showing in the sidebar: real filesystems and
/// network shares, not snaps, runtime tmpfs or boot partitions.
pub fn volumes() -> Vec<Mount> {
    let mut out: Vec<Mount> = mounts()
        .into_iter()
        .filter(|m| {
            let real = matches!(
                m.fstype.as_str(),
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "vfat" | "exfat" | "ntfs" | "ntfs3"
                    | "fuseblk" | "nfs" | "nfs4" | "cifs" | "smb3" | "fuse.sshfs" | "9p" | "virtiofs" | "overlay"
            );
            let p = &m.point;
            real && !p.starts_with(b"/snap/") && !p.starts_with(b"/boot") && !p.starts_with(b"/proc") && !p.starts_with(b"/sys")
        })
        .collect();
    out.sort_by(|a, b| a.point.cmp(&b.point));
    out.dedup_by(|a, b| a.point == b.point);
    out
}

/// Open with the desktop's default handler. Returns at once; the child is
/// reaped on a thread so it never lingers as a zombie.
pub fn open_with_default(path: &[u8]) -> Result<(), String> {
    let mut child = std::process::Command::new("xdg-open")
        .arg(os(path))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run xdg-open: {e}"))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mountinfo() {
        assert_eq!(unescape(br"/media/My\040Disk"), b"/media/My Disk");
        assert_eq!(unescape(br"/plain"), b"/plain");
        assert_eq!(unescape(br"/end\04"), br"/end\04");
    }

    #[test]
    fn scans_names_that_are_not_text() {
        let root = std::env::temp_dir().join(format!("fex-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let odd: &[u8] = b"caf\xe9\nline.txt"; // Latin-1 é and a newline: legal on Linux
        std::fs::write(root.join(os(odd)), b"12345").unwrap();
        std::os::unix::fs::symlink("sub", root.join("link")).unwrap();
        let dir = Dir::open(root.as_os_str().as_bytes()).unwrap();
        let mut seen = Vec::new();
        dir.scan(&mut Vec::new(), |e| seen.push((e.name.to_vec(), e.flags))).unwrap();
        seen.sort();
        assert_eq!(seen, vec![(odd.to_vec(), 0), (b"link".to_vec(), LINK), (b"sub".to_vec(), DIR)]);
        assert_eq!(dir.stat(odd).unwrap().size, 5);
        assert!(dir.link_is_dir(b"link"));
        let _ = std::fs::remove_dir_all(root);
    }
}
