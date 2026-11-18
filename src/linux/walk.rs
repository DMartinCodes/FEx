//! Work-stealing parallel directory traversal (the Linux side of `walk.rs`;
//! same queue design, byte paths, getdents scans).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use super::fsys;
use super::sys::{Dir, RawEntry, DIR, LINK};

struct State {
    pending: Vec<Arc<[u8]>>,
    idle: usize,
    /// Workers still taking part; one that panics leaves.
    live: usize,
}

struct Queue {
    state: Mutex<State>,
    cv: Condvar,
    idle_hint: AtomicUsize,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self, cancel: &AtomicBool) -> Option<Arc<[u8]>> {
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
                self.cv.notify_all();
                return None;
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Hand the shallower half of `local` to the queue if a worker is starving.
    fn balance(&self, local: &mut Vec<Arc<[u8]>>) {
        if local.len() > 1 && self.idle_hint.load(Relaxed) > 0 {
            let k = local.len() / 2;
            self.lock().pending.extend(local.drain(..k));
            self.cv.notify_all();
        }
    }
}

struct Enlisted<'a>(&'a Queue);

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

/// What a walk's visitor sees for each folder: `Begin` once it is open
/// (empty folders included), then each entry with the open handle (for
/// relative `statx`); or `Unreadable` for a folder that couldn't be opened.
pub enum Visit<'a, 'b> {
    Begin(&'a Dir),
    Entry(&'a Dir, RawEntry<'b>),
    Unreadable,
}

/// Walk every folder under `roots` on `workers` threads. Links are not
/// followed and folders in `skip` (pseudo filesystems) are not entered.
/// `pace` runs before each folder (background crawls pause there); `init`
/// runs once on each worker thread (and can set its priority). `strict`
/// refuses any path with a symbolic link in it (`Dir::open_strict`), for
/// crawls made with more privileges than the people who search them.
/// Returns the states of the workers that finished and how many panicked.
#[allow(clippy::too_many_arguments)]
pub fn walk<S: Send>(
    roots: &[Arc<[u8]>],
    workers: usize,
    cancel: &AtomicBool,
    skip: &HashSet<Vec<u8>>,
    strict: bool,
    pace: &(dyn Fn() + Sync),
    init: impl Fn() -> S + Sync,
    visit: impl Fn(&mut S, &Arc<[u8]>, Visit<'_, '_>) + Sync,
) -> (Vec<S>, usize) {
    let queue = Queue {
        state: Mutex::new(State { pending: roots.to_vec(), idle: 0, live: workers }),
        cv: Condvar::new(),
        idle_hint: AtomicUsize::new(0),
    };
    thread::scope(|sc| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                sc.spawn(|| {
                    let _enlisted = Enlisted(&queue);
                    let mut state = init();
                    let mut local: Vec<Arc<[u8]>> = Vec::new();
                    let mut buf = Vec::new();
                    while let Some(path) = local.pop().or_else(|| queue.take(cancel)) {
                        if cancel.load(Relaxed) {
                            break;
                        }
                        pace();
                        let opened = if strict { Dir::open_strict(&path) } else { Dir::open(&path) };
                        let r = opened.and_then(|dir| {
                            visit(&mut state, &path, Visit::Begin(&dir));
                            dir.scan(&mut buf, |e| {
                                let sub = e.flags & (DIR | LINK) == DIR;
                                let name = e.name;
                                visit(&mut state, &path, Visit::Entry(&dir, e));
                                if sub {
                                    let full = fsys::join(&path, name);
                                    if !skip.contains(&full) {
                                        local.push(full.into());
                                    }
                                }
                            })
                        });
                        if r.is_err() {
                            visit(&mut state, &path, Visit::Unreadable);
                        }
                        queue.balance(&mut local);
                    }
                    state
                })
            })
            .collect();
        let mut failed = 0;
        let states = handles.into_iter().filter_map(|h| h.join().map_err(|_| failed += 1).ok()).collect();
        (states, failed)
    })
}
