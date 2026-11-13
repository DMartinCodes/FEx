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
//! Queries never run on these threads: answering a user stays at normal
//! priority, because speed is the point.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// Lower the calling thread to idle CPU and I/O priority (per thread on
/// Linux, so the rest of the process is unaffected).
pub fn background_thread() {
    unsafe {
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0 as libc::c_long, IOPRIO_CLASS_IDLE << 13);
    }
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
}

impl Throttle {
    /// Start watching pressure and power every half second.
    pub fn start(io_limit: f64) -> Arc<Throttle> {
        let cgroup = own_cgroup();
        let t = Arc::new(Throttle { paused: AtomicBool::new(false), reason: Mutex::new(String::new()), io_limit, own_cgroup: cgroup.is_some() });
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

    /// Block while paused (called by background threads between folders).
    pub fn wait(&self) {
        while self.paused.load(Relaxed) {
            thread::sleep(Duration::from_millis(250));
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
