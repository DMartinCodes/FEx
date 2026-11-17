//! Whole-filesystem change notification for the index daemon.
//!
//! One `FAN_MARK_FILESYSTEM` mark per indexed filesystem reports every
//! create, delete and rename on it, as (parent folder handle, name). The
//! daemon only needs the parent folder: that is the listing to re-read.
//! This needs CAP_SYS_ADMIN (marks) and CAP_DAC_READ_SEARCH (turning handles
//! back into paths), which is why it lives in a system daemon; inotify would
//! need one watch per folder. Kernel 5.1+ for directory handles with names.
//!
//! Network filesystems report nothing for other clients' changes, and some
//! filesystems refuse filesystem marks; those stay with the periodic sweep.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::sys;

/// Info record types (linux/fanotify.h).
const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;
const FAN_EVENT_INFO_TYPE_DFID: u8 = 3;

pub struct Fanotify {
    fd: OwnedFd,
    /// Filesystem id -> an open folder on it, for `open_by_handle_at`.
    mounts: HashMap<[i32; 2], OwnedFd>,
    pub marked: Vec<Vec<u8>>,
    pub refused: Vec<(Vec<u8>, String)>,
}

pub enum Events {
    /// Folders whose listings changed.
    Dirs(Vec<Vec<u8>>),
    /// The kernel queue overflowed: only a full sweep is accurate.
    Overflow,
}

fn fsid_of(fd: i32) -> Option<[i32; 2]> {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut st) } != 0 {
        return None;
    }
    // SAFETY: fsid_t is two i32s.
    Some(unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(st.f_fsid) })
}

impl Fanotify {
    /// Mark the filesystems mounted at `mounts`. Fails only if fanotify
    /// itself is unavailable; individual refusals are listed in `refused`.
    pub fn start(mounts: &[Vec<u8>]) -> Result<Fanotify, String> {
        let raw = unsafe {
            libc::fanotify_init(
                libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_NONBLOCK | libc::FAN_REPORT_DFID_NAME,
                (libc::O_RDONLY | libc::O_LARGEFILE) as u32,
            )
        };
        if raw < 0 {
            return Err(format!("fanotify unavailable: {}", std::io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut me = Fanotify { fd, mounts: HashMap::new(), marked: Vec::new(), refused: Vec::new() };
        let mask = libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO | libc::FAN_ONDIR;
        for m in mounts {
            let Ok(c) = sys::cstr(m) else { continue };
            let rc = unsafe { libc::fanotify_mark(me.fd.as_raw_fd(), libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM, mask, libc::AT_FDCWD, c.as_ptr()) };
            if rc != 0 {
                me.refused.push((m.clone(), std::io::Error::last_os_error().to_string()));
                continue;
            }
            let dir = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
            if dir >= 0 {
                let dir = unsafe { OwnedFd::from_raw_fd(dir) };
                if let Some(id) = fsid_of(dir.as_raw_fd()) {
                    me.mounts.insert(id, dir);
                }
            }
            me.marked.push(m.clone());
        }
        Ok(me)
    }

    /// Wait up to `timeout_ms` for events, then drain everything queued.
    pub fn wait(&self, timeout_ms: i32) -> Events {
        let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } <= 0 {
            return Events::Dirs(Vec::new());
        }
        let mut handles: HashSet<([i32; 2], Vec<u8>)> = HashSet::new();
        let mut buf = vec![0u64; 32 * 1024];
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len() * 8) };
            if n <= 0 {
                break;
            }
            // SAFETY: `n` bytes were written by the kernel.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
            let mut off = 0;
            // struct fanotify_event_metadata { u32 event_len; u8 vers; u8 reserved;
            //   u16 metadata_len; u64 mask; i32 fd; i32 pid; }, then info records.
            while off + 24 <= bytes.len() {
                let len = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
                let meta_len = u16::from_ne_bytes(bytes[off + 6..off + 8].try_into().unwrap()) as usize;
                let mask = u64::from_ne_bytes(bytes[off + 8..off + 16].try_into().unwrap());
                let fd = i32::from_ne_bytes(bytes[off + 16..off + 20].try_into().unwrap());
                if len < 24 || off + len > bytes.len() {
                    break;
                }
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
                if mask & libc::FAN_Q_OVERFLOW != 0 {
                    return Events::Overflow;
                }
                // Info record: u8 type; u8 pad; u16 len; fsid [i32; 2];
                //   struct file_handle { u32 bytes; i32 type; u8 handle[]; } [name\0]
                let mut r = off + meta_len;
                while r + 20 <= off + len {
                    let kind = bytes[r];
                    let rlen = u16::from_ne_bytes(bytes[r + 2..r + 4].try_into().unwrap()) as usize;
                    if rlen < 20 || r + rlen > off + len {
                        break;
                    }
                    if kind == FAN_EVENT_INFO_TYPE_DFID_NAME || kind == FAN_EVENT_INFO_TYPE_DFID {
                        let fsid = [i32::from_ne_bytes(bytes[r + 4..r + 8].try_into().unwrap()), i32::from_ne_bytes(bytes[r + 8..r + 12].try_into().unwrap())];
                        let hbytes = u32::from_ne_bytes(bytes[r + 12..r + 16].try_into().unwrap()) as usize;
                        if r + 20 + hbytes <= r + rlen {
                            // handle_bytes, handle_type and the handle itself, as the kernel wants them back.
                            handles.insert((fsid, bytes[r + 12..r + 20 + hbytes].to_vec()));
                        }
                    }
                    r += rlen;
                }
                off += len;
            }
        }
        Events::Dirs(handles.into_iter().filter_map(|(fsid, h)| self.resolve(fsid, &h)).collect())
    }

    /// Turn a folder handle back into its current path.
    fn resolve(&self, fsid: [i32; 2], handle: &[u8]) -> Option<Vec<u8>> {
        let mount = self.mounts.get(&fsid)?;
        // struct file_handle must be 4-aligned: copy into u32s.
        let mut aligned = vec![0u32; handle.len().div_ceil(4)];
        unsafe { std::ptr::copy_nonoverlapping(handle.as_ptr(), aligned.as_mut_ptr() as *mut u8, handle.len()) };
        let fd = unsafe { libc::open_by_handle_at(mount.as_raw_fd(), aligned.as_mut_ptr() as *mut libc::file_handle, libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return None; // the folder is gone; its parent's event covers it
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
        use std::os::unix::ffi::OsStrExt;
        let p = link.as_os_str().as_bytes();
        (p.starts_with(b"/") && !p.ends_with(b" (deleted)")).then(|| p.to_vec())
    }
}
