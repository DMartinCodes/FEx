//! Persistent worker threads for short parallel bursts (index queries,
//! sorting results), so each burst doesn't pay for spawning a thread per core.

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

struct Pool {
    /// (batch number, current batch)
    current: Mutex<(u64, Option<Arc<Batch>>)>,
    wake: Condvar,
    finished: Condvar,
    /// One batch at a time.
    busy: Mutex<()>,
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
        Pool {
            current: Mutex::new((0, None)),
            wake: Condvar::new(),
            finished: Condvar::new(),
            busy: Mutex::new(()),
            workers,
        }
    })
}

fn worker() {
    let p = pool();
    let mut seen = 0;
    loop {
        let batch = {
            let mut g = lock(&p.current);
            while g.0 == seen {
                g = p.wake.wait(g).unwrap_or_else(PoisonError::into_inner);
            }
            seen = g.0;
            g.1.clone()
        };
        if let Some(b) = batch {
            work(p, &b);
        }
    }
}

fn work(p: &Pool, b: &Batch) {
    loop {
        let i = b.next.fetch_add(1, Ordering::Relaxed);
        if i >= b.n {
            return;
        }
        // SAFETY: see `Batch::task`.
        let f = unsafe { &*b.task };
        if catch_unwind(AssertUnwindSafe(|| f(i))).is_err() {
            b.panicked.store(true, Ordering::Relaxed);
        }
        if b.done.fetch_add(1, Ordering::AcqRel) + 1 == b.n {
            let _g = lock(&p.current);
            p.finished.notify_all();
        }
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
    let _one = lock(&p.busy);
    // SAFETY: only the lifetime is erased; we wait below until no thread can call it.
    let task: *const Task = unsafe { std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), *const Task>(f) };
    let batch = Arc::new(Batch {
        task,
        n,
        next: AtomicUsize::new(0),
        done: AtomicUsize::new(0),
        panicked: AtomicBool::new(false),
    });
    {
        let mut g = lock(&p.current);
        g.0 += 1;
        g.1 = Some(batch.clone());
        p.wake.notify_all();
    }
    work(p, &batch);
    let mut g = lock(&p.current);
    while batch.done.load(Ordering::Acquire) < n {
        g = p.finished.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
    g.1 = None;
    !batch.panicked.load(Ordering::Relaxed)
}
