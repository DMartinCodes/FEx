//! What the machine is like, measured at install (`fex service profile`)
//! or on the service's first start: the Windows side of `linux/profile.rs`.
//! File counts come from each NTFS volume's file table size (no crawl);
//! the first crawl replaces the estimate with the real count.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::reserve::reserve_for;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub cores: usize,
    pub memory_mb: u64,
    pub battery: bool,
    pub files: u64,
    pub entry_cost_ns: u64,
    pub reserve: f64,
}

impl Profile {
    pub fn with_files(&self, files: u64) -> Profile {
        Profile { files, reserve: reserve_for(self.cores, files, self.battery), ..self.clone() }
    }

    pub fn full_load_crawl(&self) -> Duration {
        let cpu = self.files as f64 * self.entry_cost_ns as f64 / 1e9;
        Duration::from_secs_f64(cpu / (self.reserve * self.cores as f64).max(1e-9))
    }

    pub fn describe(&self) -> String {
        format!(
            "{} cores, {} MB RAM{}; {} files ({} per core); {} ns CPU per entry; \
             reserve under full load: {:.2}% of total CPU (a full crawl would take {:.0} s on the reserve alone)",
            self.cores,
            self.memory_mb,
            if self.battery { ", battery" } else { "" },
            self.files,
            self.files / self.cores.max(1) as u64,
            self.entry_cost_ns,
            self.reserve * 100.0,
            self.full_load_crawl().as_secs_f64()
        )
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = format!(
            "# FEx index service profile. Edit `reserve` to override.\r\ncores={}\r\nmemory_mb={}\r\nbattery={}\r\nfiles={}\r\nentry_cost_ns={}\r\nreserve={}\r\n",
            self.cores, self.memory_mb, self.battery, self.files, self.entry_cost_ns, self.reserve
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
            files: get("files")?.parse().ok()?,
            entry_cost_ns: get("entry_cost_ns")?.parse().ok()?,
            reserve: get("reserve")?.parse::<f64>().ok()?.clamp(0.0, 0.5),
        })
    }
}

fn memory_mb() -> u64 {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 { m.ullTotalPhys / (1024 * 1024) } else { 0 }
}

fn has_battery() -> bool {
    let mut st = SYSTEM_POWER_STATUS { ACLineStatus: 0, BatteryFlag: 0, BatteryLifePercent: 0, SystemStatusFlag: 0, BatteryLifeTime: 0, BatteryFullLifeTime: 0 };
    // BatteryFlag 128: no system battery; 255: unknown.
    unsafe { GetSystemPowerStatus(&mut st) != 0 && st.BatteryFlag != 128 && st.BatteryFlag != 255 }
}

/// File records in use on the NTFS volume at `root` (an upper bound on its
/// files and folders), from the size of its file table.
fn ntfs_records(root: &str) -> Option<u64> {
    let letter = root.chars().next()?;
    let name: Vec<u16> = format!(r"\\.\{letter}:").encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(h, FSCTL_GET_NTFS_VOLUME_DATA, std::ptr::null(), 0, &mut data as *mut _ as *mut _, size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32, &mut got, std::ptr::null_mut())
    };
    unsafe { CloseHandle(h) };
    (ok != 0 && data.BytesPerFileRecordSegment > 0).then(|| data.MftValidDataLength as u64 / data.BytesPerFileRecordSegment as u64)
}

fn process_cpu_ns() -> u64 {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let ft = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    (ft(k) + ft(u)) * 100
}

/// CPU time per entry for a crawl of up to ~2 s of `sample`.
fn entry_cost_ns(sample: &str) -> u64 {
    let build = crate::index::Build::new(sample);
    let b = build.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        b.cancel();
    });
    let before = process_cpu_ns();
    let _ = build.run(&|_, _| {});
    let cpu = process_cpu_ns().saturating_sub(before);
    let entries = build.entries.load(std::sync::atomic::Ordering::Relaxed).max(1) as u64;
    (cpu / entries).clamp(200, 100_000)
}

/// Measure this machine for indexing `roots`.
pub fn measure(roots: &[String], known_files: Option<u64>) -> Profile {
    let cores = crate::walk::cores();
    let battery = has_battery();
    let files = known_files
        .or_else(|| roots.iter().map(|r| ntfs_records(r)).sum::<Option<u64>>())
        .unwrap_or(cores as u64 * 100_000);
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let sample = Arc::<str>::from(format!(r"{windir}\System32"));
    Profile { cores, memory_mb: memory_mb(), battery, files, entry_cost_ns: entry_cost_ns(&sample), reserve: reserve_for(cores, files, battery) }
}
