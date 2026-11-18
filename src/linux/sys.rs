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
use std::os::unix::ffi::{OsStrExt, OsStringExt};

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
    /// Inode change time (ns). For a folder it changes whenever an entry is
    /// added, removed or renamed, and on permission changes; unlike mtime it
    /// can't be set back by `touch`, so it's the freshness signal.
    pub ctime: i64,
    pub flags: u8,
    /// `st_mode` (type and permission bits).
    pub mode: u32,
}

/// Every user may list this folder: read and search for owner, group and
/// others alike, and no ACL that could deny someone. (Owner and group bits
/// matter too: the kernel applies the first class that matches, so an owner
/// without `r` is refused even when others have it.)
pub fn world_listable(mode: u32, has_acl: bool) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR && mode & 0o555 == 0o555 && !has_acl
}

/// Whether the open file has a POSIX access ACL. Errors other than "none"
/// or "not supported" count as having one, so callers fail closed.
pub fn fd_has_acl(fd: i32) -> bool {
    let n = unsafe { libc::fgetxattr(fd, c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) };
    n > 0 || (n < 0 && !matches!(errno(), libc::ENODATA | libc::EOPNOTSUPP))
}

/// `fd_has_acl` for a path (a final link is not followed).
pub fn path_has_acl(path: &[u8]) -> bool {
    let Ok(c) = cstr(path) else { return true };
    let n = unsafe { libc::lgetxattr(c.as_ptr(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) };
    n > 0 || (n < 0 && !matches!(errno(), libc::ENODATA | libc::EOPNOTSUPP))
}

/// Whether every user may list the folder at `path` (see `world_listable`).
pub fn path_world_listable(path: &[u8]) -> bool {
    lstat(path).is_some_and(|m| world_listable(m.mode, false) && !path_has_acl(path))
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

    /// Open `path` (absolute) only if no component of it is a symbolic link.
    /// For the root daemon: a folder swapped for a link between listing its
    /// parent and opening it must not get another folder's names indexed
    /// under its path. `openat2(RESOLVE_NO_SYMLINKS)` where the kernel has it
    /// (5.6+); otherwise the opened folder's real path must equal `path`.
    pub fn open_strict(path: &[u8]) -> Result<Dir, i32> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_NO_SYMLINKS: u64 = 0x04;
        static NO_OPENAT2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let c = cstr(path)?;
        let flags = (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NOFOLLOW) as u64;
        if !NO_OPENAT2.load(std::sync::atomic::Ordering::Relaxed) {
            let how = OpenHow { flags, mode: 0, resolve: RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS };
            let fd = unsafe { libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, c.as_ptr(), &how as *const OpenHow, size_of::<OpenHow>()) } as i32;
            if fd >= 0 {
                return Ok(Dir { fd });
            }
            let e = errno();
            if e != libc::ENOSYS {
                return Err(e);
            }
            NO_OPENAT2.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let fd = unsafe { libc::open(c.as_ptr(), flags as i32) };
        if fd < 0 {
            return Err(errno());
        }
        let dir = Dir { fd };
        let real = std::fs::read_link(format!("/proc/self/fd/{fd}")).map_err(|_| libc::EACCES)?;
        let want = match path.strip_suffix(b"/") {
            Some(p) if !p.is_empty() => p,
            _ => path,
        };
        if real.as_os_str().as_bytes() == want { Ok(dir) } else { Err(libc::ELOOP) }
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd
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

    /// Metadata of this folder itself.
    pub fn stat_self(&self) -> Option<Meta> {
        statx(self.fd, b"", libc::AT_EMPTY_PATH)
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
    let mask = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_SIZE | libc::STATX_MTIME | libc::STATX_CTIME;
    let rc = unsafe { libc::statx(dirfd, c.as_ptr(), follow | libc::AT_NO_AUTOMOUNT, mask, &mut st) };
    (rc == 0).then(|| Meta {
        size: st.stx_size,
        mtime: st.stx_mtime.tv_sec * 1_000_000_000 + st.stx_mtime.tv_nsec as i64,
        ctime: st.stx_ctime.tv_sec * 1_000_000_000 + st.stx_ctime.tv_nsec as i64,
        flags: flags_of_mode(st.stx_mode as u32),
        mode: st.stx_mode as u32,
    })
}

/// Metadata of `path` itself (a final link is not followed).
pub fn lstat(path: &[u8]) -> Option<Meta> {
    statx(libc::AT_FDCWD, path, libc::AT_SYMLINK_NOFOLLOW)
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

/// A folder from XDG `user-dirs.dirs` (`DESKTOP`, `TEMPLATES`, ...). These
/// are localized, so the file is the source of truth; `fallback` (relative
/// to home) is used when it doesn't say. XDG points unused folders at $HOME
/// itself, which counts as unset.
pub fn xdg_user_dir(key: &str, fallback: &str) -> Option<Vec<u8>> {
    let home = home();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(|c| c.as_bytes().to_vec())
        .unwrap_or_else(|| [home.as_slice(), b"/.config"].concat());
    let file = std::fs::read(os(&[config.as_slice(), b"/user-dirs.dirs"].concat())).unwrap_or_default();
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
    let path = configured.unwrap_or_else(|| [home.as_slice(), b"/", fallback.as_bytes()].concat());
    (path != home && trim_end_slash(&path) != trim_end_slash(&home)).then_some(path)
}

fn trim_end_slash(p: &[u8]) -> &[u8] {
    p.strip_suffix(b"/").unwrap_or(p)
}

/// The user's standard folders that exist, with English labels.
pub fn quick_access() -> Vec<(String, Vec<u8>)> {
    let mut out = vec![("Home".to_string(), home())];
    for (key, label) in [("DESKTOP", "Desktop"), ("DOCUMENTS", "Documents"), ("DOWNLOAD", "Downloads"), ("MUSIC", "Music"), ("PICTURES", "Pictures"), ("VIDEOS", "Videos")] {
        if let Some(path) = xdg_user_dir(key, label).filter(|p| is_dir(p)) {
            out.push((label.to_string(), path));
        }
    }
    out
}

/// One line of /proc/self/mountinfo that matters here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mount {
    pub point: Vec<u8>,
    pub fstype: String,
    /// What is mounted: a device (`/dev/sda2`), or a share for network
    /// filesystems (`//server/share`, `server:/export`, `user@host:/path`).
    pub source: Vec<u8>,
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
            Some(Mount {
                point: unescape(fields.get(4)?),
                fstype: String::from_utf8_lossy(fields.get(dash + 1)?).into_owned(),
                source: fields.get(dash + 2).map_or_else(Vec::new, |s| unescape(s)),
            })
        })
        .collect()
}

/// Filesystems served over the network. Their calls can stall for as long
/// as the server is unreachable (a hard NFS mount: indefinitely), so FEx
/// only queries them off the UI thread, and their free space only on request.
pub fn is_network_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "ncpfs" | "afs" | "ceph" | "glusterfs" | "lustre" | "gpfs" | "beegfs"
            | "ocfs2" | "gfs2" | "davfs" | "fuse.sshfs" | "fuse.glusterfs" | "fuse.davfs2" | "fuse.rclone" | "fuse.s3fs"
            | "fuse.ceph-fuse" | "fuse.gcsfuse" | "fuse.smbnetfs" | "fuse.curlftpfs" | "9p"
    )
}

/// The server a network mount's source names, for labels: `//nas/media`
/// and `nas:/export` give `nas`, `user@host:/path` gives `host`.
pub fn server_of(source: &[u8]) -> Option<&[u8]> {
    let s = source.strip_prefix(b"//").or_else(|| source.strip_prefix(br"\\"));
    let host = match s {
        Some(rest) => rest.split(|&b| b == b'/' || b == b'\\').next()?,
        // `host:/path`, but not a bare IPv6 address without a path.
        None => {
            let colon = memchr::memchr(b':', source)?;
            &source[..colon]
        }
    };
    let host = match memchr::memrchr(b'@', host) {
        Some(at) => &host[at + 1..],
        None => host,
    };
    (!host.is_empty()).then_some(host)
}

/// Free and total bytes on the filesystem holding `path` (free as an
/// unprivileged user sees it, like Windows' "free bytes available").
pub fn space(path: &[u8]) -> Option<(u64, u64)> {
    let c = cstr(path).ok()?;
    // SAFETY: statvfs is plain old data.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let unit = if st.f_frsize > 0 { st.f_frsize } else { st.f_bsize } as u64;
    Some((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
}

/// Shares mounted through GVFS (the desktop's "Connect to Server", SMB and
/// SFTP bookmarks in GNOME Files). They live as folders under
/// `$XDG_RUNTIME_DIR/gvfs`, named like `smb-share:server=nas,share=media`.
/// Listing that folder asks gvfsd, so call this off the UI thread.
pub fn gvfs_mounts() -> Vec<(String, Vec<u8>)> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|r| r.as_bytes().to_vec())
        .unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }).into_bytes());
    let root = [runtime.as_slice(), b"/gvfs"].concat();
    let Ok(dir) = Dir::open(&root) else { return Vec::new() };
    let mut out = Vec::new();
    let _ = dir.scan(&mut Vec::new(), |e| out.push((gvfs_label(e.name), [root.as_slice(), b"/", e.name].concat())));
    out.sort();
    out
}

/// `smb-share:server=nas,share=media` -> `media on nas`;
/// `sftp:host=box,user=me` -> `box (sftp)`. Values are percent-encoded
/// (twice for nested URLs, as archive mounts have), so they are decoded,
/// and a value that is itself a URL is labelled by its last component.
fn gvfs_label(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    let Some((scheme, args)) = name.split_once(':') else { return name.into_owned() };
    let get = |key: &str| {
        args.split(',').find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('='))).map(|v| {
            let v = percent_decode(&percent_decode(v));
            match v.split_once("://") {
                Some((_, rest)) => rest.trim_end_matches('/').rsplit('/').next().unwrap_or(rest).to_string(),
                None => v,
            }
        })
    };
    let host = get("server").or_else(|| get("host"));
    match (get("share"), host) {
        (Some(share), Some(host)) => format!("{share} on {host}"),
        (None, Some(host)) => format!("{host} ({})", scheme.trim_end_matches("-share")),
        _ => name.into_owned(),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Call `changed` whenever the mount table changes (the kernel flags
/// /proc/self/mountinfo with POLLPRI), and at least every `every`, until
/// it returns false. Blocks the calling thread.
pub fn watch_mounts(every: std::time::Duration, mut changed: impl FnMut() -> bool) {
    let fd = unsafe { libc::open(c"/proc/self/mountinfo".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    loop {
        if fd >= 0 {
            let mut p = libc::pollfd { fd, events: libc::POLLPRI | libc::POLLERR, revents: 0 };
            let rc = unsafe { libc::poll(&mut p, 1, every.as_millis() as i32) };
            if rc > 0 {
                // Re-arm: the event stays raised until the file is read again.
                let mut buf = [0u8; 4096];
                unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
                while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
            }
        } else {
            std::thread::sleep(every);
        }
        if !changed() {
            break;
        }
    }
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// Everything `statx` says about one file, for the Properties window.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stat {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub ino: u64,
    pub size: u64,
    /// Space allocated on disk, in bytes.
    pub on_disk: u64,
    /// Nanoseconds since the epoch; `btime` is 0 where the filesystem
    /// doesn't record creation times.
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub btime: i64,
    /// `STATX_ATTR_*` bits (immutable, append-only, compressed, ...).
    pub attributes: u64,
    pub dev_major: u32,
    pub dev_minor: u32,
}

impl Stat {
    pub fn flags(&self) -> u8 {
        flags_of_mode(self.mode)
    }
}

fn ns(t: libc::statx_timestamp) -> i64 {
    t.tv_sec * 1_000_000_000 + t.tv_nsec as i64
}

fn statx_full(dirfd: i32, name: &[u8], follow: bool) -> Result<Stat, i32> {
    let c = cstr(name)?;
    // SAFETY: statx is plain old data.
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    let mut flags = libc::AT_NO_AUTOMOUNT;
    if !follow {
        flags |= libc::AT_SYMLINK_NOFOLLOW;
    }
    if name.is_empty() {
        flags |= libc::AT_EMPTY_PATH;
    }
    let mask = libc::STATX_BASIC_STATS | libc::STATX_BTIME;
    if unsafe { libc::statx(dirfd, c.as_ptr(), flags, mask, &mut st) } != 0 {
        return Err(errno());
    }
    Ok(Stat {
        mode: st.stx_mode as u32,
        uid: st.stx_uid,
        gid: st.stx_gid,
        nlink: st.stx_nlink,
        ino: st.stx_ino,
        size: st.stx_size,
        on_disk: st.stx_blocks * 512,
        atime: ns(st.stx_atime),
        mtime: ns(st.stx_mtime),
        ctime: ns(st.stx_ctime),
        btime: if st.stx_mask & libc::STATX_BTIME != 0 { ns(st.stx_btime) } else { 0 },
        attributes: st.stx_attributes & st.stx_attributes_mask,
        dev_major: st.stx_dev_major,
        dev_minor: st.stx_dev_minor,
    })
}

/// Full metadata of `path`; `follow` resolves a final symbolic link.
pub fn stat_full(path: &[u8], follow: bool) -> Result<Stat, i32> {
    statx_full(libc::AT_FDCWD, path, follow)
}

impl Dir {
    /// Full metadata of `name` in this folder, without following a link.
    pub fn stat_full(&self, name: &[u8]) -> Option<Stat> {
        statx_full(self.fd, name, false).ok()
    }
}

/// Where a symbolic link points, as stored (possibly relative).
pub fn read_link(path: &[u8]) -> Result<Vec<u8>, i32> {
    std::fs::read_link(os(path)).map(|p| p.into_os_string().into_vec()).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

/// The absolute, link-free path of `path`.
pub fn real_path(path: &[u8]) -> Option<Vec<u8>> {
    std::fs::canonicalize(os(path)).ok().map(|p| p.into_os_string().into_vec())
}

/// Account name for a user id (through NSS, so LDAP/SSSD users resolve too;
/// that can be slow, so call off the UI thread). `None` if it has no name.
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: passwd is plain old data; getpwuid_r writes into `buf`.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned())
}

/// Group name for a group id (see `user_name`).
pub fn group_name(gid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 16384];
    // SAFETY: group is plain old data; getgrgid_r writes into `buf`.
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(gr.gr_name) }.to_string_lossy().into_owned())
}

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// What the current user may do with `path`, as the kernel decides it
/// (mode bits, ACLs, read-only mounts): (read, write, execute).
pub fn access(path: &[u8]) -> (bool, bool, bool) {
    let Ok(c) = cstr(path) else { return (false, false, false) };
    let can = |mode| unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), mode, libc::AT_EACCESS) } == 0;
    (can(libc::R_OK), can(libc::W_OK), can(libc::X_OK))
}

/// Set permission bits (follows a final link: links have no permissions
/// of their own).
pub fn chmod(path: &[u8], mode: u32) -> Result<(), i32> {
    let c = cstr(path)?;
    if unsafe { libc::fchmodat(libc::AT_FDCWD, c.as_ptr(), mode & 0o7777, 0) } == 0 { Ok(()) } else { Err(errno()) }
}

/// One entry of a POSIX access control list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclEntry {
    /// `user`, `group`, `mask`, ... ; named entries carry `id`.
    pub tag: AclTag,
    pub id: Option<u32>,
    /// rwx bits (4, 2, 1).
    pub perm: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclTag {
    UserObj,
    User,
    GroupObj,
    Group,
    Mask,
    Other,
}

/// The extended entries of a POSIX ACL (`access`, or `default` for what new
/// files in a folder inherit). Empty when the file has no ACL beyond its
/// mode bits, or the filesystem doesn't support ACLs.
pub fn acl(path: &[u8], default: bool) -> Vec<AclEntry> {
    let name = if default { c"system.posix_acl_default" } else { c"system.posix_acl_access" };
    let Ok(c) = cstr(path) else { return Vec::new() };
    let mut buf = vec![0u8; 1024];
    loop {
        let n = unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf.truncate(n as usize);
            return parse_acl(&buf);
        }
        if errno() != libc::ERANGE || buf.len() >= 1 << 20 {
            return Vec::new();
        }
        let len = buf.len() * 4;
        buf.resize(len, 0);
    }
}

/// Decode the kernel's xattr form: a u32 version (2), then 8-byte entries
/// of u16 tag, u16 perm, u32 id, all little-endian.
fn parse_acl(b: &[u8]) -> Vec<AclEntry> {
    if b.len() < 4 || u32::from_le_bytes([b[0], b[1], b[2], b[3]]) != 2 {
        return Vec::new();
    }
    b[4..]
        .as_chunks::<8>()
        .0
        .iter()
        .filter_map(|e| {
            let tag = u16::from_le_bytes([e[0], e[1]]);
            let perm = (u16::from_le_bytes([e[2], e[3]]) & 7) as u8;
            let id = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            let (tag, id) = match tag {
                0x01 => (AclTag::UserObj, None),
                0x02 => (AclTag::User, Some(id)),
                0x04 => (AclTag::GroupObj, None),
                0x08 => (AclTag::Group, Some(id)),
                0x10 => (AclTag::Mask, None),
                0x20 => (AclTag::Other, None),
                _ => return None,
            };
            Some(AclEntry { tag, id, perm })
        })
        .collect()
}

/// `-rwxr-x---` style text for a mode, as `ls -l` prints it.
pub fn mode_text(mode: u32) -> String {
    let kind = match mode & libc::S_IFMT {
        libc::S_IFDIR => 'd',
        libc::S_IFLNK => 'l',
        libc::S_IFCHR => 'c',
        libc::S_IFBLK => 'b',
        libc::S_IFIFO => 'p',
        libc::S_IFSOCK => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for (shift, special, set, unset) in [(6, libc::S_ISUID, 's', 'S'), (3, libc::S_ISGID, 's', 'S'), (0, libc::S_ISVTX, 't', 'T')] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => set,
            (false, true) => unset,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Local time with seconds, `YYYY-MM-DD HH:MM:SS`.
pub fn format_time_full(ns: i64) -> String {
    if ns == 0 {
        return String::new();
    }
    let t: libc::time_t = ns.div_euclid(1_000_000_000) as libc::time_t;
    // SAFETY: as in `format_time`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
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
    volumes_from(mounts())
}

fn volumes_from(mounts: Vec<Mount>) -> Vec<Mount> {
    let mut out: Vec<Mount> = mounts
        .into_iter()
        .filter(|m| {
            let local = matches!(
                m.fstype.as_str(),
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "vfat" | "exfat" | "ntfs" | "ntfs3"
                    | "fuseblk" | "virtiofs" | "overlay" | "jfs" | "reiserfs" | "hfsplus" | "udf" | "iso9660" | "bcachefs"
            );
            let p = &m.point;
            (local || is_network_fs(&m.fstype))
                && !p.starts_with(b"/snap/")
                && !p.starts_with(b"/boot")
                && !p.starts_with(b"/proc")
                && !p.starts_with(b"/sys")
                && !p.starts_with(b"/var/lib/docker/")
                && !p.starts_with(b"/var/lib/containers/")
        })
        .collect();
    out.sort_by(|a, b| a.point.cmp(&b.point));
    // A bind or stacked mount shows once, as the topmost (last listed) one.
    out.dedup_by(|later, kept| {
        let same = later.point == kept.point;
        if same {
            std::mem::swap(later, kept);
        }
        same
    });
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
    fn servers_of_network_sources() {
        assert_eq!(server_of(b"//nas/media"), Some(&b"nas"[..]));
        assert_eq!(server_of(b"nas.lan:/export/home"), Some(&b"nas.lan"[..]));
        assert_eq!(server_of(b"me@box:/srv"), Some(&b"box"[..]));
        assert_eq!(server_of(b"/dev/sda1"), None);
    }

    #[test]
    fn labels_gvfs_shares() {
        assert_eq!(gvfs_label(b"smb-share:server=nas,share=media"), "media on nas");
        assert_eq!(gvfs_label(b"sftp:host=box,user=me"), "box (sftp)");
        assert_eq!(gvfs_label(b"odd"), "odd");
        assert_eq!(gvfs_label(b"archive:host=file%253A%252F%252F%252Fhome%252Fme%252Fshare.zip"), "share.zip (archive)");
        assert_eq!(gvfs_label(b"smb-share:server=nas,share=my%20files"), "my files on nas");
    }

    #[test]
    fn decodes_acls() {
        // user::rwx, user:1001:r-x, group::r--, mask::r-x, other::---
        let mut b = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [(1u16, 7u16, u32::MAX), (2, 5, 1001), (4, 4, u32::MAX), (0x10, 5, u32::MAX), (0x20, 0, u32::MAX)] {
            b.extend(tag.to_le_bytes());
            b.extend(perm.to_le_bytes());
            b.extend(id.to_le_bytes());
        }
        let acl = parse_acl(&b);
        assert_eq!(acl.len(), 5);
        assert_eq!(acl[1], AclEntry { tag: AclTag::User, id: Some(1001), perm: 5 });
        assert_eq!(acl[3].tag, AclTag::Mask);
        assert!(parse_acl(&[1, 0, 0, 0]).is_empty());
    }

    #[test]
    fn mode_texts() {
        assert_eq!(mode_text(libc::S_IFREG | 0o644), "-rw-r--r--");
        assert_eq!(mode_text(libc::S_IFDIR | 0o1777), "drwxrwxrwt");
        assert_eq!(mode_text(libc::S_IFREG | 0o4754), "-rwsr-xr--");
        assert_eq!(mode_text(libc::S_IFREG | 0o2640), "-rw-r-S---");
    }

    #[test]
    fn volumes_keep_real_and_network_mounts() {
        let m = |point: &str, fstype: &str| Mount { point: point.into(), fstype: fstype.into(), source: Vec::new() };
        let got = volumes_from(vec![m("/", "ext4"), m("/proc", "proc"), m("/mnt/nas", "cifs"), m("/snap/x", "squashfs"), m("/run/user/1000", "tmpfs")]);
        let points: Vec<&[u8]> = got.iter().map(|v| v.point.as_slice()).collect();
        assert_eq!(points, vec![&b"/"[..], b"/mnt/nas"]);
    }

    #[test]
    fn unescapes_mountinfo() {
        assert_eq!(unescape(br"/media/My\040Disk"), b"/media/My Disk");
        assert_eq!(unescape(br"/plain"), b"/plain");
        assert_eq!(unescape(br"/end\04"), br"/end\04");
    }

    #[test]
    fn strict_open_refuses_links_anywhere_in_the_path() {
        let root = std::env::temp_dir().join(format!("fex-strict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();
        let b = |p: std::path::PathBuf| p.as_os_str().as_bytes().to_vec();
        assert!(Dir::open_strict(&b(root.join("real/inner"))).is_ok());
        assert!(Dir::open_strict(&b(root.join("link"))).is_err(), "final link");
        assert!(Dir::open_strict(&b(root.join("link/inner"))).is_err(), "link in the middle");
        assert!(Dir::open(&b(root.join("link/inner"))).is_ok(), "the plain open still follows links");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn world_listable_needs_every_class() {
        assert!(world_listable(libc::S_IFDIR | 0o755, false));
        assert!(world_listable(libc::S_IFDIR | 0o1777, false));
        assert!(!world_listable(libc::S_IFDIR | 0o711, false), "execute-only");
        assert!(!world_listable(libc::S_IFDIR | 0o075, false), "owner without read is refused");
        assert!(!world_listable(libc::S_IFDIR | 0o755, true), "an ACL may deny someone");
        assert!(!world_listable(libc::S_IFREG | 0o755, false));
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
