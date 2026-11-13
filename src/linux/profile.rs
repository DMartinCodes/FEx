//! What the machine is like, measured once at install (`fex profile`) or on
//! the daemon's first start, and how much CPU the index may claim when the
//! machine is fully loaded.
//!
//! Idle priority alone means a machine that is always busy never finishes
//! indexing. So the daemon reserves a small share of total CPU capacity for
//! background work under full load (it still takes everything that's idle):
//! * 0.5% when there are few files per core (a compute server: many cores,
//!   a modest tree, the index is cheap to keep up);
//! * up to 5% when there are many files per core (a laptop or workstation
//!   holding millions of files on a few cores);
//! * scaled on a log scale between 10K and 1M files per core, and a quarter
//!   more on battery-capable machines (capped at 5%), where a full crawl
//!   competes with less hardware.
//!
//! File counts come from the filesystems' own inode counts (`statvfs`), so
//! profiling doesn't crawl; filesystems that don't report them (Btrfs) are
//! counted by the first crawl, after which the reserve is recomputed.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;

use super::index::{self, Crawl};
use super::{fsys, sys, walk};

pub const MIN_RESERVE: f64 = 0.005;
pub const MAX_RESERVE: f64 = 0.05;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub cores: usize,
    pub memory_mb: u64,
    /// Has a battery (a laptop or similar).
    pub battery: bool,
    /// The root filesystem is on a rotational disk.
    pub rotational: bool,
    /// Files and folders on the indexed filesystems (inodes in use).
    pub files: u64,
    /// CPU time per indexed entry (crawl, ns), measured on a sample.
    pub entry_cost_ns: u64,
    /// Share of total CPU capacity reserved under full load (0.005 = 0.5%).
    pub reserve: f64,
}

/// The reserve for a machine with `cores` cores and `files` files.
pub fn reserve_for(cores: usize, files: u64, battery: bool) -> f64 {
    let per_core = files as f64 / cores.max(1) as f64;
    let t = ((per_core.max(1.0).log10() - 4.0) / 2.0).clamp(0.0, 1.0);
    let r = MIN_RESERVE + t * (MAX_RESERVE - MIN_RESERVE);
    if battery { (r * 1.25).min(MAX_RESERVE) } else { r }
}

impl Profile {
    /// Time a full crawl would take with only the reserve to run on.
    pub fn full_load_crawl(&self) -> Duration {
        let cpu = self.files as f64 * self.entry_cost_ns as f64 / 1e9;
        Duration::from_secs_f64(cpu / (self.reserve * self.cores as f64).max(1e-9))
    }

    /// Recompute the reserve after learning the real number of files.
    pub fn with_files(&self, files: u64) -> Profile {
        Profile { files, reserve: reserve_for(self.cores, files, self.battery), ..self.clone() }
    }

    pub fn describe(&self) -> String {
        let per_core = self.files / self.cores.max(1) as u64;
        format!(
            "{} cores, {} MB RAM{}{}; {} files ({} per core); {} ns CPU per entry\n\
             reserve under full load: {:.2}% of total CPU (a full crawl would take {:.0} s on the reserve alone)",
            self.cores,
            self.memory_mb,
            if self.battery { ", battery" } else { "" },
            if self.rotational { ", rotational root disk" } else { "" },
            self.files,
            per_core,
            self.entry_cost_ns,
            self.reserve * 100.0,
            self.full_load_crawl().as_secs_f64()
        )
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = format!(
            "# FEx index daemon profile (fex profile). Edit `reserve` to override.\n\
             cores={}\nmemory_mb={}\nbattery={}\nrotational={}\nfiles={}\nentry_cost_ns={}\nreserve={}\n",
            self.cores, self.memory_mb, self.battery, self.rotational, self.files, self.entry_cost_ns, self.reserve
        );
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> Option<Profile> {
        let text = std::fs::read_to_string(path).ok()?;
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(str::trim);
        Some(Profile {
            cores: get("cores")?.parse().ok()?,
            memory_mb: get("memory_mb")?.parse().ok()?,
            battery: get("battery")? == "true",
            rotational: get("rotational")? == "true",
            files: get("files")?.parse().ok()?,
            entry_cost_ns: get("entry_cost_ns")?.parse().ok()?,
            reserve: get("reserve")?.parse::<f64>().ok()?.clamp(0.0, 0.5),
        })
    }
}

fn memory_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse::<u64>().ok()))
        .map_or(0, |kb| kb / 1024)
}

fn has_battery() -> bool {
    std::fs::read_dir("/sys/class/power_supply")
        .map(|d| d.flatten().any(|e| std::fs::read_to_string(e.path().join("type")).is_ok_and(|t| t.trim() == "Battery")))
        .unwrap_or(false)
}

/// Whether the block device holding `path` is rotational.
fn rotational(path: &[u8]) -> bool {
    let Ok(c) = sys::cstr(path) else { return false };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let (major, minor) = (libc::major(st.st_dev), libc::minor(st.st_dev));
    // A partition's queue settings live on its parent disk.
    for p in [format!("/sys/dev/block/{major}:{minor}/queue/rotational"), format!("/sys/dev/block/{major}:{minor}/../queue/rotational")] {
        if let Ok(v) = std::fs::read_to_string(p) {
            return v.trim() == "1";
        }
    }
    false
}

/// Inodes in use across the filesystems a crawl of `root` would enter.
/// `None` if one of them doesn't report inode counts.
fn files_in_use(root: &[u8], skip: &HashSet<Vec<u8>>) -> Option<u64> {
    let mut seen_devs = HashSet::new();
    let mut total = 0u64;
    let mut points: Vec<Vec<u8>> = vec![root.to_vec()];
    points.extend(sys::mounts().into_iter().filter(|m| fsys::in_scope(&m.point, root) && !skip.contains(&m.point)).map(|m| m.point));
    for p in points {
        let c = sys::cstr(&p).ok()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 || !seen_devs.insert(st.st_dev) {
            continue;
        }
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
            continue;
        }
        if vfs.f_files == 0 {
            return None; // dynamic inodes (Btrfs): unknown until crawled
        }
        total += vfs.f_files.saturating_sub(vfs.f_ffree);
    }
    Some(total)
}

/// CPU time per entry for a crawl, measured on up to ~2 s of `sample`.
fn entry_cost_ns(sample: &[u8], skip: &HashSet<Vec<u8>>) -> u64 {
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        c2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let seen = AtomicUsize::new(0);
    let before = process_cpu_ns();
    let c = Crawl { workers: 1, cancel: &cancel, skip, with_meta: false, seen: &seen, init: &|| {}, pace: &|| {} };
    let root: Arc<[u8]> = sample.into();
    let (shards, _) = index::crawl(std::slice::from_ref(&root), &c);
    let cpu = process_cpu_ns().saturating_sub(before);
    let entries = seen.load(std::sync::atomic::Ordering::Relaxed).max(1) as u64;
    drop(shards);
    // Partial batches aren't counted in `seen`; this errs on the high side.
    (cpu / entries).clamp(200, 100_000)
}

pub fn process_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Measure this machine for indexing `root`.
pub fn measure(root: &[u8], skip: &HashSet<Vec<u8>>, known_files: Option<u64>) -> Profile {
    let cores = walk::cores();
    let battery = has_battery();
    let files = known_files.or_else(|| files_in_use(root, skip)).unwrap_or(cores as u64 * 100_000);
    let sample: &[u8] = if sys::is_dir(b"/usr") && fsys::in_scope(b"/usr", root) { b"/usr" } else { root };
    Profile {
        cores,
        memory_mb: memory_mb(),
        battery,
        rotational: rotational(root),
        files,
        entry_cost_ns: entry_cost_ns(sample, skip),
        reserve: reserve_for(cores, files, battery),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_scales_with_files_per_core() {
        // A 64-core compute server with half a million files: the floor.
        assert_eq!(reserve_for(64, 500_000, false), MIN_RESERVE);
        // An 8-core laptop with 2M files: near the ceiling.
        let laptop = reserve_for(8, 2_000_000, true);
        assert!(laptop > 0.04 && laptop <= MAX_RESERVE, "{laptop}");
        // 1M files per core and beyond: the ceiling.
        assert_eq!(reserve_for(2, 5_000_000, false), MAX_RESERVE);
        // Monotonic in files.
        assert!(reserve_for(16, 4_000_000, false) > reserve_for(16, 400_000, false));
    }

    #[test]
    fn round_trips() {
        let p = Profile { cores: 18, memory_mb: 65536, battery: false, rotational: false, files: 3_000_000, entry_cost_ns: 900, reserve: 0.012 };
        let path = std::env::temp_dir().join(format!("fex-profile-{}", std::process::id()));
        p.save(&path).unwrap();
        assert_eq!(Profile::load(&path), Some(p));
        let _ = std::fs::remove_file(path);
    }
}
