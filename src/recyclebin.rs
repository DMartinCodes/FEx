//! Restoring recycled items, for undo. Each volume keeps a per-user
//! `$Recycle.Bin\<SID>` folder where a deleted item is renamed to `$R<id>`
//! next to a small `$I<id>` record of where it came from. Restoring is the
//! reverse rename, which is what Explorer's "Restore" does too.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{fileop, fsys, win};

/// The current user's SID as a string (`S-1-5-21-...`).
fn user_sid() -> Option<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        // u64 storage keeps the TOKEN_USER header aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let mut out = String::new();
        win::push_utf16(&mut out, std::slice::from_raw_parts(s, len));
        LocalFree(s as _);
        Some(out)
    }
}

/// Decode a `$I` record: (deletion time as FILETIME, original path).
/// Version 1 (Vista–8.1) has a fixed MAX_PATH name; version 2 (Windows 10+)
/// stores the name length first.
fn parse_info(bytes: &[u8]) -> Option<(u64, String)> {
    let u64_at = |i: usize| Some(u64::from_le_bytes(bytes.get(i..i + 8)?.try_into().ok()?));
    let deleted = u64_at(16)?;
    let units: Vec<u16> = match u64_at(0)? {
        1 => bytes.get(24..24 + 520)?.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect(),
        2 => {
            let chars = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) as usize;
            bytes.get(28..28 + chars * 2)?.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect()
        }
        _ => return None,
    };
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    let mut path = String::new();
    win::push_utf16(&mut path, &units[..len]);
    Some((deleted, path))
}

/// Volume root for a path that may no longer exist.
fn root_of(path: &str) -> Option<String> {
    let mut p = path.to_string();
    loop {
        if fileop::stat(&p).is_some() {
            return fileop::volume_root(&p);
        }
        p = fsys::parent(&p)?;
    }
}

/// The newest recycled copy of `original`: ($I path, $R path).
fn find(original: &str, sid: &str) -> Option<(String, String)> {
    if original.starts_with(r"\\") {
        return None; // network deletes bypass the Recycle Bin
    }
    let bin = format!(r"{}$Recycle.Bin\{sid}", root_of(original)?);
    let mut best: Option<(u64, String)> = None;
    let mut name = String::new();
    let _ = win::scan_dir(&bin, |e| {
        name.clear();
        win::push_utf16(&mut name, e.name);
        if !name.starts_with("$I") || e.attrs & win::ATTR_DIR != 0 {
            return;
        }
        let info = fsys::join(&bin, &name);
        let Some((deleted, path)) = std::fs::read(&info).ok().and_then(|b| parse_info(&b)) else { return };
        if path.eq_ignore_ascii_case(original) && best.as_ref().is_none_or(|b| deleted > b.0) {
            best = Some((deleted, info));
        }
    });
    let info = best?.1;
    let (dir, file) = info.rsplit_once('\\')?;
    let data = fsys::join(dir, &format!("$R{}", &file[2..]));
    Some((info, data))
}

/// Put recycled items back where they were. Items that can't be restored
/// (not in the bin, or something else now has their name) are reported.
pub fn restore(paths: &[String]) -> Result<(), String> {
    let sid = user_sid().ok_or("Could not determine the current user")?;
    let mut errors = Vec::new();
    for original in paths {
        let Some((info, data)) = find(original, &sid) else {
            errors.push(format!("{original}: not found in the Recycle Bin"));
            continue;
        };
        if fileop::stat(original).is_some() {
            errors.push(format!("{original}: something with that name already exists"));
            continue;
        }
        if let Some(parent) = fsys::parent(original) {
            let _ = std::fs::create_dir_all(&parent);
        }
        match fileop::rename(&data, original, false) {
            Ok(()) => {
                let _ = fileop::delete_file(&info);
            }
            Err(e) => errors.push(format!("{original}: {}", win::error_text(e))),
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: u64, path: &str) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut b = Vec::new();
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&1234u64.to_le_bytes()); // size
        b.extend_from_slice(&0x01DA_0000_0000_0000u64.to_le_bytes()); // deletion time
        if version == 2 {
            b.extend_from_slice(&(units.len() as u32).to_le_bytes());
            units.iter().for_each(|u| b.extend_from_slice(&u.to_le_bytes()));
        } else {
            let mut fixed = units.clone();
            fixed.resize(260, 0);
            fixed.iter().for_each(|u| b.extend_from_slice(&u.to_le_bytes()));
        }
        b
    }

    #[test]
    fn parses_both_info_versions() {
        for version in [1, 2] {
            let (deleted, path) = parse_info(&record(version, r"C:\Users\me\ünïcode.txt")).unwrap();
            assert_eq!(deleted, 0x01DA_0000_0000_0000);
            assert_eq!(path, r"C:\Users\me\ünïcode.txt");
        }
        assert!(parse_info(&record(2, "x")[..20]).is_none());
        assert!(parse_info(&record(3, "x")).is_none());
    }

    #[test]
    fn finds_the_current_user() {
        assert!(user_sid().is_some_and(|s| s.starts_with("S-1-")));
    }
}
