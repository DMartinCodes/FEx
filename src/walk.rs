//! Work-stealing parallel directory traversal shared by the indexer, the
//! transfer planner and folder-size calculation.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::fsys;
use crate::win::{self, RawEntry};

struct State {
    pending: Vec<Arc<str>>,
    idle: usize,
    /// Workers still taking part. One that panics leaves, so the others
    /// don't wait forever for it to go idle.
    live: usize,
}

/// Shared directory stack with idle tracking for termination detection.
pub struct Queue {
    state: Mutex<State>,
    cv: Condvar,
    idle_hint: AtomicUsize,
}

impl Queue {
    pub fn new(roots: Vec<Arc<str>>, workers: usize) -> Queue {
        Queue {
            state: Mutex::new(State { pending: roots, idle: 0, live: workers }),
            cv: Condvar::new(),
            idle_hint: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn take(&self, cancel: &AtomicBool) -> Option<Arc<str>> {
        let mut st = self.lock();
        st.idle += 1;
        self.idle_hint.store(st.idle, Relaxed);
        loop {
            if let Some(d) = st.pending.pop() {
                st.idle -= 1;
                self.idle_hint.store(st.idle, Relaxed);
                return Some(d);
            }
            if st.idle >= st.live || cancel.load(Relaxed) {
                // Everyone is idle with nothing queued: the walk is done.
                self.cv.notify_all();
                return None;
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    pub fn give(&self, dirs: impl Iterator<Item = Arc<str>>) {
        self.lock().pending.extend(dirs);
        self.cv.notify_all();
    }

    /// Hand the shallower half of `local` to the queue if a worker is starving.
    #[inline]
    pub fn balance(&self, local: &mut Vec<Arc<str>>) {
        if local.len() > 1 && self.idle_hint.load(Relaxed) > 0 {
            let k = local.len() / 2;
            self.give(local.drain(..k));
        }
    }

    /// Each worker holds one of these for as long as it runs. If the worker
    /// panics, dropping it takes the worker out of the walk.
    pub fn enlist(&self) -> Enlisted<'_> {
        Enlisted(self)
    }
}

pub struct Enlisted<'a>(&'a Queue);

impl Drop for Enlisted<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.lock().live -= 1;
            self.0.cv.notify_all();
        }
    }
}

pub fn cores() -> usize {
    thread::available_parallelism().map_or(4, |n| n.get())
}

/// Walk every directory under `roots` on `workers` threads. Each thread owns
/// a state from `init`; `visit` sees each entry (or the error code for a
/// directory that couldn't be read). Junctions/symlinks are not followed.
/// Returns the states of the workers that finished and how many failed
/// (panicked); after a failure the walk is incomplete.
pub fn walk<S: Send>(
    roots: &[Arc<str>],
    workers: usize,
    cancel: &AtomicBool,
    init: impl Fn() -> S + Sync,
    visit: impl Fn(&mut S, &Arc<str>, Result<(&str, &RawEntry), u32>) + Sync,
) -> (Vec<S>, usize) {
    let queue = Queue::new(roots.to_vec(), workers);
    thread::scope(|sc| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                sc.spawn(|| {
                    let _enlisted = queue.enlist();
                    let mut state = init();
                    let mut local: Vec<Arc<str>> = Vec::new();
                    let mut name = String::new();
                    while let Some(dir) = local.pop().or_else(|| queue.take(cancel)) {
                        if cancel.load(Relaxed) {
                            break;
                        }
                        let r = win::scan_dir(&dir, |e| {
                            name.clear();
                            win::push_utf16(&mut name, e.name);
                            visit(&mut state, &dir, Ok((&name, &e)));
                            if e.attrs & win::ATTR_DIR != 0 && e.attrs & win::ATTR_REPARSE == 0 {
                                local.push(fsys::join(&dir, &name).into());
                            }
                        });
                        if let Err(code) = r {
                            visit(&mut state, &dir, Err(code));
                        }
                        queue.balance(&mut local);
                    }
                    state
                })
            })
            .collect();
        let mut failed = 0;
        let states = handles
            .into_iter()
            .filter_map(|h| h.join().map_err(|_| failed += 1).ok())
            .collect();
        (states, failed)
    })
}
