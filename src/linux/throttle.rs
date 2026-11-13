//! Background work that yields to everything else.
//!
//! * CPU: background threads run under `SCHED_IDLE`. The kernel gives them
//!   only CPU time nothing else wants and preempts them for any normal task,
//!   so an idle 18-core box crawls on all cores and a busy one barely notices.
//! * I/O: background threads get the idle I/O class. Only the BFQ scheduler
//!   honours it (NVMe defaults to `none`), so the daemon also watches
//!   pressure stall information (PSI): when other processes spend more than
//!   `io_limit` percent of the time stalled on I/O, or memory is under
//!   pressure, background work pauses between folders.
//! * Power: on battery, background work pauses.
//!
//! * Reserve: `SCHED_IDLE` gets nothing on a machine that is always fully
//!   loaded, so a first crawl there would never finish. A controller
//!   measures the CPU time background threads actually get (their own
//!   thread clocks, so queries don't count) and, while that is below the
//!   reserve (`profile.rs`: 0.5% to 5% of total capacity), steps them up
//!   through nice 19, 15, 10, 5, 0 under the normal scheduler; once they get
//!   enough, back down to `SCHED_IDLE`. On an idle machine they never leave
//!   `SCHED_IDLE`, which already gives them everything.
//!
//! Queries never run on these threads: answering a user stays at normal
//! priority, because speed is the point.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// Background priority steps: `SCHED_IDLE`, then these nice values.
const NICE_STEPS: [i32; 5] = [19, 15, 10, 5, 0];

/// Put the calling thread at background step `step` (0 = idle CPU and I/O
/// priority; higher = the normal scheduler at `NICE_STEPS[step - 1]`, with
/// the lowest best-effort I/O priority). Per thread on Linux.
fn set_step(step: usize) {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_BE: libc::c_long = 2;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    let param = libc::sched_param { sched_priority: 0 };
    unsafe {
        if step == 0 {
            libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
            libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0 as libc::c_long, IOPRIO_CLASS_IDLE << 13);
        } else {
            libc::sched_setscheduler(0, libc::SCHED_OTHER, &param);
            libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, NICE_STEPS[step - 1]);
            libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0 as libc::c_long, (IOPRIO_CLASS_BE << 13) | 7);
        }
    }
}

thread_local! {
    /// This thread's CPU time when it last reported, and its current step.
    static LAST_CPU: Cell<u64> = const { Cell::new(0) };
    static STEP: Cell<usize> = const { Cell::new(0) };
}

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Lower the calling thread to idle CPU and I/O priority (per thread on
/// Linux, so the rest of the process is unaffected).
pub fn background_thread() {
    set_step(0);
    STEP.with(|s| s.set(0));
    LAST_CPU.with(|c| c.set(thread_cpu_ns()));
}

/// `some avg10` from a PSI file: percent of the last 10 s in which at least
/// one task was stalled on the resource.
fn psi_some(path: &std::path::Path) -> Option<f64> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace().find_map(|f| f.strip_prefix("avg10="))?.parse().ok()
}

/// This process's own cgroup, when it has one (as a systemd service does).
/// Its I/O stalls are subtracted from the system's, so a crawl doesn't
/// throttle itself for the stalls it causes.
fn own_cgroup() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    (path != "/").then(|| PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// True when the machine has mains power supplies and none is online.
fn on_battery() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    let mut mains = false;
    for e in dir.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).is_ok_and(|t| t.trim() == "Mains") {
            mains = true;
            if std::fs::read_to_string(p.join("online")).is_ok_and(|o| o.trim() == "1") {
                return false;
            }
        }
    }
    mains
}

pub struct Throttle {
    paused: AtomicBool,
    reason: Mutex<String>,
    /// Pause when others' I/O stall share (PSI some avg10) exceeds this.
    io_limit: f64,
    pub own_cgroup: bool,
    /// Share of total CPU capacity guaranteed to background work (0 = none),
    /// as f64 bits so it can change once the real file count is known.
    reserve: AtomicU64,
    cores: usize,
    /// CPU time background threads have used (ns, cumulative).
    used_ns: AtomicU64,
    /// When a background thread last paced (monotonic ns): work is pending.
    last_pace: AtomicU64,
    /// Current background step (see `set_step`), set by the controller.
    step: AtomicUsize,
    /// Background share of total CPU over the last second or so.
    share_ppm: AtomicU64,
}

impl Throttle {
    /// Start watching pressure and power every half second, and enforce
    /// `reserve` (a share of total CPU capacity across `cores`) every 100 ms.
    pub fn start(io_limit: f64, reserve: f64, cores: usize) -> Arc<Throttle> {
        let cgroup = own_cgroup();
        let t = Arc::new(Throttle {
            paused: AtomicBool::new(false),
            reason: Mutex::new(String::new()),
            io_limit,
            own_cgroup: cgroup.is_some(),
            reserve: AtomicU64::new(reserve.to_bits()),
            cores: cores.max(1),
            used_ns: AtomicU64::new(0),
            last_pace: AtomicU64::new(0),
            step: AtomicUsize::new(0),
            share_ppm: AtomicU64::new(0),
        });
        let w = Arc::downgrade(&t);
        thread::Builder::new()
            .name("fex-reserve".into())
            .spawn(move || {
                let (mut used, mut at, mut ewma) = (0u64, mono_ns(), 0.0f64);
                while let Some(t) = w.upgrade() {
                    let (u, now) = (t.used_ns.load(Relaxed), mono_ns());
                    let share = (u - used) as f64 / ((now - at).max(1) as f64 * t.cores as f64);
                    (used, at) = (u, now);
                    ewma = crate::reserve::smooth(ewma, share);
                    t.share_ppm.store((ewma * 1e6) as u64, Relaxed);
                    let active = now.saturating_sub(t.last_pace.load(Relaxed)) < 300_000_000;
                    let step = t.step.load(Relaxed);
                    let reserve = t.reserve();
                    let next = crate::reserve::next_step(step, ewma, reserve, active, NICE_STEPS.len());
                    t.step.store(next, Relaxed);
                    drop(t);
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .ok();
        let w = Arc::downgrade(&t);
        thread::Builder::new()
            .name("fex-throttle".into())
            .spawn(move || {
                while let Some(t) = w.upgrade() {
                    t.check(cgroup.as_deref());
                    drop(t);
                    thread::sleep(Duration::from_millis(500));
                }
            })
            .ok();
        t
    }

    fn check(&self, cgroup: Option<&std::path::Path>) {
        let system = psi_some("/proc/pressure/io".as_ref()).unwrap_or(0.0);
        let own = cgroup.and_then(|c| psi_some(&c.join("io.pressure"))).unwrap_or(0.0);
        let others = (system - own).max(0.0);
        let memory = psi_some("/proc/pressure/memory".as_ref()).unwrap_or(0.0);
        let battery = on_battery();
        let was = self.paused.load(Relaxed);
        // Hysteresis: resume only well below the limits.
        let (io, mem) = if was { (self.io_limit / 2.0, 5.0) } else { (self.io_limit, 10.0) };
        let why = if battery {
            Some("on battery".to_string())
        } else if others > io {
            Some(format!("I/O pressure {others:.0}%"))
        } else if memory > mem {
            Some(format!("memory pressure {memory:.0}%"))
        } else {
            None
        };
        self.paused.store(why.is_some(), Relaxed);
        *self.reason.lock().unwrap_or_else(PoisonError::into_inner) = why.unwrap_or_default();
    }

    /// Called by background threads between folders: report CPU used,
    /// take on the controller's current step, and block while paused.
    pub fn wait(&self) {
        let now = thread_cpu_ns();
        let last = LAST_CPU.with(|c| c.replace(now));
        if last != 0 {
            self.used_ns.fetch_add(now.saturating_sub(last), Relaxed);
        }
        self.last_pace.store(mono_ns(), Relaxed);
        let step = self.step.load(Relaxed);
        if STEP.with(|s| s.replace(step)) != step {
            set_step(step);
        }
        while self.paused.load(Relaxed) {
            thread::sleep(Duration::from_millis(250));
            LAST_CPU.with(|c| c.set(thread_cpu_ns()));
        }
    }

    pub fn reserve(&self) -> f64 {
        f64::from_bits(self.reserve.load(Relaxed))
    }

    pub fn set_reserve(&self, r: f64) {
        self.reserve.store(r.to_bits(), Relaxed);
    }

    /// Background CPU share and priority, for status displays.
    pub fn reserve_state(&self) -> String {
        let reserve = self.reserve();
        let share = self.share_ppm.load(Relaxed) as f64 / 1e4;
        match self.step.load(Relaxed) {
            0 => format!("idle priority, {share:.2}% of CPU (reserve {:.2}%)", reserve * 100.0),
            s => format!("raised to nice {} to keep the {:.2}% reserve ({share:.2}% now)", NICE_STEPS[s - 1], reserve * 100.0),
        }
    }

    /// Why background work is paused, if it is.
    pub fn paused(&self) -> Option<String> {
        self.paused.load(Relaxed).then(|| self.reason.lock().unwrap_or_else(PoisonError::into_inner).clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_psi() {
        let dir = std::env::temp_dir().join(format!("fex-psi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("io.pressure");
        std::fs::write(&f, "some avg10=12.50 avg60=3.00 avg300=1.00 total=1\nfull avg10=1.00 avg60=0.00 avg300=0.00 total=1\n").unwrap();
        assert_eq!(super::psi_some(&f), Some(12.5));
        let _ = std::fs::remove_dir_all(dir);
    }
}
