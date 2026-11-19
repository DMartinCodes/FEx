//! Primitives for changing the filesystem (the Linux side of `fileop.rs`):
//! copy, move, delete, create, links, and the storage queries the transfer
//! engine uses to pick a thread count. Paths are bytes; errors are errno.
//!
//! Copies try a reflink first (`FICLONE`: instant and space-sharing on Btrfs,
//! XFS, bcachefs), then `copy_file_range` (in-kernel, and server-side on NFS
//! 4.2 and SMB3), then plain read/write. Mode bits and timestamps follow the
//! source, as `cp -p` would keep them (owner only when running as root).

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::sys::{self, cstr, DIR, LINK};

/// Copies are made in pieces this large, so progress and cancellation are
/// checked often even on fast disks.
const CHUNK: usize = 8 << 20;
/// Files at least this large are dropped from the page cache once copied,
/// so one big copy doesn't evict everything else (the analogue of Windows'
/// unbuffered copies).
const DONTNEED_MIN: u64 = 256 << 20;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// What the engine needs to know about an existing path (a final link is not
/// followed).
#[derive(Clone, Copy, Debug)]
pub struct St {
    pub size: u64,
    /// Nanoseconds since the epoch.
    pub mtime: i64,
    pub flags: u8,
    pub mode: u32,
}

impl St {
    pub fn is_dir(&self) -> bool {
        self.flags & (DIR | LINK) == DIR
    }
}

pub fn stat(path: &[u8]) -> Option<St> {
    let s = sys::stat_full(path, false).ok()?;
    Some(St { size: s.size, mtime: s.mtime, flags: s.flags(), mode: s.mode })
}

pub fn exists(path: &[u8]) -> bool {
    stat(path).is_some()
}

/// Rename `from` to `to`. Without `replace`, an existing `to` is an error
/// (EEXIST), atomically where the filesystem supports `RENAME_NOREPLACE`.
pub fn rename(from: &[u8], to: &[u8], replace: bool) -> Result<(), i32> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    if replace {
        return if unsafe { libc::rename(f.as_ptr(), t.as_ptr()) } == 0 { Ok(()) } else { Err(errno()) };
    }
    let rc = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    if rc == 0 {
        return Ok(());
    }
    match errno() {
        // Not supported here (some network and FUSE filesystems): check, then rename.
        libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP => {
            // A case-only rename on a case-insensitive filesystem "exists" as itself.
            if exists(to) && sys::stat_full(from, false).ok().map(|s| s.ino) != sys::stat_full(to, false).ok().map(|s| s.ino) {
                return Err(libc::EEXIST);
            }
            if unsafe { libc::rename(f.as_ptr(), t.as_ptr()) } == 0 { Ok(()) } else { Err(errno()) }
        }
        e => Err(e),
    }
}

/// Create a folder with `mode` (less the umask).
pub fn create_dir_mode(path: &[u8], mode: u32) -> Result<(), i32> {
    let c = cstr(path)?;
    if unsafe { libc::mkdir(c.as_ptr(), mode & 0o7777) } == 0 { Ok(()) } else { Err(errno()) }
}

pub fn create_dir(path: &[u8]) -> Result<(), i32> {
    create_dir_mode(path, 0o777)
}

pub fn remove_dir(path: &[u8]) -> Result<(), i32> {
    let c = cstr(path)?;
    if unsafe { libc::rmdir(c.as_ptr()) } == 0 { Ok(()) } else { Err(errno()) }
}

pub fn delete_file(path: &[u8]) -> Result<(), i32> {
    let c = cstr(path)?;
    if unsafe { libc::unlink(c.as_ptr()) } == 0 { Ok(()) } else { Err(errno()) }
}

/// Delete a file, link or whole folder tree for good. Links are removed,
/// never followed.
pub fn delete_permanently(path: &[u8]) -> Result<(), String> {
    let st = stat(path).ok_or_else(|| format!("{}: not found", lossy(path)))?;
    let r = if st.is_dir() { std::fs::remove_dir_all(sys::os(path)) } else { std::fs::remove_file(sys::os(path)) };
    r.map_err(|e| format!("{}: {e}", lossy(path)))
}

pub fn symlink(target: &[u8], link: &[u8]) -> Result<(), i32> {
    let (t, l) = (cstr(target)?, cstr(link)?);
    if unsafe { libc::symlink(t.as_ptr(), l.as_ptr()) } == 0 { Ok(()) } else { Err(errno()) }
}

fn ts(ns: i64) -> libc::timespec {
    libc::timespec { tv_sec: ns.div_euclid(1_000_000_000) as _, tv_nsec: ns.rem_euclid(1_000_000_000) as _ }
}

/// An open file descriptor, closed on drop.
struct Fd(i32);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Copy one file (or recreate one symbolic link) to `dst`. Without
/// `overwrite`, an existing `dst` is an error. `progress` gets the bytes
/// copied so far. On failure or cancellation a partly written `dst` is
/// removed.
pub fn copy_file(src: &[u8], dst: &[u8], overwrite: bool, cancel: &AtomicBool, progress: &mut dyn FnMut(u64)) -> Result<(), i32> {
    let st = sys::stat_full(src, false)?;
    match st.mode & libc::S_IFMT {
        libc::S_IFLNK => {
            let target = sys::read_link(src)?;
            if overwrite && stat(dst).is_some_and(|d| !d.is_dir()) {
                delete_file(dst)?;
            }
            symlink(&target, dst)?;
            // The link's own times, so a later "update" sees it unchanged.
            if let Ok(d) = cstr(dst) {
                let times = [ts(st.atime), ts(st.mtime)];
                unsafe { libc::utimensat(libc::AT_FDCWD, d.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
            }
            return Ok(());
        }
        libc::S_IFREG => {}
        _ => return Err(libc::EOPNOTSUPP), // devices, sockets, FIFOs
    }
    let (s, d) = (cstr(src)?, cstr(dst)?);
    let input = Fd(unsafe { libc::open(s.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) });
    if input.0 < 0 {
        return Err(errno());
    }
    let excl = if overwrite { libc::O_TRUNC } else { libc::O_EXCL };
    // Owner-writable while we write; the source's exact mode is set at the end.
    let output = Fd(unsafe { libc::open(d.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC | excl, (st.mode & 0o777) | 0o200) });
    if output.0 < 0 {
        return Err(errno());
    }
    let result = copy_data(&input, &output, st.size, cancel, progress).map(|()| {
        // Mode, then times (writing would bump mtime again), then owner if we can.
        unsafe { libc::fchmod(output.0, st.mode & 0o7777) };
        let times = [ts(st.atime), ts(st.mtime)];
        unsafe { libc::futimens(output.0, times.as_ptr()) };
        if sys::uid() == 0 {
            unsafe { libc::fchown(output.0, st.uid, st.gid) };
        }
        if st.size >= DONTNEED_MIN {
            unsafe { libc::posix_fadvise(output.0, 0, 0, libc::POSIX_FADV_DONTNEED) };
            unsafe { libc::posix_fadvise(input.0, 0, 0, libc::POSIX_FADV_DONTNEED) };
        }
    });
    if result.is_err() {
        drop(output);
        let _ = delete_file(dst);
    }
    result
}

fn copy_data(input: &Fd, output: &Fd, size: u64, cancel: &AtomicBool, progress: &mut dyn FnMut(u64)) -> Result<(), i32> {
    // A reflink shares the source's blocks: no data moves at all.
    if size > 0 && unsafe { libc::ioctl(output.0, libc::FICLONE as _, input.0) } == 0 {
        progress(size);
        return Ok(());
    }
    let mut done = 0u64;
    let mut kernel_copy = true;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if cancel.load(Relaxed) {
            return Err(libc::ECANCELED);
        }
        let n = if kernel_copy {
            let n = unsafe { libc::copy_file_range(input.0, std::ptr::null_mut(), output.0, std::ptr::null_mut(), CHUNK, 0) };
            if n < 0 {
                match errno() {
                    // Across filesystems on older kernels, or not supported: copy by hand.
                    libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM if done == 0 => {
                        kernel_copy = false;
                        continue;
                    }
                    libc::EINTR => continue,
                    e => return Err(e),
                }
            }
            n
        } else {
            if buf.is_empty() {
                buf.resize(1 << 20, 0);
            }
            let n = unsafe { libc::read(input.0, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                match errno() {
                    libc::EINTR => continue,
                    e => return Err(e),
                }
            }
            let mut off = 0usize;
            while off < n as usize {
                let w = unsafe { libc::write(output.0, buf[off..].as_ptr().cast(), n as usize - off) };
                if w < 0 {
                    match errno() {
                        libc::EINTR => continue,
                        e => return Err(e),
                    }
                }
                off += w as usize;
            }
            n
        };
        if n == 0 {
            return Ok(());
        }
        done += n as u64;
        progress(done);
    }
}

/// Whether `a` and the folder `dest` are on one filesystem (so a move is a
/// rename).
pub fn same_volume(a: &[u8], dest: &[u8]) -> bool {
    match (sys::stat_full(a, false), sys::stat_full(dest, true)) {
        (Ok(x), Ok(y)) => (x.dev_major, x.dev_minor) == (y.dev_major, y.dev_minor),
        _ => false,
    }
}

/// The mount holding `path` (longest matching mount point of its real path).
pub fn mount_of(path: &[u8]) -> Option<sys::Mount> {
    let real = sys::real_path(path).or_else(|| super::fsys::parent(path).and_then(|p| sys::real_path(&p)))?;
    sys::mounts().into_iter().filter(|m| super::fsys::in_scope(&real, &m.point)).max_by_key(|m| m.point.len())
}

pub fn is_network(path: &[u8]) -> bool {
    mount_of(path).is_some_and(|m| sys::is_network_fs(&m.fstype) || m.fstype == "fuse.gvfsd-fuse")
}

/// Whether the disk holding `path` spins (seeks are costly), from the block
/// device's `queue/rotational` (a partition's is on its parent disk).
pub fn seek_penalty(path: &[u8]) -> Option<bool> {
    let s = sys::stat_full(path, true).ok()?;
    let base = format!("/sys/dev/block/{}:{}", s.dev_major, s.dev_minor);
    for rel in ["queue/rotational", "../queue/rotational"] {
        if let Ok(v) = std::fs::read_to_string(format!("{base}/{rel}")) {
            return Some(v.trim() == "1");
        }
    }
    None
}

/// Whether a mounted volume can be safely removed: its device is on USB, a
/// memory card, FireWire or optical, marked removable by the kernel, or a
/// disk image the desktop attached.
pub fn ejectable(m: &sys::Mount) -> bool {
    if !m.source.starts_with(b"/dev/") || sys::is_network_fs(&m.fstype) {
        return false;
    }
    let Some(real) = sys::real_path(&m.source) else { return false };
    let name = String::from_utf8_lossy(super::fsys::base_name(&real)).into_owned();
    let Ok(dev) = std::fs::canonicalize(format!("/sys/class/block/{name}")) else { return false };
    // A partition's attributes live on its disk, one level up.
    let disk = if dev.join("partition").exists() { dev.parent().map(|p| p.to_path_buf()).unwrap_or(dev.clone()) } else { dev.clone() };
    let flag = |f: &str| std::fs::read_to_string(disk.join(f)).is_ok_and(|v| v.trim() == "1");
    let path = disk.to_string_lossy();
    // A disk image attached through the desktop (udisks sets autoclear) is
    // ejected like a stick: unmounted, then detached.
    let image = name.starts_with("loop") && flag("loop/autoclear");
    flag("removable") || path.contains("/usb") || path.contains("/mmc") || path.contains("/firewire") || name.starts_with("sr") || image
}

/// Unmount a removable volume and power its drive down (or detach a disk
/// image), as the desktop's file manager does, through udisks: polkit lets
/// the user's own removable drives go without a password, and asks with the
/// desktop's dialog when one is needed. GIO is the fallback (where udisksctl
/// isn't installed), bounded by a timeout since it can wait on questions
/// nobody can answer.
pub fn eject(point: &[u8], source: &[u8]) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let run = |cmd: &str, args: &[&std::ffi::OsStr], limit: Duration| -> Result<(), String> {
        let mut child = Command::new(cmd)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{cmd}: {e}"))?;
        let deadline = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut e, &mut err);
                    }
                    let err = err.trim().rsplit(": ").next().unwrap_or("").trim_end_matches('.').to_string();
                    return if status.success() { Ok(()) } else { Err(if err.is_empty() { format!("{cmd} failed") } else { err }) };
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{cmd} did not finish"));
                }
            }
        }
    };
    let dev = sys::os(source);
    let udisks = run("udisksctl", &["unmount".as_ref(), "-b".as_ref(), dev], Duration::from_secs(60));
    match udisks {
        Ok(()) => {
            // Best effort: the volume is already safe to remove once unmounted.
            // A desktop-attached image (autoclear) detaches itself, and
            // udisksctl waits forever for a device that is gone, so the
            // follow-up only runs on a device that is still there.
            let name = String::from_utf8_lossy(super::fsys::base_name(&sys::real_path(source).unwrap_or_else(|| source.to_vec()))).into_owned();
            let image = name.starts_with("loop");
            std::thread::sleep(Duration::from_millis(300));
            let present = std::path::Path::new(&format!("/sys/class/block/{name}")).exists()
                && (!image || std::fs::read_to_string(format!("/sys/class/block/{name}/loop/backing_file")).is_ok());
            if present {
                let last = if image { "loop-delete" } else { "power-off" };
                let _ = run("udisksctl", &[last.as_ref(), "--no-user-interaction".as_ref(), "-b".as_ref(), dev], Duration::from_secs(5));
            }
            Ok(())
        }
        Err(e) if e.contains("busy") => Err("files on it are still open in another program; close them and try again".into()),
        Err(e) if e.starts_with("udisksctl:") => run("gio", &["mount".as_ref(), "-e".as_ref(), sys::os(point)], Duration::from_secs(20)),
        Err(e) => Err(e),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fex-fileop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn b(p: &std::path::Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    #[test]
    fn copies_data_mode_times_and_links() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("copy");
        let src = d.join("a.bin");
        std::fs::write(&src, vec![9u8; 3 * 1024 * 1024 + 7]).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
        let dst = d.join("b.bin");
        let mut last = 0;
        copy_file(&b(&src), &b(&dst), false, &AtomicBool::new(false), &mut |n| last = n).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap().len(), 3 * 1024 * 1024 + 7);
        assert_eq!(last, 3 * 1024 * 1024 + 7);
        let (s, t) = (stat(&b(&src)).unwrap(), stat(&b(&dst)).unwrap());
        assert_eq!(t.mode & 0o777, 0o640);
        assert_eq!(s.mtime, t.mtime);
        // No overwrite unless asked.
        assert_eq!(copy_file(&b(&src), &b(&dst), false, &AtomicBool::new(false), &mut |_| {}), Err(libc::EEXIST));

        std::os::unix::fs::symlink("a.bin", d.join("ln")).unwrap();
        copy_file(&b(&d.join("ln")), &b(&d.join("ln2")), false, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(std::fs::read_link(d.join("ln2")).unwrap().as_os_str(), "a.bin");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn rename_refuses_to_replace() {
        let d = scratch("rename");
        std::fs::write(d.join("x"), b"x").unwrap();
        std::fs::write(d.join("y"), b"y").unwrap();
        assert_eq!(rename(&b(&d.join("x")), &b(&d.join("y")), false), Err(libc::EEXIST));
        rename(&b(&d.join("x")), &b(&d.join("z")), false).unwrap();
        assert!(same_volume(&b(&d.join("z")), &b(&d)));
        let _ = std::fs::remove_dir_all(d);
    }
}
