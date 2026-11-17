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
//! * Memory (`memory.rs`): available physical memory and commit, plus the
//!   OS's low-memory notification, set a level; background work pauses
//!   while it isn't normal, and the server sheds (heap, working set, the
//!   descriptor cache). The whole process runs at below-normal memory
//!   priority, so its pages are trimmed before other programs'.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::System::Memory::{CreateMemoryResourceNotification, LowMemoryResourceNotification, QueryMemoryResourceNotification};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetThreadTimes, ProcessMemoryPriority, SetProcessInformation, SetThreadPriority, MEMORY_PRIORITY_BELOW_NORMAL,
    MEMORY_PRIORITY_INFORMATION, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY, THREAD_PRIORITY_BELOW_NORMAL,
    THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
};

use crate::memory::{self, Level};
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

/// Give the whole process below-normal memory priority: under pressure the
/// memory manager trims its pages (and repurposes its standby pages) before
/// those of normal programs. Queries are unaffected while memory is ample.
pub fn lower_memory_priority() {
    let info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY_BELOW_NORMAL };
    unsafe { SetProcessInformation(GetCurrentProcess(), ProcessMemoryPriority, (&raw const info).cast(), size_of::<MEMORY_PRIORITY_INFORMATION>() as u32) };
}

/// The OS's low-memory signal (a handle that lives as long as the process;
/// 0 if it couldn't be made).
fn low_memory_handle() -> usize {
    unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) as usize }
}

/// One look at memory. Available is the smaller of the physical memory and
/// the commit charge left, as fractions: running out of either hurts other
/// programs (paging, or allocations failing). Windows has no PSI, so the
/// OS's own low-memory signal stands in for stalls.
fn memory_reading(low_handle: usize) -> Option<memory::Reading> {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 || m.ullTotalPhys == 0 {
        return None;
    }
    let phys = m.ullAvailPhys as f64 / m.ullTotalPhys as f64;
    let commit = if m.ullTotalPageFile > 0 { m.ullAvailPageFile as f64 / m.ullTotalPageFile as f64 } else { 1.0 };
    let mut state = 0;
    let low = low_handle != 0 && unsafe { QueryMemoryResourceNotification(low_handle as HANDLE, &mut state) } != 0 && state != 0;
    Some(memory::Reading { available: phys.min(commit), stall: 0.0, low })
}

pub struct Throttle {
    /// Background work waits: on battery, or memory isn't normal.
    paused: AtomicBool,
    battery: AtomicBool,
    mem_level: AtomicU8,
    mem_available_ppm: AtomicU64,
    memory_floor: f64,
    /// Shutting down: nothing waits for a pause to end any more.
    stopping: AtomicBool,
    reserve: AtomicU64,
    cores: usize,
    used_ns: AtomicU64,
    last_pace: AtomicU64,
    step: AtomicUsize,
    share_ppm: AtomicU64,
    epoch: Instant,
}

impl Throttle {
    /// The state alone, with no monitor thread (`start` adds it).
    fn new(reserve: f64, cores: usize, memory_floor: f64) -> Throttle {
        Throttle {
            paused: AtomicBool::new(false),
            battery: AtomicBool::new(false),
            mem_level: AtomicU8::new(Level::Normal as u8),
            mem_available_ppm: AtomicU64::new(1_000_000),
            memory_floor,
            stopping: AtomicBool::new(false),
            reserve: AtomicU64::new(reserve.to_bits()),
            cores: cores.max(1),
            used_ns: AtomicU64::new(0),
            last_pace: AtomicU64::new(0),
            step: AtomicUsize::new(0),
            share_ppm: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    pub fn start(reserve: f64, cores: usize, memory_floor: f64) -> Arc<Throttle> {
        let t = Arc::new(Throttle::new(reserve, cores, memory_floor));
        let w = Arc::downgrade(&t);
        let low = low_memory_handle();
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
                        t.battery.store(on_battery(), Relaxed);
                        if let Some(r) = memory_reading(low) {
                            t.observe_memory(&r);
                        }
                        t.paused.store(t.battery.load(Relaxed) || t.memory_level() != Level::Normal, Relaxed);
                    }
                    ticks = ticks.wrapping_add(1);
                    drop(t);
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .ok();
        t
    }

    fn observe_memory(&self, r: &memory::Reading) {
        let level = memory::level(self.memory_level(), r, self.memory_floor);
        self.mem_level.store(level as u8, Relaxed);
        self.mem_available_ppm.store((r.available * 1e6) as u64, Relaxed);
    }

    pub fn memory_level(&self) -> Level {
        Level::from_u8(self.mem_level.load(Relaxed))
    }

    /// Available memory (the smaller of physical and commit), 0..1.
    pub fn memory_available(&self) -> f64 {
        self.mem_available_ppm.load(Relaxed) as f64 / 1e6
    }

    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Release paused threads for good, so they reach their cancel checks
    /// (called when the service stops).
    pub fn stop(&self) {
        self.stopping.store(true, Relaxed);
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
        while self.paused.load(Relaxed) && !self.stopping.load(Relaxed) {
            thread::sleep(Duration::from_millis(500));
            LAST_CPU.with(|c| c.set(thread_cpu_ns()));
        }
    }

    pub fn paused(&self) -> Option<String> {
        if self.battery.load(Relaxed) {
            Some("on battery".to_string())
        } else if self.memory_level() != Level::Normal {
            Some(format!("memory {} ({:.0}% available)", self.memory_level().name(), self.memory_available() * 100.0))
        } else {
            None
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_releases_paused_threads() {
        let t = Arc::new(Throttle::new(0.0, 1, memory::DEFAULT_FLOOR));
        t.paused.store(true, Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        let t2 = t.clone();
        thread::spawn(move || {
            t2.wait();
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err(), "a paused wait must block");
        t.stop();
        assert!(rx.recv_timeout(Duration::from_secs(3)).is_ok(), "stop must release it");
    }

    #[test]
    fn memory_reading_is_sane() {
        let r = memory_reading(low_memory_handle()).expect("GlobalMemoryStatusEx");
        assert!(r.available > 0.0 && r.available <= 1.0, "{r:?}");
        let t = Throttle::new(0.0, 1, 0.99);
        t.observe_memory(&r);
        assert_ne!(t.memory_level(), Level::Normal, "a 99% floor must read as tight");
        assert!(t.paused().is_some_and(|p| p.starts_with("memory")));
    }
}
