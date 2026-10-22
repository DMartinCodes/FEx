//! Network shares: mapped drives and connecting with credentials.
//!
//! Credentials are never handled by FEx: connections that need them go
//! through Windows' own credential prompt (`CONNECT_INTERACTIVE`).

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_CANCELLED, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HANDLE};
use windows_sys::Win32::NetworkManagement::WNet::{
    WNetAddConnection3W, WNetCancelConnection2W, WNetCloseEnum, WNetEnumResourceW, WNetOpenEnumW,
    CONNECT_INTERACTIVE, CONNECT_PROMPT, CONNECT_UPDATE_PROFILE, NETRESOURCEW, RESOURCETYPE_DISK,
    RESOURCE_CONNECTED, RESOURCE_REMEMBERED,
};

use crate::win::{self, wide};

fn pwstr(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    let mut s = String::with_capacity(len);
    win::push_utf16(&mut s, unsafe { std::slice::from_raw_parts(p, len) });
    Some(s)
}

fn wnet_error(code: u32) -> String {
    if code == ERROR_CANCELLED { "Cancelled".into() } else { win::error_text(code) }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    /// Drive letter (`Z:`), or `None` for a deviceless connection.
    pub local: Option<String>,
    pub remote: String,
    pub connected: bool,
    pub persistent: bool,
}

fn enum_resources(scope: u32) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut h: HANDLE = null_mut();
    if unsafe { WNetOpenEnumW(scope, RESOURCETYPE_DISK, 0, null(), &mut h) } != 0 {
        return out;
    }
    // u64 backing keeps NETRESOURCEW records aligned.
    let mut buf = vec![0u64; 2048];
    loop {
        let mut count = u32::MAX;
        let mut size = (buf.len() * 8) as u32;
        let rc = unsafe { WNetEnumResourceW(h, &mut count, buf.as_mut_ptr() as *mut c_void, &mut size) };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != 0 && rc != ERROR_MORE_DATA {
            break;
        }
        let capacity = buf.len() * 8 / size_of::<NETRESOURCEW>();
        let returned = (count as usize).min(capacity);
        if returned > 0 && count != u32::MAX {
            let items = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const NETRESOURCEW, returned) };
            for r in items {
                if let Some(remote) = pwstr(r.lpRemoteName) {
                    // Remembered connections come back as "f:"; normalize to "F:".
                    out.push((pwstr(r.lpLocalName).filter(|s| !s.is_empty()).map(|s| s.to_uppercase()), remote));
                }
            }
        }
        if rc == ERROR_MORE_DATA {
            let minimum_words = (size as usize).div_ceil(size_of::<u64>());
            buf.resize(minimum_words.max(buf.len().saturating_mul(2)), 0);
        }
    }
    unsafe { WNetCloseEnum(h) };
    out
}

/// Current and remembered (persistent) disk connections, merged.
pub fn connections() -> Vec<Connection> {
    let remembered = enum_resources(RESOURCE_REMEMBERED);
    let mut list: Vec<Connection> = enum_resources(RESOURCE_CONNECTED)
        .into_iter()
        .map(|(local, remote)| {
            let persistent = remembered.iter().any(|(l, _)| l.is_some() && *l == local);
            Connection { local, remote, connected: true, persistent }
        })
        .collect();
    for (local, remote) in remembered {
        if !list.iter().any(|c| c.local.is_some() && c.local == local) {
            list.push(Connection { local, remote, connected: false, persistent: true });
        }
    }
    list.sort_by(|a, b| a.local.cmp(&b.local).then_with(|| a.remote.cmp(&b.remote)));
    list
}

/// Servers (`\\name`) behind the given connections, deduplicated.
pub fn servers_of(conns: &[Connection]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in conns {
        if let Some(server) = c.remote.strip_prefix(r"\\").and_then(|r| r.split('\\').next()).filter(|s| !s.is_empty()) {
            let s = format!(r"\\{server}");
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&s)) {
                out.push(s);
            }
        }
    }
    out
}

/// Drive letters not in use (and not reserved by a remembered mapping).
pub fn free_letters() -> Vec<char> {
    let used: Vec<char> = win::drives().iter().filter_map(|d| d.chars().next()).collect();
    let remembered: Vec<char> = connections().iter().filter_map(|c| c.local.as_ref()?.chars().next()).collect();
    ('D'..='Z').filter(|c| !used.contains(c) && !remembered.contains(c)).collect()
}

/// Connect `remote` (`\\server\share`), optionally as drive `letter`.
/// Windows shows its credential prompt if needed (or always, with `prompt`).
/// `hwnd` owns that dialog; blocks, so call off the UI thread.
pub fn connect(hwnd: isize, remote: &str, letter: Option<char>, persistent: bool, prompt: bool) -> Result<(), String> {
    let mut remote_w = wide(remote.trim_end_matches('\\'), "");
    let mut local_w = letter.map(|l| wide(&format!("{l}:"), ""));
    let res = NETRESOURCEW {
        dwScope: 0,
        dwType: RESOURCETYPE_DISK,
        dwDisplayType: 0,
        dwUsage: 0,
        lpLocalName: local_w.as_mut().map_or(null_mut(), |w| w.as_mut_ptr()),
        lpRemoteName: remote_w.as_mut_ptr(),
        lpComment: null_mut(),
        lpProvider: null_mut(),
    };
    let mut flags = CONNECT_INTERACTIVE;
    if prompt {
        flags |= CONNECT_PROMPT;
    }
    if persistent && letter.is_some() {
        flags |= CONNECT_UPDATE_PROFILE;
    }
    match unsafe { WNetAddConnection3W(hwnd as *mut c_void, &res, null(), null(), flags) } {
        0 => Ok(()),
        e => Err(format!("{remote}: {}", wnet_error(e))),
    }
}

/// Disconnect a drive (`Z:`) or a deviceless connection (`\\server\share`),
/// and forget it so it doesn't come back at sign-in.
pub fn disconnect(name: &str, force: bool) -> Result<(), String> {
    match unsafe { WNetCancelConnection2W(wide(name, "").as_ptr(), CONNECT_UPDATE_PROFILE, force as i32) } {
        0 => Ok(()),
        e => Err(format!("{name}: {}", wnet_error(e))),
    }
}

/// The connection to (re)authenticate when `path` is refused: the share for
/// `\\server\share\...`, or the server's IPC$ for a bare `\\server`.
pub fn auth_target(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\")?;
    let mut parts = rest.split('\\').filter(|p| !p.is_empty());
    let server = parts.next()?;
    Some(match parts.next() {
        Some(share) => format!(r"\\{server}\{share}"),
        None => format!(r"\\{server}\IPC$"),
    })
}

fn servers_file() -> Option<String> {
    std::env::var("APPDATA").ok().map(|d| format!(r"{d}\FEx\servers.txt"))
}

/// Servers the user has visited, remembered across sessions.
pub fn load_servers() -> Vec<String> {
    servers_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.lines().map(str::trim).filter(|l| l.starts_with(r"\\")).map(String::from).collect())
        .unwrap_or_default()
}

pub fn save_servers(servers: &[String]) {
    if let Some(f) = servers_file() {
        if let Some(dir) = f.rsplit_once('\\').map(|(d, _)| d) {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, servers.join("\r\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_from_connections() {
        let c = |r: &str| Connection { local: None, remote: r.into(), connected: true, persistent: false };
        let conns = [c(r"\\srv\a"), c(r"\\SRV\b"), c(r"\\10.0.0.1\x"), c(r"\\other.dom.com\y")];
        assert_eq!(servers_of(&conns), [r"\\srv", r"\\10.0.0.1", r"\\other.dom.com"]);
    }

    #[test]
    fn auth_targets() {
        assert_eq!(auth_target(r"\\srv\share\deep\dir").as_deref(), Some(r"\\srv\share"));
        assert_eq!(auth_target(r"\\srv").as_deref(), Some(r"\\srv\IPC$"));
        assert_eq!(auth_target(r"C:\x"), None);
    }
}
