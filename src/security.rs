//! Read-only view of a file's owner and access control list. Editing goes
//! through Windows' own Security dialog (see `fileop::show_security_dialog`).

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GetAce, LookupAccountSidW, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, INHERITED_ACE,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};

use crate::win;

const ACCESS_ALLOWED: u8 = 0;
const ACCESS_DENIED: u8 = 1;
const INHERIT_ONLY: u8 = 0x08;

pub struct Ace {
    pub account: String,
    pub allow: bool,
    pub rights: String,
    pub inherited: bool,
    pub inherit_only: bool,
}

pub struct AclInfo {
    pub owner: String,
    pub aces: Vec<Ace>,
}

pub fn read(path: &str) -> Result<AclInfo, String> {
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            win::wide(path, "").as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(win::error_text(rc));
    }
    let mut info = AclInfo { owner: if owner.is_null() { String::new() } else { account(owner) }, aces: Vec::new() };
    if dacl.is_null() {
        // A NULL DACL grants everyone full access.
        info.aces.push(Ace { account: "Everyone".into(), allow: true, rights: "Full control".into(), inherited: false, inherit_only: false });
    } else {
        let count = unsafe { (*dacl).AceCount };
        for i in 0..count as u32 {
            let mut ace: *mut c_void = null_mut();
            if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
                continue;
            }
            // SAFETY: every ACE starts with ACE_HEADER; allow/deny ACEs share ACCESS_ALLOWED_ACE's layout.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED && header.AceType != ACCESS_DENIED {
                continue;
            }
            let a = ace as *const ACCESS_ALLOWED_ACE;
            let mask = unsafe { (*a).Mask };
            let sid = unsafe { &(*a).SidStart as *const u32 as PSID };
            info.aces.push(Ace {
                account: account(sid),
                allow: header.AceType == ACCESS_ALLOWED,
                rights: rights(mask),
                inherited: header.AceFlags as u32 & INHERITED_ACE != 0,
                inherit_only: header.AceFlags & INHERIT_ONLY != 0,
            });
        }
    }
    unsafe { LocalFree(sd as _) };
    Ok(info)
}

/// `DOMAIN\name`, falling back to the SID string for unknown accounts.
fn account(sid: PSID) -> String {
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut n, mut d, mut kind) = (name.len() as u32, domain.len() as u32, 0);
    let ok = unsafe { LookupAccountSidW(std::ptr::null(), sid, name.as_mut_ptr(), &mut n, domain.as_mut_ptr(), &mut d, &mut kind) };
    let mut out = String::new();
    if ok != 0 {
        if d > 0 {
            win::push_utf16(&mut out, &domain[..d as usize]);
            out.push('\\');
        }
        win::push_utf16(&mut out, &name[..n as usize]);
        return out;
    }
    let mut s: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } != 0 {
        let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
        win::push_utf16(&mut out, unsafe { std::slice::from_raw_parts(s, len) });
        unsafe { LocalFree(s as _) };
    }
    out
}

/// Summarize an access mask the way Explorer's Security tab does.
fn rights(mask: u32) -> String {
    const GENERIC_ALL: u32 = 0x1000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    let named = match mask {
        0x001F_01FF | GENERIC_ALL => "Full control",
        0x0013_01BF => "Modify",
        0x0012_00A9 => "Read & execute",
        0x0012_0089 => "Read",
        0x0010_0116 => "Write",
        0x0012_019F => "Read & write",
        m if m == GENERIC_READ | GENERIC_EXECUTE => "Read & execute",
        m if m == GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE => "Modify",
        _ => "",
    };
    if named.is_empty() { format!("Special ({mask:#010x})") } else { named.to_string() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_own_temp_dir() {
        let dir = std::env::temp_dir();
        let info = super::read(&dir.to_string_lossy()).unwrap();
        assert!(!info.owner.is_empty());
        assert!(!info.aces.is_empty());
    }
}
