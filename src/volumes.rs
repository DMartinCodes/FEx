//! Drive letters as this session sees them, and the volumes behind them.
//!
//! `GetLogicalDrives` alone misrepresents removable drives in two common
//! cases:
//!
//! - **Shadowed letters.** The mount manager gives a new USB drive the first
//!   letter free in the *global* namespace. Mapped network drives live in
//!   the user's own logon session, so the mount manager can't see them and
//!   may hand out a letter a network drive already uses. In this session the
//!   mapping wins and the USB drive is unreachable by letter.
//! - **No letter.** The letter Windows remembers for the drive is taken, or
//!   automount is off, and the volume mounts without one.
//!
//! [`scan`] reads each letter's actual target (`QueryDosDevice`, which
//! never touches the network) and walks every volume, so both cases show up
//! in the sidebar with a way to fix them ([`assign_letter`]). [`watch`]
//! repeats the scan so drives appear and disappear without a refresh.

use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNamesForVolumeNameW, QueryDosDeviceW,
};
use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::fileop;
use crate::win::{self, wide};

/// One drive letter present in this session.
#[derive(Clone, Debug, PartialEq)]
pub struct Letter {
    /// `E:\`
    pub root: String,
    /// What the letter points to here (`\Device\HarddiskVolume5`, a network
    /// redirector path, `\??\C:\folder` for `subst`).
    pub target: String,
    /// `\\server\share` when the letter is a network drive.
    pub network: Option<String>,
    /// `\\?\Volume{guid}\` when the letter is a local volume.
    pub volume: Option<String>,
}

/// A volume with no drive letter that reaches it from this session.
#[derive(Clone, Debug, PartialEq)]
pub struct Hidden {
    /// `\\?\Volume{guid}\`
    pub volume: String,
    /// The volume label, if it has one.
    pub label: String,
    /// The letter Windows gave it, which something else holds in this
    /// session; `None` when it has no letter at all.
    pub letter: Option<char>,
    /// What holds that letter here (`\\server\share`, ...).
    pub taken_by: Option<String>,
}

impl Hidden {
    /// A short name for the sidebar and dialogs.
    pub fn name(&self) -> String {
        if self.label.is_empty() { "Drive".into() } else { self.label.clone() }
    }

    /// Why it isn't listed under a letter.
    pub fn reason(&self) -> String {
        match (self.letter, &self.taken_by) {
            (Some(l), Some(by)) => format!("Windows gave this drive {l}:, but {l}: is already {by} in your session"),
            (Some(l), None) => format!("Windows gave this drive {l}:, but {l}: points somewhere else in your session"),
            (None, _) => "Windows didn't give this drive a letter".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// In letter order.
    pub drives: Vec<Letter>,
    pub hidden: Vec<Hidden>,
}

impl Snapshot {
    pub fn letter(&self, root: &str) -> Option<&Letter> {
        self.drives.iter().find(|d| d.root.eq_ignore_ascii_case(root))
    }

    /// Letters every volume holds in the mount manager, visible or not.
    fn volume_letters(&self) -> impl Iterator<Item = char> + '_ {
        let visible = self.drives.iter().filter(|d| d.volume.is_some()).filter_map(|d| d.root.chars().next());
        visible.chain(self.hidden.iter().filter_map(|h| h.letter))
    }
}

/// Facts about a letterless volume that only change with the media, kept
/// between scans so devices aren't opened every time.
#[derive(Default)]
pub struct Cache {
    /// Labels of volumes with a taken letter.
    labels: HashMap<String, String>,
    /// Letterless volumes: the label if it belongs in the sidebar.
    letterless: HashMap<String, Option<String>>,
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let mut s = String::with_capacity(len);
    win::push_utf16(&mut s, &buf[..len]);
    s
}

/// The object-manager target of `E:` in this session.
fn dos_target(letter: char) -> Option<String> {
    let mut buf = [0u16; 1024];
    let n = unsafe { QueryDosDeviceW(wide(&format!("{letter}:"), "").as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (n != 0).then(|| from_wide(&buf))
}

/// `\\server\share` for a network redirector target such as
/// `\Device\LanmanRedirector\;Z:000000000001a2b3\server\share` or
/// `\Device\Mup\;LanmanRedirector\;Z:0000...\server\share`.
fn network_target(target: &str) -> Option<String> {
    let lower = target.to_ascii_lowercase();
    if !(lower.contains("redirector") || lower.starts_with(r"\device\mup")) {
        return None;
    }
    // The UNC part follows the last `;X:<logon id>` component.
    let after = &target[target.rfind(';')? + 1..];
    let unc = after.split_once('\\').map(|(_, rest)| rest).unwrap_or("");
    Some(if unc.is_empty() { target.to_string() } else { format!(r"\\{unc}") })
}

/// What a letter's target is, in words, for explaining a conflict.
fn describe(target: &str) -> String {
    if let Some(unc) = network_target(target) {
        format!("the network drive {unc}")
    } else if let Some(folder) = target.strip_prefix(r"\??\") {
        format!("a substitute for {folder}")
    } else {
        format!("used by {target}")
    }
}

/// The volume (`\\?\Volume{guid}\`) mounted at `root` in this session.
fn volume_at(root: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(wide(root, "").as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| from_wide(&buf))
}

/// Mount points the mount manager records for `volume`: drive letters
/// (`E:\`) and folders.
fn mount_points(volume: &str) -> Vec<String> {
    let name = wide(volume, "");
    let mut buf = vec![0u16; 512];
    loop {
        let mut needed = 0u32;
        let ok = unsafe { GetVolumePathNamesForVolumeNameW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut needed) };
        if ok != 0 {
            break;
        }
        if unsafe { GetLastError() } != 234 || needed as usize <= buf.len() {
            return Vec::new(); // ERROR_MORE_DATA is the only one worth retrying
        }
        buf.resize(needed as usize, 0);
    }
    buf.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(from_wide).collect()
}

fn drive_letter(path: &str) -> Option<char> {
    let b = path.as_bytes();
    (b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\').then(|| b[0].to_ascii_uppercase() as char)
}

fn volumes() -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = [0u16; 64];
    let h = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if h == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        out.push(from_wide(&buf));
        if unsafe { FindNextVolumeW(h, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(h) };
    out
}

/// The volume's label, or `None` when it has no readable filesystem (no
/// media, unformatted).
fn label(volume: &str) -> Option<String> {
    let mut name = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationW(wide(volume, "").as_ptr(), name.as_mut_ptr(), name.len() as u32, null_mut(), null_mut(), null_mut(), null_mut(), 0)
    };
    (ok != 0).then(|| from_wide(&name))
}

/// Read the drive letters and volumes as this session sees them. Talks to
/// the object manager and mount manager only; it never touches a network
/// drive, so a dead mapping can't stall it. Letterless volumes are opened
/// once each (and remembered in `cache`).
pub fn scan(cache: &mut Cache) -> Snapshot {
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i) as char;
        let root = format!("{letter}:\\");
        let target = dos_target(letter).unwrap_or_default();
        let network = network_target(&target);
        let volume = if network.is_none() { volume_at(&root) } else { None };
        drives.push(Letter { root, target, network, volume });
    }

    let mut hidden = Vec::new();
    let mut seen = Vec::new();
    for volume in volumes() {
        let points = mount_points(&volume);
        let reachable = drives.iter().any(|d| d.volume.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(&volume)));
        if reachable {
            continue;
        }
        let letters: Vec<char> = points.iter().filter_map(|p| drive_letter(p)).collect();
        if let Some(&letter) = letters.first() {
            // The mount manager says it has a letter, but the letter here is something else.
            seen.push(volume.clone());
            let taken_by = dos_target(letter).map(|t| describe(&t));
            let label = cache.labels.entry(volume.clone()).or_insert_with(|| label(&volume).unwrap_or_default()).clone();
            hidden.push(Hidden { volume, label, letter: Some(letter), taken_by });
        } else if points.is_empty() {
            // Mounted nowhere. Internal EFI and recovery partitions look
            // like this too, so only removable data volumes count.
            seen.push(volume.clone());
            let label = cache
                .letterless
                .entry(volume.clone())
                .or_insert_with(|| {
                    let facts = fileop::volume_facts(&volume);
                    if facts.removable && facts.data { label(&volume) } else { None }
                })
                .clone();
            if let Some(label) = label {
                hidden.push(Hidden { volume, label, letter: None, taken_by: None });
            }
        }
        // Mounted only in folders: reachable that way, as intended.
    }
    cache.labels.retain(|v, _| seen.contains(v));
    cache.letterless.retain(|v, _| seen.contains(v));
    hidden.sort_by(|a, b| a.letter.cmp(&b.letter).then_with(|| a.label.cmp(&b.label)));
    Snapshot { drives, hidden }
}

/// Re-scan in the background, calling `on_change` with each new snapshot
/// (the first one included) until it returns `false`.
pub fn watch(on_change: impl Fn(Snapshot) -> bool + Send + 'static) {
    let spawned = std::thread::Builder::new().name("drive-watch".into()).spawn(move || {
        let mut cache = Cache::default();
        let mut last: Option<Snapshot> = None;
        loop {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan(&mut cache))) {
                Ok(snapshot) if last.as_ref() != Some(&snapshot) => {
                    if !on_change(snapshot.clone()) {
                        return;
                    }
                    last = Some(snapshot);
                }
                Ok(_) => {}
                Err(_) => cache = Cache::default(),
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
    });
    drop(spawned);
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// Letters the mount manager remembers for volumes (attached or not), from
/// `HKLM\SYSTEM\MountedDevices`, leaving out `except`'s own. Best effort:
/// empty if the key can't be read.
pub fn reserved_letters(except: Option<&str>) -> Vec<char> {
    let path = wide(r"SYSTEM\MountedDevices", "");
    let mut key = null_mut();
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return Vec::new();
    }
    let key = Key(key);
    let mut letters: Vec<(char, Vec<u8>)> = Vec::new();
    let mut own: Option<Vec<u8>> = None;
    // `\??\Volume{guid}` names the same device data as its letter.
    let own_name = except.map(|v| v.trim_end_matches('\\').replacen(r"\\?\", r"\??\", 1).to_ascii_lowercase());
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut data = vec![0u8; 1024];
        let mut data_len = data.len() as u32;
        let rc = unsafe {
            RegEnumValueW(key.0, index, name.as_mut_ptr(), &mut name_len, null(), null_mut(), data.as_mut_ptr(), &mut data_len)
        };
        if rc == 259 {
            break; // ERROR_NO_MORE_ITEMS
        }
        if rc != 0 {
            continue; // an oversized value; not one of ours
        }
        data.truncate(data_len as usize);
        let name = from_wide(&name[..name_len as usize]);
        if let Some(rest) = name.strip_prefix(r"\DosDevices\")
            && let Some(letter) = drive_letter(&format!("{rest}\\"))
        {
            letters.push((letter, data));
        } else if own_name.as_deref() == Some(name.to_ascii_lowercase().as_str()) {
            own = Some(data);
        }
    }
    let mut out: Vec<char> = letters.into_iter().filter(|(_, d)| own.as_ref() != Some(d)).map(|(l, _)| l).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Letters free for `volume`: not present in this session, not held by any
/// volume, not remembered for a network drive (`remembered`), and not
/// reserved for another volume. In order, `D:` first.
pub fn free_letters(snapshot: &Snapshot, remembered: &[char], volume: &str) -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    let held: Vec<char> = snapshot.volume_letters().collect();
    let reserved = reserved_letters(Some(volume));
    let free = |c: &char| mask & (1 << (*c as u8 - b'A')) == 0 && !held.contains(c) && !remembered.contains(c);
    let mut letters: Vec<char> = ('D'..='Z').filter(free).filter(|c| !reserved.contains(c)).collect();
    if letters.is_empty() {
        // Every letter has been reserved by some past drive; reservations
        // for absent drives are only a preference.
        letters = ('D'..='Z').filter(free).collect();
    }
    letters
}

fn valid_volume(volume: &str) -> bool {
    let Some(guid) = volume.strip_prefix(r"\\?\Volume{").and_then(|v| v.strip_suffix(r"}\")) else { return false };
    guid.len() == 36 && guid.chars().enumerate().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Give `volume` the drive letter `letter`, replacing `old` (its current
/// letter, if any). Uses Windows' own `mountvol`, run through the UAC
/// prompt, so FEx never asks to elevate itself. Windows remembers the new
/// letter for the next time the drive is plugged in. Blocks; call off the
/// UI thread.
pub fn assign_letter(volume: &str, old: Option<char>, letter: char) -> Result<(), String> {
    if !valid_volume(volume) || !letter.is_ascii_uppercase() || old.is_some_and(|o| !o.is_ascii_uppercase()) {
        return Err("Not a drive FEx can change".into());
    }
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let cmd = format!(r"{system}\System32\cmd.exe");
    let mountvol = format!(r#""{system}\System32\mountvol.exe""#);
    // A volume has one drive letter, so the old one goes first; if the new
    // one can't be set, the old one is put back.
    let script = match old {
        Some(o) => format!(
            "{mountvol} {o}: /D && ({mountvol} {letter}: {volume} || ({mountvol} {o}: {volume} & exit /b 1))"
        ),
        None => format!("{mountvol} {letter}: {volume}"),
    };
    let params = format!(r#"/d /s /c "{script}""#);
    let (verb, file, params) = (wide("runas", ""), wide(&cmd, ""), wide(&params, ""));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = unsafe { GetLastError() };
        return Err(if e == 1223 { "Cancelled".into() } else { win::error_text(e) });
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Windows couldn't give the drive {letter}: (mountvol exit code {code})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_targets() {
        assert_eq!(
            network_target(r"\Device\LanmanRedirector\;Z:000000000001a2b3\server\share").as_deref(),
            Some(r"\\server\share")
        );
        assert_eq!(
            network_target(r"\Device\Mup\;LanmanRedirector\;E:0000000000abcdef\srv\data\sub").as_deref(),
            Some(r"\\srv\data\sub")
        );
        assert_eq!(network_target(r"\Device\HarddiskVolume5"), None);
        assert_eq!(network_target(r"\??\C:\work"), None);
    }

    #[test]
    fn volume_names() {
        assert!(valid_volume(r"\\?\Volume{0b1d2c3e-aaaa-4bbb-8ccc-0123456789ab}\"));
        assert!(!valid_volume(r"\\?\Volume{0b1d2c3e-aaaa-4bbb-8ccc-0123456789ab}"));
        assert!(!valid_volume(r"\\?\Volume{0b1d2c3e-aaaa-4bbb-8ccc-0123456789ab}\ & calc"));
        assert!(!valid_volume(r"\\?\Volume{0b1d2c3e&aaaa-4bbb-8ccc-0123456789ab}\"));
    }

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter(r"e:\"), Some('E'));
        assert_eq!(drive_letter(r"E:\mnt\"), None);
        assert_eq!(drive_letter(r"E:"), None);
    }
}
