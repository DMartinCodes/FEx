//! Installing an update on Windows: the copy in `%ProgramFiles%\FEx` (only
//! administrators can write there) replaces itself and restarts the index
//! service. Downloads use WinHTTP, so the system's proxy settings and
//! certificate store apply.

use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts, INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP, WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, FOLDERID_ProgramFiles, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::{verified_bytes, Download};
use crate::winsvc::{proto, scm};

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `%ProgramFiles%\FEx`, from the shell (not the environment, which the
/// user controls).
fn install_dir() -> Result<PathBuf, String> {
    crate::win::known_folder(&FOLDERID_ProgramFiles).map(|p| PathBuf::from(p).join("FEx")).ok_or_else(|| "cannot find Program Files".into())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// Write `bytes` as `dir\fex.exe` (via `fex.exe.new`), keeping the previous
/// copy as `fex.exe.old` (renaming works while it runs). Returns whether
/// there was a previous copy.
fn swap_in(dir: &Path, bytes: &[u8]) -> Result<bool, String> {
    use std::io::Write;
    let (target, new, old) = (dir.join("fex.exe"), dir.join("fex.exe.new"), dir.join("fex.exe.old"));
    let _ = fs::remove_file(&new);
    fs::File::create(&new).and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all())).map_err(|e| format!("cannot write {}: {e}", new.display()))?;
    // Leftovers of earlier updates (best effort: one may still be running).
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("fex.exe.old-") || name == "fex.exe.failed" {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    // A previous `.old` may still be running (an open window): move it aside.
    if fs::remove_file(&old).is_err() && old.exists() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let _ = fs::rename(&old, dir.join(format!("fex.exe.old-{stamp}")));
    }
    let had = target.exists();
    if had {
        fs::rename(&target, &old).map_err(|e| format!("cannot move {} aside: {e}", target.display()))?;
    }
    if let Err(e) = fs::rename(&new, &target) {
        if had {
            let _ = fs::rename(&old, &target);
        }
        return Err(format!("cannot install {}: {e}", target.display()));
    }
    Ok(had)
}

/// Copy this binary into `%ProgramFiles%\FEx` (unless it runs from there)
/// and return that path: what the service is registered to run.
pub fn install_copy() -> Result<PathBuf, String> {
    let dir = install_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {} (run as administrator): {e}", dir.display()))?;
    let target = dir.join("fex.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !same_file(&me, &target) {
        let bytes = fs::read(&me).map_err(|e| format!("{}: {e}", me.display()))?;
        swap_in(&dir, &bytes)?;
    }
    Ok(target)
}

/// The copy that installs updates: the one in Program Files if present.
pub fn installed_path() -> Result<PathBuf, String> {
    let installed = install_dir()?.join("fex.exe");
    if installed.exists() {
        return Ok(installed);
    }
    std::env::current_exe().map_err(|e| e.to_string())
}

fn wait_ready(limit: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < limit {
        if proto::status().is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Install a downloaded release (as administrator): verify it, install it
/// in Program Files, restart the service on it, and roll back if the new
/// service doesn't answer. A service registered elsewhere is moved to the
/// Program Files copy. Without `force`, refuses while the service builds a
/// drive's first index.
pub fn apply(bin: &Path, sig: &Path, force: bool) -> Result<String, String> {
    let (bytes, v) = verified_bytes(bin, sig)?;
    let dir = install_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {} (run as administrator): {e}", dir.display()))?;
    let target = dir.join("fex.exe");
    let svc = scm::installed()?;
    let running = svc.as_ref().is_some_and(|s| s.running);
    if running && !force && proto::status().is_ok_and(|st| st.ready.len() < st.roots.len()) {
        return Err("the index service is still building a drive's first index; updating now would start it over. Try later, or pass --force".into());
    }
    if running {
        // The service saves its indexes and journal positions as it stops.
        scm::stop_and_wait(Duration::from_secs(120))?;
    }
    let had_old = match swap_in(&dir, &bytes) {
        Ok(had) => had,
        Err(e) => {
            if running {
                let _ = scm::start();
            }
            return Err(e);
        }
    };
    let command = format!("\"{}\" service run", target.display());
    if let Some(s) = &svc {
        let moved = s.command != command;
        if moved {
            scm::set_command(&command)?;
        }
        if running && !(scm::start().is_ok() && wait_ready(Duration::from_secs(60))) {
            let _ = scm::stop_and_wait(Duration::from_secs(30));
            let failed = dir.join("fex.exe.failed");
            let _ = fs::remove_file(&failed);
            let restored = had_old && fs::rename(&target, &failed).is_ok() && fs::rename(dir.join("fex.exe.old"), &target).is_ok();
            if moved {
                let _ = scm::set_command(&s.command);
            }
            let _ = scm::start();
            return Err(format!("FEx {v} installed, but the index service didn't start; {}", if restored || moved { "rolled back" } else { "see the service log" }));
        }
    }
    Ok(format!("FEx {v} installed in {}{}", target.display(), if running { "; the index service restarted with its saved indexes" } else { "" }))
}

/// From the app: install a verified download as administrator. Windows
/// asks (UAC); the elevated process is the installed copy, never the
/// download.
pub fn elevate_apply(d: &Download, force: bool) -> Result<(), String> {
    let installer = installed_path()?;
    let report_file = installer.with_file_name(REPORT);
    let before = fs::metadata(&report_file).and_then(|m| m.modified()).ok();
    let params = format!("update apply \"{}\" \"{}\"{}", d.bin.display(), d.sig.display(), if force { " --force" } else { "" });
    let (verb, file, params) = (w("runas"), w(&installer.to_string_lossy()), w(&params));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(if unsafe { GetLastError() } == 1223 { "cancelled".into() } else { format!("cannot start the installer: {}", std::io::Error::last_os_error()) });
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        return Ok(());
    }
    // Its message, if it wrote one during this run.
    let fresh = fs::metadata(&report_file).and_then(|m| m.modified()).ok().is_some_and(|t| Some(t) != before);
    let msg = fresh.then(|| fs::read_to_string(&report_file).ok()).flatten();
    Err(msg.and_then(|m| m.strip_prefix("error\n").map(str::to_string)).unwrap_or_else(|| format!("the installer failed (exit code {code})")))
}

/// The elevated installer's outcome, in the install folder (only
/// administrators write there, everyone may read it). Never in a folder of
/// the user's: an administrator writing where a user can plant links could
/// be steered into overwriting a protected file.
const REPORT: &str = "last-update.txt";

/// Record the outcome for the app that started the installer (it has no
/// console to read).
pub(super) fn report(result: &Result<String, String>) {
    let text = match result {
        Ok(m) => format!("ok\n{m}"),
        Err(e) => format!("error\n{e}"),
    };
    if let Ok(dir) = install_dir()
        && dir.is_dir()
    {
        let _ = fs::write(dir.join(REPORT), text);
    }
}

/// Start the installed FEx and let this process exit.
pub fn relaunch() {
    if let Ok(p) = installed_path() {
        let _ = std::process::Command::new(p).spawn();
    }
}

/// `HKLM\SOFTWARE\Policies\FEx` `DisableUpdateCheck` (DWORD) is non-zero.
pub(super) fn policy_disables_checks() -> bool {
    let (key, value) = (w(r"SOFTWARE\Policies\FEx"), w("DisableUpdateCheck"));
    let (mut data, mut len) = (0u32, 4u32);
    unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut data).cast(), &mut len) == 0 && data != 0 }
}

struct Internet(*mut core::ffi::c_void);

impl Drop for Internet {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn opened(h: *mut core::ffi::c_void, what: &str) -> Result<Internet, String> {
    if h.is_null() {
        return Err(format!("{what} failed: {}", std::io::Error::last_os_error()));
    }
    Ok(Internet(h))
}

/// GET an https URL (following https redirects), at most `max` bytes.
pub(super) fn fetch(url: &str, max: usize) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("only https URLs are fetched")?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    unsafe {
        let agent = w(concat!("FEx/", env!("CARGO_PKG_VERSION")));
        let mut session = WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0);
        if session.is_null() {
            // Before Windows 8.1.
            session = WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, null(), null(), 0);
        }
        let session = opened(session, "WinHttpOpen")?;
        let set = |option: u32, value: u32| WinHttpSetOption(session.0, option, (&raw const value).cast(), 4) != 0;
        // TLS 1.2+ (1.3 is unknown before Windows 11 / Server 2022).
        if !set(WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3) {
            set(WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2);
        }
        set(WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP);
        WinHttpSetTimeouts(session.0, 20_000, 20_000, 30_000, 60_000);
        let h = w(host);
        let conn = opened(WinHttpConnect(session.0, h.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0), "WinHttpConnect")?;
        let (verb, p) = (w("GET"), w(&path));
        let req = opened(WinHttpOpenRequest(conn.0, verb.as_ptr(), p.as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE), "WinHttpOpenRequest")?;
        if WinHttpSendRequest(req.0, null(), 0, null(), 0, 0, 0) == 0 || WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(format!("download failed: {}", std::io::Error::last_os_error()));
        }
        let (mut status, mut len) = (0u32, 4u32);
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, null(), (&raw mut status).cast(), &mut len, null_mut());
        match status {
            200 => {}
            404 => return Err(format!("not found: {url}")),
            s => return Err(format!("download failed: HTTP {s}")),
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut n = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) == 0 {
                return Err(format!("download failed: {}", std::io::Error::last_os_error()));
            }
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > max {
                return Err("download too large".into());
            }
        }
    }
}
