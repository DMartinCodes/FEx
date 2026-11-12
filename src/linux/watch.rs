//! Live updates for the open folder via inotify (the Linux side of
//! `watch::watch`). As on Windows, notifications are coalesced per name and
//! each affected entry is re-read once, so a burst of writes to one file
//! becomes a single update.
//!
//! inotify watches one folder per watch, which suits the open folder
//! exactly. Whole-tree watching for search indexes is a separate problem
//! (per-folder watch limits, or fanotify with CAP_SYS_ADMIN) not solved here.
//! inotify also doesn't see changes made by other clients of NFS/CIFS mounts.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::fsys::Row;
use super::sys::{self, Dir, DIR};

pub enum Change {
    Upsert(Row),
    Removed(Box<[u8]>),
    /// Changes were lost (queue overflow) or the folder itself went away.
    Rescan,
}

/// Stops the watcher thread when dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// Watch `dir` (non-recursively), delivering coalesced batches to `sink`.
pub fn watch(dir: Vec<u8>, sink: impl Fn(Vec<Change>) + Send + 'static) -> Option<Watcher> {
    let path = sys::cstr(&dir).ok()?;
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mask = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR
        | libc::IN_EXCL_UNLINK;
    if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) } < 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    thread::spawn(move || {
        struct Fd(i32);
        impl Drop for Fd {
            fn drop(&mut self) {
                unsafe { libc::close(self.0) };
            }
        }
        let fd = Fd(fd);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(fd.0, &dir, &s, &sink)));
    });
    Some(Watcher { stop })
}

fn run(fd: i32, dir: &[u8], stop: &AtomicBool, sink: &dyn Fn(Vec<Change>)) {
    let mut buf = vec![0u64; 8 * 1024];
    while !stop.load(Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, 250) };
        if ready <= 0 {
            continue;
        }
        // Let a burst accumulate in the kernel queue, then drain it all.
        thread::sleep(Duration::from_millis(100));
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut rescan = false;
        let mut gone = false;
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len() * 8) };
            if n <= 0 {
                break;
            }
            // SAFETY: `n` bytes were written by the kernel.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
            let mut off = 0;
            // struct inotify_event { i32 wd; u32 mask; u32 cookie; u32 len; char name[len]; }
            while off + 16 <= bytes.len() {
                let mask = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(bytes[off + 12..off + 16].try_into().unwrap()) as usize;
                let raw = &bytes[off + 16..(off + 16 + len).min(bytes.len())];
                let name = &raw[..memchr::memchr(0, raw).unwrap_or(raw.len())];
                off += 16 + len;
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    rescan = true;
                } else if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
                    gone = true;
                } else if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_vec());
                }
            }
        }
        if rescan || gone {
            sink(vec![Change::Rescan]);
            if gone {
                return;
            }
            continue;
        }
        if names.is_empty() {
            continue;
        }
        let Ok(handle) = Dir::open(dir) else {
            sink(vec![Change::Rescan]);
            return;
        };
        let changes = names
            .into_iter()
            .map(|name| match handle.stat(&name) {
                Some(m) => {
                    let mut flags = m.flags;
                    if flags & sys::LINK != 0 && handle.link_is_dir(&name) {
                        flags |= DIR;
                    }
                    let size = if flags & DIR != 0 { 0 } else { m.size };
                    Change::Upsert(Row { name: name.into(), dir: None, size, mtime: m.mtime, flags, sel: false })
                }
                None => Change::Removed(name.into()),
            })
            .collect();
        sink(changes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc::channel;

    #[test]
    fn add_modify_remove() {
        let dir = std::env::temp_dir().join(format!("fex-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = channel();
        let _w = watch(dir.as_os_str().as_bytes().to_vec(), move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        let settle = || {
            let mut state = std::collections::HashMap::new();
            let mut wait = Duration::from_secs(5);
            while let Ok(batch) = rx.recv_timeout(wait) {
                wait = Duration::from_millis(400);
                for c in batch {
                    match c {
                        Change::Upsert(r) => drop(state.insert(r.name.to_vec(), Some(r.size))),
                        Change::Removed(n) => drop(state.insert(n.to_vec(), None)),
                        Change::Rescan => panic!("unexpected rescan"),
                    }
                }
            }
            state
        };
        std::fs::write(dir.join("new.txt"), b"12345").unwrap();
        assert_eq!(settle().get(&b"new.txt"[..]), Some(&Some(5)));
        for i in 0..200 {
            std::fs::write(dir.join(format!("bulk{i}")), b"x").unwrap();
        }
        assert_eq!(settle().values().filter(|v| v.is_some()).count(), 200);
        std::fs::remove_file(dir.join("new.txt")).unwrap();
        assert_eq!(settle().get(&b"new.txt"[..]), Some(&None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
