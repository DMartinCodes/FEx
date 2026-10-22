//! Thin wrappers over the Win32 calls FEx needs. Everything here is
//! allocation-conscious: directory scans hand out borrowed UTF-16 names and
//! let the caller decide what (if anything) to keep.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE,
    SYSTEMTIME,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    NetApiBufferFree, MAX_PREFERRED_LENGTH,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileTimeToLocalFileTime, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetDiskFreeSpaceExW, GetFileAttributesW, GetFullPathNameW, GetLogicalDrives, NetShareEnum,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SYSTEM, FIND_FIRST_EX_LARGE_FETCH, INVALID_FILE_ATTRIBUTES, SHARE_INFO_1,
    STYPE_DISKTREE, STYPE_MASK, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::Time::FileTimeToSystemTime;
use windows_sys::Win32::UI::Shell::{
    SHGetKnownFolderPath, ShellExecuteW, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
    FOLDERID_Pictures, FOLDERID_Profile, SE_ERR_NOASSOC,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const ATTR_DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
pub const ATTR_HIDDEN: u32 = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
pub const ATTR_REPARSE: u32 = FILE_ATTRIBUTE_REPARSE_POINT;

/// One directory entry, borrowed from the kernel's find buffer.
pub struct RawEntry<'a> {
    pub name: &'a [u16],
    pub attrs: u32,
    pub size: u64,
    pub mtime: u64,
}

/// NUL-terminated UTF-16 for `path` + `suffix`, adding the `\\?\` prefix when
/// the result would exceed MAX_PATH so deep trees still work.
pub fn wide(path: &str, suffix: &str) -> Vec<u16> {
    let mut s = String::with_capacity(path.len() + suffix.len() + 8);
    if path.len() + suffix.len() >= 248 && !path.starts_with(r"\\?\") {
        if let Some(unc) = path.strip_prefix(r"\\") {
            s.push_str(r"\\?\UNC\");
            s.push_str(unc);
        } else {
            s.push_str(r"\\?\");
            s.push_str(path);
        }
    } else {
        s.push_str(path);
    }
    s.push_str(suffix);
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Append UTF-16 `w` to `out` (lossy for unpaired surrogates).
#[inline]
pub fn push_utf16(out: &mut String, w: &[u16]) {
    out.extend(char::decode_utf16(w.iter().copied()).map(|c| c.unwrap_or('\u{FFFD}')));
}

/// Enumerate `dir`, calling `f` for every entry except `.` and `..`.
/// Uses the basic info level (no 8.3 names) and large-fetch buffers, which is
/// the fastest enumeration path Win32 offers.
pub fn scan_dir(dir: &str, mut f: impl FnMut(RawEntry<'_>)) -> Result<(), u32> {
    let pattern = wide(dir, if dir.ends_with('\\') { "*" } else { "\\*" });
    // SAFETY: WIN32_FIND_DATAW is plain old data; zeroed is a valid initial state.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut c_void,
            FindExSearchNameMatch,
            null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return if err == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(err) };
    }
    // Closed however the scan ends, including a panicking callback.
    struct Find(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Find {
        fn drop(&mut self) {
            unsafe { FindClose(self.0) };
        }
    }
    let find = Find(handle);
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !dot {
            f(RawEntry {
                name,
                attrs: data.dwFileAttributes,
                size: (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64,
                mtime: ft_u64(data.ftLastWriteTime),
            });
        }
        if unsafe { FindNextFileW(find.0, &mut data) } == 0 {
            let err = unsafe { GetLastError() };
            return if err == ERROR_NO_MORE_FILES { Ok(()) } else { Err(err) };
        }
    }
}

pub fn ft_u64(ft: FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// A FILETIME (as u64) as a `SystemTime`; `None` for 0 or pre-1970 times.
pub fn system_time(ft: u64) -> Option<std::time::SystemTime> {
    const UNIX_EPOCH_FT: u64 = 116_444_736_000_000_000;
    let t = ft.checked_sub(UNIX_EPOCH_FT).filter(|_| ft != 0)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(t.saturating_mul(100)))
}

/// Format a FILETIME (as u64) in local time: `YYYY-MM-DD HH:MM`.
pub fn format_time(t: u64) -> String {
    if t == 0 {
        return String::new();
    }
    let utc = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: SYSTEMTIME is plain old data.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToLocalFileTime(&utc, &mut local) == 0 || FileTimeToSystemTime(&local, &mut st) == 0 {
            return String::new();
        }
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute)
}

/// File attributes of `path`, or `None` if it doesn't exist / is inaccessible.
pub fn attributes(path: &str) -> Option<u32> {
    let a = unsafe { GetFileAttributesW(wide(path, "").as_ptr()) };
    (a != INVALID_FILE_ATTRIBUTES).then_some(a)
}

/// Resolve `.`/`..` and relative components into an absolute path.
pub fn full_path(path: &str) -> Option<String> {
    let w = wide(path, "");
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), null_mut()) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            let mut s = String::with_capacity(n);
            push_utf16(&mut s, &buf[..n]);
            return Some(s);
        }
        buf.resize(n + 1, 0);
    }
}

/// Root paths of all mounted drives (`C:\`, `D:\`, mapped network drives, ...).
pub fn drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i as u8) as char)).collect()
}

/// Available and total bytes for a drive root.
pub fn drive_space(path: &str) -> Option<(u64, u64)> {
    let (mut free, mut total) = (0u64, 0u64);
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide(path, "").as_ptr(),
            &mut free,
            &mut total,
            null_mut(),
        )
    };
    (ok != 0).then_some((free, total))
}

/// Quick-access folders as (label, path), resolved through the shell so
/// OneDrive/redirected locations are honoured.
pub fn quick_access() -> Vec<(&'static str, String)> {
    let ids = [
        ("Home", FOLDERID_Profile),
        ("Desktop", FOLDERID_Desktop),
        ("Documents", FOLDERID_Documents),
        ("Downloads", FOLDERID_Downloads),
        ("Pictures", FOLDERID_Pictures),
    ];
    ids.iter().filter_map(|(label, id)| known_folder(id).map(|path| (*label, path))).collect()
}

/// Path of a shell known folder (`FOLDERID_*`).
pub fn known_folder(id: &windows_sys::core::GUID) -> Option<String> {
    let mut p: *mut u16 = null_mut();
    unsafe {
        if SHGetKnownFolderPath(id, 0, null_mut(), &mut p) < 0 {
            CoTaskMemFree(p as *const c_void);
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let mut s = String::new();
        push_utf16(&mut s, std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p as *const c_void);
        Some(s)
    }
}

/// Disk shares exported by `server` (given as `\\server`).
pub fn net_shares(server: &str) -> Result<Vec<String>, u32> {
    let w = wide(server, "");
    let mut out = Vec::new();
    let mut resume = 0u32;
    loop {
        let mut buf: *mut u8 = null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let rc = unsafe {
            NetShareEnum(
                w.as_ptr(),
                1,
                &mut buf,
                MAX_PREFERRED_LENGTH,
                &mut read,
                &mut total,
                &mut resume,
            )
        };
        if rc != 0 && rc != ERROR_MORE_DATA {
            if !buf.is_null() {
                unsafe { NetApiBufferFree(buf as *const c_void) };
            }
            return Err(rc);
        }
        if !buf.is_null() {
            // SAFETY: NetShareEnum returns `read` SHARE_INFO_1 records in this buffer.
            let infos = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
        for info in infos {
            if info.shi1_type & STYPE_MASK == STYPE_DISKTREE {
                let p = info.shi1_netname;
                let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
                let mut s = String::new();
                push_utf16(&mut s, unsafe { std::slice::from_raw_parts(p, len) });
                out.push(s);
            }
        }
            unsafe { NetApiBufferFree(buf as *const c_void) };
        }
        if rc == 0 {
            break;
        }
    }
    Ok(out)
}

/// Initialize COM on the calling worker thread (shell APIs require it).
pub fn com_init() {
    unsafe { CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
}

/// Open `path` with its registered default handler (falls back to the
/// "Open with" dialog when no association exists). Blocks; call off the UI thread.
pub fn shell_open(path: &str) -> Result<(), String> {
    let file = wide(path, "");
    let verb = |v: &str| -> Vec<u16> { v.encode_utf16().chain(std::iter::once(0)).collect() };
    com_init();
    unsafe {
        let mut rc = ShellExecuteW(null_mut(), verb("open").as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) as isize;
        if rc == SE_ERR_NOASSOC as isize {
            rc = ShellExecuteW(null_mut(), verb("openas").as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) as isize;
        }
        if rc <= 32 {
            return Err(format!("could not open {path} (shell error {rc})"));
        }
    }
    Ok(())
}

/// Run `path` with the Windows elevation prompt. Call off the UI thread.
pub fn shell_run_as(path: &str) -> Result<(), String> {
    let file = wide(path, "");
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let rc = unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) as isize };
    if rc > 32 {
        Ok(())
    } else if rc == 1223 {
        Err("Elevation was cancelled".into())
    } else {
        Err(format!("could not run {path} as administrator (shell error {rc})"))
    }
}

/// Whether a window placed at `x, y` would be visible on some monitor (a
/// saved position may belong to a monitor that is gone).
pub fn on_screen(x: f32, y: f32) -> bool {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
    let pt = POINT { x: x as i32 + 40, y: y as i32 + 20 };
    !unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONULL) }.is_null()
}

/// Mouse position in screen pixels.
pub fn cursor_pos() -> (i32, i32) {
    use windows_sys::Win32::Foundation::POINT;
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt) };
    (pt.x, pt.y)
}

/// Whether the screen point lies inside `hwnd`'s window rectangle.
pub fn window_contains(hwnd: windows_sys::Win32::Foundation::HWND, x: i32, y: i32) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r) } != 0;
    ok && x >= r.left
        && x < r.right
        && y >= r.top
        && y < r.bottom
}

/// Our window, for owning system dialogs (credential prompts). Call on the UI thread.
pub fn active_window() -> isize {
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow() as isize }
}

/// Blocking error dialog (used by the crash reporter).
pub fn message_box(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let (t, m) = (wide(title, ""), wide(text, ""));
    unsafe { MessageBoxW(null_mut(), m.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR) };
}

/// Human-readable text for a Win32 / NetAPI error code.
pub fn error_text(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

/// Text of a caught panic's payload.
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or(payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown error")
        .to_string()
}

/// Whether FEx runs with an elevated (administrator) token. Then nothing it
/// does shows a UAC prompt, so the UI says so.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Mark of the Web for content of unknown origin (dropped from browsers,
/// mail clients and other apps): the Internet zone.
pub const INTERNET_ZONE: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

/// The Mark of the Web on `path` (its `Zone.Identifier` stream), if any.
/// Files taken out of a marked archive must carry it too, so SmartScreen and
/// Office's Protected View still warn about them.
pub fn zone_of(path: &str) -> Option<Vec<u8>> {
    std::fs::read(format!("{path}:Zone.Identifier")).ok().filter(|z| !z.is_empty())
}

/// Give `path` the Mark of the Web `zone`. Volumes without alternate data
/// streams (FAT) can't hold one; Windows can't mark files there either.
pub fn set_zone(path: &std::path::Path, zone: &[u8]) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let _ = std::fs::write(stream, zone);
}
