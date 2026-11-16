//! Persistent worker threads for short parallel bursts (index queries,
//! sorting results), so each burst doesn't pay for spawning a thread per core.
//!
//! Several bursts can run at once (queries from different clients of the
//! index daemon or service): each caller works on its own batch, and idle
//! workers take one item at a time from the active batches in turn, so a
//! long batch doesn't hold up a short one.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

type Task = dyn Fn(usize) + Sync;

struct Batch {
    /// Only dereferenced for claimed indices below `n`; `for_each` doesn't
    /// return (so the closure outlives every call) until all `n` are done.
    task: *const Task,
    n: usize,
    next: AtomicUsize,
    done: AtomicUsize,
    panicked: AtomicBool,
}

// SAFETY: `task` points to a `Sync` closure that `for_each` keeps alive while
// any thread can still call it.
unsafe impl Send for Batch {}
unsafe impl Sync for Batch {}

impl Batch {
    fn has_work(&self) -> bool {
        self.next.load(Ordering::Relaxed) < self.n
    }

    /// Claim and run one item; false when none was left.
    fn run_one(&self, p: &Pool) -> bool {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        if i >= self.n {
            return false;
        }
        // SAFETY: see `Batch::task`.
        let f = unsafe { &*self.task };
        if catch_unwind(AssertUnwindSafe(|| f(i))).is_err() {
            self.panicked.store(true, Ordering::Relaxed);
        }
        if self.done.fetch_add(1, Ordering::AcqRel) + 1 == self.n {
            let _g = lock(&p.state);
            p.finished.notify_all();
        }
        true
    }
}

struct State {
    /// Batches with unclaimed items (or not yet removed by their caller).
    active: Vec<Arc<Batch>>,
    /// Where the next idle worker starts looking, for fairness.
    cursor: usize,
}

struct Pool {
    state: Mutex<State>,
    wake: Condvar,
    finished: Condvar,
    workers: usize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        // The calling thread works too.
        // If threads can't be created, the caller simply does more of the work.
        let workers = (1..std::thread::available_parallelism().map_or(4, |n| n.get()))
            .filter(|_| thread::Builder::new().name("fex-pool".into()).spawn(worker).is_ok())
            .count();
        Pool { state: Mutex::new(State { active: Vec::new(), cursor: 0 }), wake: Condvar::new(), finished: Condvar::new(), workers }
    })
}

/// The next batch with work, taking batches in turn.
fn pick(st: &mut State) -> Option<Arc<Batch>> {
    let n = st.active.len();
    for k in 0..n {
        let i = (st.cursor + k) % n;
        if st.active[i].has_work() {
            st.cursor = i + 1;
            return Some(st.active[i].clone());
        }
    }
    None
}

fn worker() {
    let p = pool();
    loop {
        let batch = {
            let mut g = lock(&p.state);
            loop {
                if let Some(b) = pick(&mut g) {
                    break b;
                }
                g = p.wake.wait(g).unwrap_or_else(PoisonError::into_inner);
            }
        };
        // One item, then look again, so concurrent batches share the workers.
        batch.run_one(p);
    }
}

/// Threads that take part in a batch (pool workers plus the caller).
pub fn threads() -> usize {
    pool().workers + 1
}

/// Run `f(0)`, ..., `f(n - 1)` across the pool and the calling thread.
/// Returns false if any call panicked (the others still ran).
pub fn for_each(n: usize, f: &(dyn Fn(usize) + Sync)) -> bool {
    if n == 0 {
        return true;
    }
    let p = pool();
    // SAFETY: only the lifetime is erased; we wait below until no thread can call it.
    let task: *const Task = unsafe { std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), *const Task>(f) };
    let batch = Arc::new(Batch { task, n, next: AtomicUsize::new(0), done: AtomicUsize::new(0), panicked: AtomicBool::new(false) });
    lock(&p.state).active.push(batch.clone());
    p.wake.notify_all();
    while batch.run_one(p) {}
    let mut g = lock(&p.state);
    while batch.done.load(Ordering::Acquire) < n {
        g = p.finished.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
    g.active.retain(|b| !Arc::ptr_eq(b, &batch));
    !batch.panicked.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn concurrent_batches_all_complete() {
        let handles: Vec<_> = (0..6)
            .map(|t| {
                thread::spawn(move || {
                    let sum = AtomicUsize::new(0);
                    assert!(for_each(1000, &|i| {
                        sum.fetch_add(i + t, Ordering::Relaxed);
                    }));
                    sum.load(Ordering::Relaxed)
                })
            })
            .collect();
        for (t, h) in handles.into_iter().enumerate() {
            assert_eq!(h.join().unwrap(), (0..1000).sum::<usize>() + 1000 * t);
        }
    }

    #[test]
    fn a_long_batch_does_not_hold_up_a_short_one() {
        let slow = thread::spawn(|| {
            let t = Instant::now();
            for_each(threads() * 2, &|_| thread::sleep(Duration::from_millis(300)));
            t.elapsed()
        });
        thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        let ran = AtomicUsize::new(0);
        for_each(4, &|_| {
            ran.fetch_add(1, Ordering::Relaxed);
        });
        let quick = t.elapsed();
        assert_eq!(ran.load(Ordering::Relaxed), 4);
        assert!(quick < Duration::from_millis(250), "short batch waited {quick:?}");
        assert!(slow.join().unwrap() >= Duration::from_millis(550));
    }

    #[test]
    fn nested_batches_and_panics() {
        let inner = AtomicUsize::new(0);
        assert!(for_each(4, &|_| {
            for_each(4, &|_| {
                inner.fetch_add(1, Ordering::Relaxed);
            });
        }));
        assert_eq!(inner.load(Ordering::Relaxed), 16);
        assert!(!for_each(3, &|i| assert_ne!(i, 1)));
    }
}
