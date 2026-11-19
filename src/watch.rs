//! Live updates via ReadDirectoryChangesW (works on local volumes and SMB
//! shares). The open folder is watched on its own: notifications are
//! coalesced per name and each affected entry is re-read once, so a burst of
//! writes to one file becomes a single update. A search index watches its
//! whole tree (`watch_tree`).

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};
use windows_sys::Win32::System::Threading::CreateEventW;

use crate::fileop;
use crate::fsys::{self, Row};
use crate::win;

/// `FILE_ACTION_MODIFIED`: an entry's data or metadata changed (not its name).
pub const MODIFIED: u32 = 3;

pub enum Change {
    /// Entry exists now (new, or metadata changed).
    Upsert(Row),
    Removed(Box<str>),
    /// Too many changes to track individually; reload the listing.
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
pub fn watch(dir: String, sink: impl Fn(Vec<Change>) + Send + 'static) -> Watcher {
    let base = dir.clone();
    spawn(dir, false, Duration::from_millis(100), false, move |batch| {
        let Some(changes) = batch else {
            sink(vec![Change::Rescan]); // the kernel's buffer overflowed
            return;
        };
        let mut names: Vec<String> = Vec::new();
        for (_, name) in changes {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let changes = names
            .into_iter()
            .map(|name| match fileop::stat(&fsys::join(&base, &name)) {
                Some(st) => Change::Upsert(Row { name: name.into(), dir: None, size: st.size, mtime: st.mtime, attrs: st.attrs, sel: false }),
                None => Change::Removed(name.into()),
            })
            .collect();
        sink(changes);
    })
}

/// Watch everything below `dir`. `sink` receives (action, path relative to
/// `dir`) pairs, or `None` when changes were lost: the kernel's buffer
/// overflowed, or the watch ended on its own (the folder couldn't be
/// opened or watched, or the sink failed), after which nothing more comes.
pub fn watch_tree(dir: String, sink: impl FnMut(Option<Vec<(u32, String)>>) + Send + 'static) -> Watcher {
    spawn(dir, true, Duration::from_millis(500), true, sink)
}

fn spawn(
    dir: String,
    subtree: bool,
    pause: Duration,
    report_end: bool,
    mut sink: impl FnMut(Option<Vec<(u32, String)>>) + Send + 'static,
) -> Watcher {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    thread::spawn(move || {
        // A failing sink ends the watch instead of the application.
        let _ = catch_unwind(AssertUnwindSafe(|| run(&dir, subtree, pause, &s, &mut sink)));
        // Ended without being asked to: whoever relies on it must not go on
        // believing it's current.
        if report_end && !s.load(Relaxed) {
            let _ = catch_unwind(AssertUnwindSafe(|| sink(None)));
        }
    });
    Watcher { stop }
}

fn run(dir: &str, subtree: bool, pause: Duration, stop: &AtomicBool, sink: &mut dyn FnMut(Option<Vec<(u32, String)>>)) {
    let path = win::wide(dir, "");
    unsafe {
        let h = CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        let event = CreateEventW(null(), 1, 0, null());
        if event.is_null() {
            CloseHandle(h);
            return;
        }
        // Closes both handles however the loop ends (including a panicking sink).
        struct Handles(windows_sys::Win32::Foundation::HANDLE, windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Handles {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                    CloseHandle(self.1);
                }
            }
        }
        let _handles = Handles(event, h);
        // DWORD-aligned; 64 KB is the maximum for network shares.
        let mut buf = vec![0u32; 16 * 1024];
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_ATTRIBUTES;

        'outer: while !stop.load(Relaxed) {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = event;
            let ok = ReadDirectoryChangesW(
                h,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as u32,
                subtree as i32,
                filter,
                null_mut(),
                &mut ov,
                None,
            );
            if ok == 0 {
                break;
            }
            let mut bytes = 0u32;
            loop {
                if GetOverlappedResultEx(h, &ov, &mut bytes, 250, 0) != 0 {
                    break;
                }
                if GetLastError() != WAIT_TIMEOUT {
                    break 'outer; // directory deleted, share disconnected, ...
                }
                if stop.load(Relaxed) {
                    CancelIoEx(h, &ov);
                    GetOverlappedResult(h, &ov, &mut bytes, 1);
                    break 'outer;
                }
            }
            sink(if bytes == 0 { None } else { Some(parse(&buf, bytes as usize)) });
            // Let bursts accumulate; the handle buffers changes between reads.
            thread::sleep(pause);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    /// Fold notifications until 500 ms of quiet; returns name → size (None = removed)
    /// and how many Upserts were seen.
    fn settle(rx: &Receiver<Vec<Change>>) -> (std::collections::HashMap<String, Option<u64>>, usize) {
        let mut state = std::collections::HashMap::new();
        let mut upserts = 0;
        let mut wait = Duration::from_secs(5);
        while let Ok(batch) = rx.recv_timeout(wait) {
            wait = Duration::from_millis(500);
            for c in batch {
                match c {
                    Change::Upsert(r) => {
                        upserts += 1;
                        state.insert(r.name.to_string(), Some(r.size));
                    }
                    Change::Removed(n) => {
                        state.insert(n.to_string(), None);
                    }
                    Change::Rescan => panic!("unexpected rescan"),
                }
            }
        }
        (state, upserts)
    }

    #[test]
    fn a_tree_watch_that_cannot_start_reports_lost_changes() {
        let missing = std::env::temp_dir().join(format!("fex-watch-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let (tx, rx) = channel();
        let _w = watch_tree(missing.to_string_lossy().into_owned(), move |batch| {
            let _ = tx.send(batch.is_none());
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true), "the end of the watch must be reported as lost changes");
    }

    #[test]
    fn add_modify_remove() {
        let dir = std::env::temp_dir().join(format!("fex-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = channel();
        let _w = watch(dir.to_string_lossy().into_owned(), move |c| {
            let _ = tx.send(c);
        });
        thread::sleep(Duration::from_millis(200));

        std::fs::write(dir.join("new.txt"), b"1").unwrap();
        let (s, _) = settle(&rx);
        assert_eq!(s.get("new.txt"), Some(&Some(1)));

        std::fs::write(dir.join("new.txt"), b"12345").unwrap();
        let (s, upserts) = settle(&rx);
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("new.txt"), Some(&Some(5)));
        assert!(upserts <= 3, "bursts should be coalesced, got {upserts} updates");

        // 200 files created at once arrive as a few batches, not 200 events.
        for i in 0..200 {
            std::fs::write(dir.join(format!("bulk{i}")), b"x").unwrap();
        }
        let (s, _) = settle(&rx);
        assert_eq!(s.values().filter(|v| v.is_some()).count(), 200);

        std::fs::remove_file(dir.join("new.txt")).unwrap();
        let (s, _) = settle(&rx);
        assert_eq!(s.get("new.txt"), Some(&None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Distinct (action, name) pairs in a FILE_NOTIFY_INFORMATION chain, in
/// first-seen order.
fn parse(buf: &[u32], len: usize) -> Vec<(u32, String)> {
    let len = len.min(buf.len() * 4);
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len) };
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut off = 0usize;
    while off + 12 <= len {
        let next = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
        let name_len = u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap()) as usize;
        let start = off + 12;
        if start + name_len > len {
            break;
        }
        let units: Vec<u16> = bytes[start..start + name_len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let mut name = String::new();
        win::push_utf16(&mut name, &units);
        if !out.iter().any(|(a, n)| *a == action && *n == name) {
            out.push((action, name));
        }
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}
