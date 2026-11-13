//! Fast per-user visibility checks for the index service.
//!
//! Opening every folder with matches as the client (the exact check) costs
//! a file open per folder, each one through the filesystem filter stack
//! (antivirus included): seconds for a broad query on a big drive. But on
//! Windows, listing a folder depends only on that folder's own access
//! control list: everyone has "bypass traverse checking" by default, so
//! ancestors don't matter. And folders share few distinct security
//! descriptors (children inherit them). So:
//!
//! * each folder's descriptor is read once, by the service, and cached with
//!   duplicates merged (`Descriptors`);
//! * a query asks `AccessCheck` with the client's token, once per distinct
//!   descriptor: no file opens at all;
//! * the cache follows the change journal (`usn::Tail`): any permission
//!   change clears it, and a folder created, deleted or renamed drops its
//!   entry, so nothing stale outlives a second's poll.
//!
//! Tokens without the traverse bypass, and drives without a change journal,
//! use the exact check instead. Not covered: mandatory integrity labels
//! (kept in the SACL, which needs extra privilege to read); the default
//! policy only restricts writing up, not listing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use windows_sys::Win32::Foundation::{HANDLE, LUID};
use windows_sys::Win32::Security::{
    AccessCheck, GetFileSecurityW, LookupPrivilegeValueW, PrivilegeCheck, DACL_SECURITY_INFORMATION, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION,
    LUID_AND_ATTRIBUTES, OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, SE_CHANGE_NOTIFY_NAME,
};
use windows_sys::Win32::System::SystemServices::PRIVILEGE_SET_ALL_NECESSARY;
use windows_sys::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY};

/// A self-relative security descriptor, 8-byte aligned for the API.
pub struct Sd {
    words: Vec<u64>,
}

impl Sd {
    fn ptr(&self) -> *mut core::ffi::c_void {
        self.words.as_ptr() as *mut _
    }
}

/// Read a folder's owner, group and DACL (as the service).
fn read_sd(path: &str) -> Option<Sd> {
    let p = crate::win::wide(path, "");
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut words = vec![0u64; 64];
    for _ in 0..2 {
        let mut needed = 0u32;
        if unsafe { GetFileSecurityW(p.as_ptr(), info, words.as_mut_ptr() as *mut _, (words.len() * 8) as u32, &mut needed) } != 0 {
            return Some(Sd { words });
        }
        if needed as usize <= words.len() * 8 {
            return None;
        }
        words = vec![0u64; (needed as usize).div_ceil(8)];
    }
    None
}

/// Folder path -> its descriptor, shared by all queries; distinct
/// descriptors are stored once.
#[derive(Default)]
pub struct Descriptors {
    by_path: RwLock<HashMap<Box<str>, u32>>,
    sds: RwLock<Vec<Arc<Sd>>>,
    ids: Mutex<HashMap<Box<[u64]>, u32>>,
}

fn key(path: &str) -> Box<str> {
    path.trim_end_matches('\\').to_lowercase().into()
}

impl Descriptors {
    /// The folder's descriptor id and descriptor, reading it if needed.
    pub fn get(&self, path: &str) -> Option<(u32, Arc<Sd>)> {
        let k = key(path);
        if let Some(&id) = self.by_path.read().unwrap_or_else(PoisonError::into_inner).get(&k) {
            return self.sds.read().unwrap_or_else(PoisonError::into_inner).get(id as usize).map(|sd| (id, sd.clone()));
        }
        let sd = read_sd(path)?;
        let id = {
            let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
            match ids.get(&sd.words[..]) {
                Some(&id) => id,
                None => {
                    let mut sds = self.sds.write().unwrap_or_else(PoisonError::into_inner);
                    let id = sds.len() as u32;
                    ids.insert(sd.words.clone().into_boxed_slice(), id);
                    sds.push(Arc::new(sd));
                    id
                }
            }
        };
        self.by_path.write().unwrap_or_else(PoisonError::into_inner).insert(k, id);
        let sd = self.sds.read().unwrap_or_else(PoisonError::into_inner)[id as usize].clone();
        Some((id, sd))
    }

    /// Drop one folder's entry (created, deleted or renamed).
    pub fn forget(&self, path: &str) {
        self.by_path.write().unwrap_or_else(PoisonError::into_inner).remove(&key(path));
    }

    /// Drop everything (a permission changed somewhere).
    pub fn clear(&self) {
        // Order matters: paths first, so no lookup finds an id whose
        // descriptor is gone.
        self.by_path.write().unwrap_or_else(PoisonError::into_inner).clear();
        self.ids.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.sds.write().unwrap_or_else(PoisonError::into_inner).clear();
    }

    pub fn folders(&self) -> usize {
        self.by_path.read().unwrap_or_else(PoisonError::into_inner).len()
    }

    pub fn distinct(&self) -> usize {
        self.sds.read().unwrap_or_else(PoisonError::into_inner).len()
    }
}

/// Whether `token` (an impersonation token) may list a folder protected by `sd`.
pub fn may_list(sd: &Sd, token: HANDLE) -> bool {
    let mapping = GENERIC_MAPPING { GenericRead: FILE_GENERIC_READ, GenericWrite: FILE_GENERIC_WRITE, GenericExecute: FILE_GENERIC_EXECUTE, GenericAll: FILE_ALL_ACCESS };
    let mut privileges = [0u64; 32];
    let mut plen = (privileges.len() * 8) as u32;
    let (mut granted, mut status) = (0u32, 0);
    let ok = unsafe { AccessCheck(sd.ptr(), token, FILE_LIST_DIRECTORY, &mapping, privileges.as_mut_ptr() as *mut PRIVILEGE_SET, &mut plen, &mut granted, &mut status) };
    ok != 0 && status != 0 && granted & FILE_LIST_DIRECTORY != 0
}

/// Whether the token has "bypass traverse checking" enabled (then only a
/// folder's own ACL decides whether it can be listed).
pub fn bypasses_traverse(token: HANDLE) -> bool {
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_CHANGE_NOTIFY_NAME, &mut luid) } == 0 {
        return false;
    }
    let mut set = PRIVILEGE_SET { PrivilegeCount: 1, Control: PRIVILEGE_SET_ALL_NECESSARY, Privilege: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: 0 }] };
    let mut result = 0;
    unsafe { PrivilegeCheck(token, &mut set, &mut result) != 0 && result != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{DuplicateTokenEx, SecurityImpersonation, TokenImpersonation, SECURITY_ATTRIBUTES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, SetThreadToken};

    fn own_impersonation_token() -> HANDLE {
        let (mut primary, mut imp) = (std::ptr::null_mut(), std::ptr::null_mut());
        unsafe {
            assert!(OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut primary) != 0);
            assert!(DuplicateTokenEx(primary, TOKEN_IMPERSONATE | TOKEN_QUERY, std::ptr::null(), SecurityImpersonation, TokenImpersonation, &mut imp) != 0);
            CloseHandle(primary);
        }
        imp
    }

    /// The exact check the fast one replaces: open the folder as the token.
    fn exact(path: &str, token: HANDLE) -> bool {
        unsafe {
            assert!(SetThreadToken(std::ptr::null(), token) != 0);
            let p = crate::win::wide(path, "");
            let h = CreateFileW(p.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
            SetThreadToken(std::ptr::null(), std::ptr::null_mut());
            if h != INVALID_HANDLE_VALUE {
                CloseHandle(h);
            }
            h != INVALID_HANDLE_VALUE
        }
    }

    #[test]
    fn agrees_with_opening_the_folder() {
        let base = std::env::temp_dir().join(format!("fex-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("open")).unwrap();
        // Everyone is denied listing (deny entries win, even for administrators).
        let denied = base.join("denied");
        let sddl: Vec<u16> = "D:P(D;OICI;0x1;;;WD)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)".encode_utf16().chain([0]).collect();
        let mut sd = std::ptr::null_mut();
        unsafe {
            assert!(ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) != 0);
            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            let p: Vec<u16> = denied.to_string_lossy().encode_utf16().chain([0]).collect();
            assert!(CreateDirectoryW(p.as_ptr(), &sa) != 0);
            windows_sys::Win32::Foundation::LocalFree(sd);
        }
        let token = own_impersonation_token();
        assert!(bypasses_traverse(token));
        let cache = Descriptors::default();
        for (dir, want) in [(base.join("open"), true), (denied.clone(), false)] {
            let path = dir.to_string_lossy().to_string();
            let (_, sd) = cache.get(&path).expect("descriptor readable");
            assert_eq!(may_list(&sd, token), want, "{path}");
            assert_eq!(exact(&path, token), want, "exact check disagrees for {path}");
        }
        // Identical descriptors are stored once; forgetting drops the entry.
        std::fs::create_dir_all(base.join("open2")).unwrap();
        let a = cache.get(&base.join("open").to_string_lossy()).unwrap().0;
        let b = cache.get(&base.join("open2").to_string_lossy()).unwrap().0;
        assert_eq!(a, b);
        assert_eq!(cache.distinct(), 2);
        cache.forget(&base.join("open").to_string_lossy());
        assert_eq!(cache.folders(), 2);
        unsafe { CloseHandle(token) };
        let _ = std::fs::remove_dir_all(base);
    }
}
