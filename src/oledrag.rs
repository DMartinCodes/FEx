//! Starting OLE drags out of FEx.
//!
//! Files travel as a shell data object, the same kind Explorer creates, so
//! every drop target understands them: Explorer (including its optimized
//! moves), the desktop, mail clients, and other FEx windows. Tabs travel
//! between FEx windows as a private clipboard format holding the location.
//!
//! The drag runs a modal loop on the UI thread until the drop, as in Explorer.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::core::{BOOL, GUID, HRESULT};
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{ILFree, SHCreateDataObject, SHDoDragDrop, SHParseDisplayName};

use crate::win;

const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
const DRAGDROP_S_DROP: HRESULT = 0x0004_0100;

/// Clipboard format name for a dragged tab: `"<pid>\n<location>"` in UTF-16.
const TAB_FORMAT: &str = "FEx.Tab";

#[repr(C)]
struct DataObjectVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_data: usize,
    get_data_here: usize,
    query_get_data: usize,
    get_canonical_format_etc: usize,
    set_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC, *const STGMEDIUM, BOOL) -> HRESULT,
}

/// An owned IDataObject reference.
struct DataObject(*mut c_void);

impl DataObject {
    fn vtbl(&self) -> &DataObjectVtbl {
        unsafe { &**(self.0 as *const *const DataObjectVtbl) }
    }

    /// Create a shell data object for `pidls` (absolute, i.e. relative to the desktop).
    fn create(pidls: &[*mut ITEMIDLIST]) -> Result<DataObject, String> {
        let mut obj = null_mut();
        let hr = unsafe {
            SHCreateDataObject(null(), pidls.len() as u32, pidls.as_ptr() as *const *const ITEMIDLIST, null_mut(), &IID_IDATAOBJECT, &mut obj)
        };
        if hr < 0 || obj.is_null() {
            return Err(format!("Could not start the drag (0x{hr:08x})"));
        }
        Ok(DataObject(obj))
    }

    /// Attach `bytes` under clipboard format `cf`; the object takes ownership.
    fn set_bytes(&self, cf: u16, bytes: &[u8]) -> Result<(), String> {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                return Err("Out of memory".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            GlobalUnlock(h);
            let format = FORMATETC { cfFormat: cf, ptd: null_mut(), dwAspect: DVASPECT_CONTENT, lindex: -1, tymed: TYMED_HGLOBAL as u32 };
            let medium = STGMEDIUM { tymed: TYMED_HGLOBAL as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: null_mut() };
            let hr = (self.vtbl().set_data)(self.0, &format, &medium, 1);
            if hr < 0 {
                GlobalFree(h);
                return Err(format!("Could not start the drag (0x{hr:08x})"));
            }
        }
        Ok(())
    }

    /// Run the drag; the effect the drop target reported (NONE when cancelled).
    fn drag(&self, hwnd: HWND, allowed: u32) -> u32 {
        let mut effect = DROPEFFECT_NONE;
        let hr = unsafe { SHDoDragDrop(hwnd, self.0, null_mut(), allowed, &mut effect) };
        if hr == DRAGDROP_S_DROP { effect } else { DROPEFFECT_NONE }
    }
}

impl Drop for DataObject {
    fn drop(&mut self) {
        unsafe { (self.vtbl().release)(self.0) };
    }
}

pub fn tab_format() -> u16 {
    let name = win::wide(TAB_FORMAT, "");
    unsafe { RegisterClipboardFormatW(name.as_ptr()) as u16 }
}

/// Decode a dragged tab: (sending process id, location).
pub fn parse_tab(bytes: &[u8]) -> Option<(u32, String)> {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    let mut text = String::new();
    win::push_utf16(&mut text, &units[..len]);
    let (pid, location) = text.split_once('\n')?;
    Some((pid.parse().ok()?, location.to_string())).filter(|(_, l)| !l.is_empty())
}

fn encode_tab(location: &str) -> Vec<u8> {
    format!("{}\n{location}", std::process::id()).encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

/// Drag files out of FEx. Returns the effect the target performed. For a
/// move, the target moves the files itself (as Explorer expects), so the
/// caller has nothing more to do either way.
pub fn drag_files(hwnd: HWND, paths: &[String]) -> Result<u32, String> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    let mut failed = None;
    for path in paths {
        let mut pidl = null_mut();
        let hr = unsafe { SHParseDisplayName(win::wide(path, "").as_ptr(), null_mut(), &mut pidl, 0, null_mut()) };
        if hr < 0 || pidl.is_null() {
            failed = Some(format!("{path}: cannot be dragged (0x{hr:08x})"));
            break;
        }
        pidls.push(pidl);
    }
    let result = match failed {
        Some(e) => Err(e),
        None => DataObject::create(&pidls).map(|obj| obj.drag(hwnd, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK)),
    };
    for pidl in pidls {
        unsafe { ILFree(pidl) };
    }
    result
}

pub enum TabDrop {
    /// Another FEx window opened it.
    Taken,
    /// Released where nothing took it (the desktop, another app).
    Released,
    /// Esc was pressed.
    Cancelled,
}

/// Drag a tab out of the window, towards another FEx window.
pub fn drag_tab(hwnd: HWND, location: &str) -> Result<TabDrop, String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    let obj = DataObject::create(&[])?;
    obj.set_bytes(tab_format(), &encode_tab(location))?;
    if obj.drag(hwnd, DROPEFFECT_MOVE) & DROPEFFECT_MOVE != 0 {
        return Ok(TabDrop::Taken);
    }
    // Windows reports a release over nothing the same way as Esc; the key tells them apart.
    Ok(if unsafe { GetAsyncKeyState(VK_ESCAPE as i32) } < 0 { TabDrop::Cancelled } else { TabDrop::Released })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_payload_round_trips() {
        let (pid, location) = parse_tab(&encode_tab(r"zip:C:\a b.zip|x/y")).unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(location, r"zip:C:\a b.zip|x/y");
        assert_eq!(parse_tab(&[]), None);
    }
}
