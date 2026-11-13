//! Background work that yields to everything else (the Windows side of
//! `linux/throttle.rs`).
//!
//! * Background mode (`THREAD_MODE_BACKGROUND_BEGIN`) lowers a crawler
//!   thread's CPU, I/O and memory priority at once; Windows honours the I/O
//!   part on every storage stack, so no pressure monitoring is needed.
//! * Reserve: a controller measures the CPU time background threads get
//!   (their own thread times, so queries don't count) and, while it is below
//!   the reserve (`reserve.rs`), steps them out of background mode through
//!   LOWEST, BELOW_NORMAL and NORMAL priority; back down once they get enough.
//! * Power: on battery, background work pauses.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetThreadTimes, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
};

use crate::reserve;

/// Priorities above background mode, in the order the controller raises.
const STEPS: [THREAD_PRIORITY; 3] = [THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL];

thread_local! {
    static LAST_CPU: Cell<u64> = const { Cell::new(0) };
    static STEP: Cell<usize> = const { Cell::new(0) };
}

fn ft(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

/// This thread's CPU time in ns.
fn thread_cpu_ns() -> u64 {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    (ft(k) + ft(u)) * 100
}

fn set_step(from: usize, to: usize) {
    unsafe {
        let me = GetCurrentThread();
        if to == 0 {
            SetThreadPriority(me, THREAD_MODE_BACKGROUND_BEGIN);
        } else {
            if from == 0 {
                SetThreadPriority(me, THREAD_MODE_BACKGROUND_END);
            }
            SetThreadPriority(me, STEPS[to - 1]);
        }
    }
}

/// Put the calling thread in background mode (CPU, I/O and memory).
pub fn background_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    STEP.with(|s| s.set(0));
    LAST_CPU.with(|c| c.set(thread_cpu_ns()));
}

fn on_battery() -> bool {
    let mut st = SYSTEM_POWER_STATUS { ACLineStatus: 0, BatteryFlag: 0, BatteryLifePercent: 0, SystemStatusFlag: 0, BatteryLifeTime: 0, BatteryFullLifeTime: 0 };
    // ACLineStatus: 0 offline, 1 online, 255 unknown.
    unsafe { GetSystemPowerStatus(&mut st) != 0 && st.ACLineStatus == 0 }
}

pub struct Throttle {
    paused: AtomicBool,
    reserve: AtomicU64,
    cores: usize,
    used_ns: AtomicU64,
    last_pace: AtomicU64,
    step: AtomicUsize,
    share_ppm: AtomicU64,
    epoch: Instant,
}

impl Throttle {
    pub fn start(reserve: f64, cores: usize) -> Arc<Throttle> {
        let t = Arc::new(Throttle {
            paused: AtomicBool::new(false),
            reserve: AtomicU64::new(reserve.to_bits()),
            cores: cores.max(1),
            used_ns: AtomicU64::new(0),
            last_pace: AtomicU64::new(0),
            step: AtomicUsize::new(0),
            share_ppm: AtomicU64::new(0),
            epoch: Instant::now(),
        });
        let w = Arc::downgrade(&t);
        thread::Builder::new()
            .name("fex-reserve".into())
            .spawn(move || {
                let (mut used, mut at, mut ewma, mut ticks) = (0u64, Instant::now(), 0.0f64, 0u32);
                while let Some(t) = w.upgrade() {
                    let (u, now) = (t.used_ns.load(Relaxed), Instant::now());
                    let share = (u - used) as f64 / ((now - at).as_nanos().max(1) as f64 * t.cores as f64);
                    (used, at) = (u, now);
                    ewma = reserve::smooth(ewma, share);
                    t.share_ppm.store((ewma * 1e6) as u64, Relaxed);
                    let active = t.now_ns().saturating_sub(t.last_pace.load(Relaxed)) < 300_000_000;
                    let step = reserve::next_step(t.step.load(Relaxed), ewma, t.reserve(), active, STEPS.len());
                    t.step.store(step, Relaxed);
                    if ticks % 5 == 0 {
                        t.paused.store(on_battery(), Relaxed);
                    }
                    ticks = ticks.wrapping_add(1);
                    drop(t);
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .ok();
        t
    }

    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    pub fn reserve(&self) -> f64 {
        f64::from_bits(self.reserve.load(Relaxed))
    }

    pub fn set_reserve(&self, r: f64) {
        self.reserve.store(r.to_bits(), Relaxed);
    }

    /// Called by background threads before each folder: report CPU used,
    /// take on the controller's priority step, and block while paused.
    pub fn wait(&self) {
        let now = thread_cpu_ns();
        let last = LAST_CPU.with(|c| c.replace(now));
        if last != 0 {
            self.used_ns.fetch_add(now.saturating_sub(last), Relaxed);
        }
        self.last_pace.store(self.now_ns().max(1), Relaxed);
        let step = self.step.load(Relaxed);
        let was = STEP.with(|s| s.replace(step));
        if was != step {
            set_step(was, step);
        }
        while self.paused.load(Relaxed) {
            thread::sleep(Duration::from_millis(500));
            LAST_CPU.with(|c| c.set(thread_cpu_ns()));
        }
    }

    pub fn paused(&self) -> Option<String> {
        self.paused.load(Relaxed).then(|| "on battery".to_string())
    }

    pub fn reserve_state(&self) -> String {
        let share = self.share_ppm.load(Relaxed) as f64 / 1e4;
        let names = ["lowest", "below normal", "normal"];
        match self.step.load(Relaxed) {
            0 => format!("background mode, {share:.2}% of CPU (reserve {:.2}%)", self.reserve() * 100.0),
            s => format!("raised to {} priority to keep the {:.2}% reserve ({share:.2}% now)", names[s - 1], self.reserve() * 100.0),
        }
    }
}
