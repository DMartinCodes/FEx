//! File clipboard in the same format Explorer uses (CF_HDROP + "Preferred
//! DropEffect"), so copy/cut/paste works between FEx and any other app.

use std::ptr::null_mut;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_HDROP;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::Shell::DragQueryFileW;

use crate::win;

const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;

fn drop_effect_format() -> u32 {
    let name: Vec<u16> = "Preferred DropEffect".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(name.as_ptr()) }
}

/// Holds the clipboard open; closes on drop. Retries briefly because other
/// apps (clipboard managers, RDP) often hold it for a few milliseconds.
struct Open;

impl Open {
    fn new() -> Option<Open> {
        for _ in 0..10 {
            if unsafe { OpenClipboard(GetActiveWindow()) } != 0 {
                return Some(Open);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

fn global_from(bytes: &[u8]) -> Option<HANDLE> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if h.is_null() {
            return None;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            GlobalFree(h);
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        Some(h)
    }
}

fn set_data(format: u32, bytes: &[u8]) -> bool {
    let Some(h) = global_from(bytes) else { return false };
    if unsafe { SetClipboardData(format, h) }.is_null() {
        unsafe { GlobalFree(h) };
        return false;
    }
    true // the clipboard owns the memory now
}

/// Put `paths` on the clipboard. Returns the clipboard sequence number so a
/// later cut-paste can tell whether the clipboard still holds our data.
pub fn set_files(paths: &[String], cut: bool) -> Result<u32, String> {
    // DROPFILES header: pFiles=20, pt=(0,0), fNC=0, fWide=1; then a
    // double-NUL-terminated UTF-16 list.
    let mut bytes: Vec<u8> = Vec::with_capacity(20 + paths.iter().map(|p| p.len() * 2 + 2).sum::<usize>() + 2);
    for v in [20u32, 0, 0, 0, 1] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    for p in paths {
        for u in p.encode_utf16().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };

    let _open = Open::new().ok_or("Clipboard is busy")?;
    unsafe { EmptyClipboard() };
    if !set_data(CF_HDROP as u32, &bytes) || !set_data(drop_effect_format(), &effect.to_le_bytes()) {
        return Err("Could not write to the clipboard".into());
    }
    drop(_open);
    Ok(sequence())
}

/// Files on the clipboard and whether they were cut (vs copied).
pub fn get_files() -> Option<(Vec<String>, bool)> {
    if unsafe { IsClipboardFormatAvailable(CF_HDROP as u32) } == 0 {
        return None;
    }
    let _open = Open::new()?;
    let hdrop = unsafe { GetClipboardData(CF_HDROP as u32) };
    if hdrop.is_null() {
        return None;
    }
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, null_mut(), 0) };
    let mut files = Vec::with_capacity(count as usize);
    let mut buf: Vec<u16> = Vec::new();
    for i in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, i, null_mut(), 0) } as usize;
        buf.resize(len + 1, 0);
        unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) };
        let mut s = String::with_capacity(len);
        win::push_utf16(&mut s, &buf[..len]);
        files.push(s);
    }
    let mut cut = false;
    let h = unsafe { GetClipboardData(drop_effect_format()) };
    if !h.is_null() {
        unsafe {
            let p = GlobalLock(h) as *const u32;
            // Another app wrote this; trust only as many bytes as it allocated.
            if !p.is_null() {
                if GlobalSize(h) >= 4 {
                    let effect = p.read_unaligned();
                    // Explorer's copy sets COPY|LINK, its cut sets MOVE only.
                    cut = effect & DROPEFFECT_MOVE != 0 && effect & DROPEFFECT_COPY == 0;
                }
                GlobalUnlock(h);
            }
        }
    }
    Some((files, cut))
}

/// Plain text on the clipboard (for text fields; see `FexApp::raw_input_hook`).
pub fn get_text() -> Option<String> {
    const CF_UNICODETEXT: u32 = 13;
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return None;
    }
    let _open = Open::new()?;
    let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if h.is_null() {
        return None;
    }
    unsafe {
        let p = GlobalLock(h) as *const u16;
        if p.is_null() {
            return None;
        }
        // Another app wrote this and may have left out the terminator: never
        // read past the allocation.
        let units = GlobalSize(h) / 2;
        let len = (0..units).take_while(|&i| *p.add(i) != 0).count();
        let mut s = String::with_capacity(len);
        win::push_utf16(&mut s, std::slice::from_raw_parts(p, len));
        GlobalUnlock(h);
        Some(s.replace("\r\n", "\n"))
    }
}

pub fn has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP as u32) != 0 }
}

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(test)]
mod tests {
    /// Overwrites the real clipboard, so it only runs on request:
    /// `cargo test -- --ignored clipboard_roundtrip`
    #[test]
    #[ignore]
    fn clipboard_roundtrip() {
        let paths = vec![r"C:\Windows\notepad.exe".to_string(), r"C:\Users\Public\ünïcode file.txt".to_string()];
        let seq = super::set_files(&paths, true).unwrap();
        assert!(super::has_files());
        let (files, cut) = super::get_files().unwrap();
        assert_eq!(files, paths);
        assert!(cut);
        super::set_files(&paths[..1], false).unwrap();
        assert_eq!(super::get_files().unwrap(), (paths[..1].to_vec(), false));
        super::clear_if_unchanged(seq); // stale sequence: must not clear
        assert!(super::has_files());
        super::clear_if_unchanged(super::sequence());
        assert!(!super::has_files());
    }
}

/// Empty the clipboard if it still holds the data from `seq` (after a
/// completed cut-paste, the cut files no longer exist at their old paths).
pub fn clear_if_unchanged(seq: u32) {
    if sequence() == seq {
        if let Some(_open) = Open::new() {
            unsafe { EmptyClipboard() };
        }
    }
}
