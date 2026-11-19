//! Removable drives the mount table alone doesn't show: the Linux side of
//! the Windows build's `volumes.rs`.
//!
//! The sidebar lists what is mounted (`sys::volumes`), which misses
//! removable drives in two cases:
//!
//! - **Not mounted.** With no desktop automounter (servers, minimal window
//!   managers, automount turned off), or after the drive was unmounted but
//!   not powered off, a plugged-in stick has no mount point. This is the
//!   Windows "no drive letter" case. [`unmounted`] finds these from sysfs
//!   and the udev database (both readable by any user, and never opening
//!   the device), and [`mount`] mounts one through udisks, as the desktop
//!   does: polkit lets the user mount their own removable drives.
//! - **Covered.** A removable volume's mount point has something else
//!   mounted on it or above it, so its files can't be reached there. This
//!   is the Windows "shadowed letter" case. [`covered`] reports these.
//!
//! Internal partitions (EFI, recovery, BIOS boot, swap, Microsoft
//! reserved, ones udev marks as ignored or hidden) are never listed.

use std::collections::HashSet;
use std::time::Duration;

use super::{fileop, fsys, sys};

/// A removable filesystem that is not mounted anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmounted {
    /// `/dev/sdb1`
    pub device: Vec<u8>,
    /// The drive holding it (`/dev/sdb`), for powering it off.
    pub disk: Vec<u8>,
    /// The filesystem label; may be empty.
    pub label: String,
    pub fstype: String,
    /// Bytes.
    pub size: u64,
}

impl Unmounted {
    /// A short name for the sidebar.
    pub fn name(&self) -> String {
        if self.label.is_empty() { format!("{} drive", fsys::format_size(self.size)) } else { self.label.clone() }
    }
}

/// A removable volume whose mount point another mount hides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Covered {
    /// Where it is mounted.
    pub point: Vec<u8>,
    /// Its device (`/dev/sdb1`).
    pub source: Vec<u8>,
    /// What hides it: the source and mount point of the mount on top.
    pub by: Vec<u8>,
    pub by_point: Vec<u8>,
}

/// Partition types that hold no user files (GPT type GUIDs, lower case, and
/// MBR type bytes as udev writes them).
const SYSTEM_PARTITIONS: &[&str] = &[
    "c12a7328-f81f-11d2-ba4b-00a0c93ec93b", // EFI system
    "21686148-6449-6e6f-744e-656564454649", // BIOS boot
    "de94bba4-06d1-4d40-a16a-bfd50179d6ac", // Windows recovery
    "e3c9e316-0b5c-4db8-817d-f92df00215ae", // Microsoft reserved
    "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", // Linux swap
    "0xef",                                 // EFI system
    "0x27",                                 // Windows recovery
    "0x82",                                 // Linux swap
    "0x12",                                 // OEM diagnostics / recovery
];

/// GPT attribute bits that mean "leave this alone": platform-required (0),
/// and for basic data partitions hidden (62) and no automount (63).
const GPT_SKIP_FLAGS: u64 = 1 | (1 << 62) | (1 << 63);

/// One device's properties from the udev database (`E:KEY=VALUE` lines).
struct Udev(Vec<(String, String)>);

impl Udev {
    fn read(major_minor: &str) -> Option<Udev> {
        let text = std::fs::read_to_string(format!("/run/udev/data/b{major_minor}")).ok()?;
        Some(Udev(parse_udev(&text)))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn parse_udev(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.strip_prefix("E:")?.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Undo udev's `\x20`-style escapes (in `ID_FS_LABEL_ENC`).
fn unescape_udev(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a device with these udev properties holds a filesystem meant for
/// user files.
fn is_data_filesystem(u: &Udev) -> bool {
    if u.get("ID_FS_USAGE") != Some("filesystem") || u.get("ID_FS_TYPE").is_none_or(|t| t.is_empty() || t == "swap") {
        return false;
    }
    if ["UDISKS_IGNORE", "UDISKS_SYSTEM"].iter().any(|k| u.get(k) == Some("1")) {
        return false;
    }
    if u.get("ID_PART_ENTRY_TYPE").is_some_and(|t| SYSTEM_PARTITIONS.contains(&t.to_ascii_lowercase().as_str())) {
        return false;
    }
    let flags = u.get("ID_PART_ENTRY_FLAGS").and_then(|f| u64::from_str_radix(f.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
    let gpt = u.get("ID_PART_ENTRY_SCHEME") == Some("gpt");
    !(gpt && flags & GPT_SKIP_FLAGS != 0)
}

/// Devices with a mounted filesystem, as `major:minor` and as kernel names
/// (`sdb1`). Both are needed: btrfs reports an anonymous device number in
/// mountinfo, and a source can be a `/dev/disk/by-*` or mapper link.
fn mounted_devices() -> (HashSet<String>, HashSet<String>) {
    let text = std::fs::read("/proc/self/mountinfo").unwrap_or_default();
    let (mut numbers, mut names) = (HashSet::new(), HashSet::new());
    for line in text.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        if let Some(n) = fields.get(2) {
            numbers.insert(String::from_utf8_lossy(n).into_owned());
        }
        if let Some(dash) = fields.iter().position(|f| *f == b"-")
            && let Some(source) = fields.get(dash + 2)
            && source.starts_with(b"/dev/")
        {
            let real = sys::real_path(source).unwrap_or_else(|| source.to_vec());
            names.insert(String::from_utf8_lossy(fsys::base_name(&real)).into_owned());
        }
    }
    // Active swap counts as in use too.
    for line in std::fs::read_to_string("/proc/swaps").unwrap_or_default().lines().skip(1) {
        if let Some(dev) = line.split_whitespace().next().filter(|d| d.starts_with("/dev/")) {
            let real = sys::real_path(dev.as_bytes()).unwrap_or_else(|| dev.as_bytes().to_vec());
            names.insert(String::from_utf8_lossy(fsys::base_name(&real)).into_owned());
        }
    }
    (numbers, names)
}

/// Removable filesystems that are plugged in but not mounted. Reads sysfs,
/// mountinfo and the udev database only; the devices themselves are never
/// opened. Empty where there is no udev database (containers).
pub fn unmounted() -> Vec<Unmounted> {
    let Ok(dir) = std::fs::read_dir("/sys/class/block") else { return Vec::new() };
    let (numbers, names) = mounted_devices();
    let devices: Vec<(String, std::path::PathBuf, std::path::PathBuf)> = dir
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| {
            let (dev, disk) = fileop::block_and_disk(&name)?;
            Some((name, dev, disk))
        })
        .collect();
    let mut out = Vec::new();
    for (name, dev, disk) in &devices {
        // A disk with partitions shows as its partitions.
        if dev == disk && devices.iter().any(|(_, d, k)| d != k && k == disk) {
            continue;
        }
        if names.contains(name) || !fileop::removable_block(name) {
            continue;
        }
        let Ok(number) = std::fs::read_to_string(dev.join("dev")) else { continue };
        let number = number.trim();
        if numbers.contains(number) {
            continue;
        }
        // In use by something else: LVM, RAID, an unlocked LUKS mapping.
        if std::fs::read_dir(dev.join("holders")).is_ok_and(|mut h| h.next().is_some()) {
            continue;
        }
        let size = std::fs::read_to_string(dev.join("size")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
        if size == 0 {
            continue; // a card reader or optical drive with no media
        }
        let Some(udev) = Udev::read(number) else { continue };
        if !is_data_filesystem(&udev) {
            continue;
        }
        let label = udev.get("ID_FS_LABEL_ENC").map(unescape_udev).or_else(|| udev.get("ID_FS_LABEL").map(String::from)).unwrap_or_default();
        let disk_name = disk.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
        out.push(Unmounted {
            device: format!("/dev/{name}").into_bytes(),
            disk: format!("/dev/{disk_name}").into_bytes(),
            label: label.trim().to_string(),
            fstype: udev.get("ID_FS_TYPE").unwrap_or_default().to_string(),
            size,
        });
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// Removable volumes in `mounts` (mountinfo order) that a later mount hides:
/// one on the same point, or on a folder above it.
pub fn covered(mounts: &[sys::Mount]) -> Vec<Covered> {
    let mut out = Vec::new();
    for (i, m) in mounts.iter().enumerate() {
        let Some(top) = mounts[i + 1..].iter().rev().find(|later| later.source != m.source && fsys::in_scope(&m.point, &later.point)) else { continue };
        if fileop::ejectable(m) {
            out.push(Covered { point: m.point.clone(), source: m.source.clone(), by: top.source.clone(), by_point: top.point.clone() });
        }
    }
    out
}

/// Where `device` is mounted now, if anywhere.
fn mount_point_of(device: &[u8]) -> Option<Vec<u8>> {
    let real = sys::real_path(device)?;
    sys::mounts().into_iter().rev().find(|m| m.source.starts_with(b"/dev/") && sys::real_path(&m.source).as_deref() == Some(&real[..])).map(|m| m.point)
}

fn valid_device(device: &[u8]) -> bool {
    device.strip_prefix(b"/dev/").is_some_and(|n| !n.is_empty() && n.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
}

/// Mount a removable filesystem as the desktop does (udisks, under
/// `/media` or `/run/media`), falling back to GIO where udisksctl isn't
/// installed. Returns where it was mounted. Blocks (polkit may ask for a
/// password); call off the UI thread.
pub fn mount(device: &[u8]) -> Result<Vec<u8>, String> {
    if !valid_device(device) {
        return Err("not a drive FEx can mount".into());
    }
    if let Some(point) = mount_point_of(device) {
        return Ok(point); // the desktop got there first
    }
    let dev = sys::os(device);
    let result = match fileop::run_tool("udisksctl", &["mount".as_ref(), "-b".as_ref(), dev], Duration::from_secs(120)) {
        Err(e) if e.starts_with("udisksctl:") => fileop::run_tool("gio", &["mount".as_ref(), "-d".as_ref(), dev], Duration::from_secs(60)),
        other => other,
    };
    match (result, mount_point_of(device)) {
        (_, Some(point)) => Ok(point),
        (Err(e), None) if e.contains("NotAuthorized") || e.contains("not authorized") => Err("you aren't allowed to mount it".into()),
        (Err(e), None) if e.starts_with("gio:") => Err("neither udisksctl nor gio is installed".into()),
        (Err(e), None) => Err(e),
        (Ok(()), None) => Err("it was mounted, but FEx can't find where".into()),
    }
}

/// Power down a removable drive that has nothing mounted (or detach a disk
/// image). Blocks; call off the UI thread.
pub fn power_off(disk: &[u8]) -> Result<(), String> {
    if !valid_device(disk) {
        return Err("not a drive FEx can power off".into());
    }
    let image = disk.starts_with(b"/dev/loop");
    let verb = if image { "loop-delete" } else { "power-off" };
    match fileop::run_tool("udisksctl", &[verb.as_ref(), "-b".as_ref(), sys::os(disk)], Duration::from_secs(30)) {
        Err(e) if e.starts_with("udisksctl:") => Err("udisksctl isn't installed (package udisks2)".into()),
        Err(e) if e.contains("busy") || e.contains("mounted") => Err("a filesystem on it is still mounted or in use".into()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udev(text: &str) -> Udev {
        Udev(parse_udev(text))
    }

    #[test]
    fn data_filesystems_only() {
        let stick = udev("S:disk/by-uuid/1234\nE:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=vfat\nE:ID_PART_ENTRY_SCHEME=dos\nE:ID_PART_ENTRY_TYPE=0xc\n");
        assert!(is_data_filesystem(&stick));
        let efi = udev("E:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=vfat\nE:ID_PART_ENTRY_SCHEME=gpt\nE:ID_PART_ENTRY_TYPE=C12A7328-F81F-11D2-BA4B-00A0C93EC93B\n");
        assert!(!is_data_filesystem(&efi));
        let swap = udev("E:ID_FS_USAGE=other\nE:ID_FS_TYPE=swap\n");
        assert!(!is_data_filesystem(&swap));
        let luks = udev("E:ID_FS_USAGE=crypto\nE:ID_FS_TYPE=crypto_LUKS\n");
        assert!(!is_data_filesystem(&luks));
        let ignored = udev("E:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=ext4\nE:UDISKS_IGNORE=1\n");
        assert!(!is_data_filesystem(&ignored));
        let hidden = udev("E:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=ntfs\nE:ID_PART_ENTRY_SCHEME=gpt\nE:ID_PART_ENTRY_FLAGS=0x4000000000000000\n");
        assert!(!is_data_filesystem(&hidden));
        // On MBR the flags are the boot flag, which says nothing about this.
        let bootable = udev("E:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=exfat\nE:ID_PART_ENTRY_SCHEME=dos\nE:ID_PART_ENTRY_FLAGS=0x80\n");
        assert!(is_data_filesystem(&bootable));
        let unpartitioned = udev("E:ID_FS_USAGE=filesystem\nE:ID_FS_TYPE=iso9660\n");
        assert!(is_data_filesystem(&unpartitioned));
    }

    #[test]
    fn labels() {
        assert_eq!(unescape_udev(r"My\x20Stick"), "My Stick");
        assert_eq!(unescape_udev(r"caf\xc3\xa9"), "café");
        assert_eq!(unescape_udev(r"a\x2"), r"a\x2");
        let u = Unmounted { device: b"/dev/sdb1".to_vec(), disk: b"/dev/sdb".to_vec(), label: String::new(), fstype: "vfat".into(), size: 16 << 30 };
        assert_eq!(u.name(), "16 GB drive");
    }

    #[test]
    fn device_names() {
        assert!(valid_device(b"/dev/sdb1"));
        assert!(valid_device(b"/dev/mmcblk0p1"));
        assert!(!valid_device(b"/dev/"));
        assert!(!valid_device(b"/dev/../etc/passwd"));
        assert!(!valid_device(b"/dev/sdb1 --flag"));
        assert!(!valid_device(b"sdb1"));
    }

    #[test]
    fn scans_this_machine_without_trouble() {
        // Whatever is plugged in here, nothing internal may be offered.
        for u in unmounted() {
            assert!(fileop::removable_block(&String::from_utf8_lossy(fsys::base_name(&u.device))));
            assert!(u.size > 0 && !u.fstype.is_empty());
        }
        for c in covered(&sys::mounts()) {
            assert!(c.source.starts_with(b"/dev/"));
        }
    }

    #[test]
    fn covered_needs_a_different_mount_on_or_above() {
        let m = |point: &str, source: &str| sys::Mount { point: point.into(), fstype: "ext4".into(), source: source.into() };
        // Not removable devices (no such sysfs entries), so nothing is reported,
        // but the scan must not trip over stacking, binds or nesting.
        let mounts = [m("/", "/dev/sda2"), m("/mnt", "/dev/sdz9"), m("/mnt", "/dev/sdz9"), m("/mnt/a", "/dev/sdy1"), m("/mnt", "tmpfs")];
        assert!(covered(&mounts).is_empty());
    }
}
