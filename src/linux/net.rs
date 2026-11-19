//! Network shares through GVFS, the desktop's own network layer (the Linux
//! side of `net.rs`): connecting a share with `gio mount` makes it appear
//! under `$XDG_RUNTIME_DIR/gvfs`, where FEx and every file manager can
//! browse it, and `gio mount -u` disconnects it.
//!
//! Credentials are typed into FEx's dialog and handed to `gio mount` on its
//! standard input, in the order it asks for them; FEx never stores them.
//! Windows-style `\\server\share` is accepted and means SMB.

use std::process::Command;

use super::session;

/// What to answer when a share asks who is connecting.
#[derive(Clone, Default)]
pub struct Credentials {
    pub user: String,
    pub domain: String,
    pub password: String,
    /// Connect as a guest (SMB) or anonymously (FTP, WebDAV).
    pub anonymous: bool,
}

/// Schemes GVFS mounts as shares.
const SCHEMES: [&str; 9] = ["smb", "sftp", "ftp", "ftps", "dav", "davs", "afp", "nfs", "ssh"];

/// A share URL from what the user typed: `smb://nas/media`, `sftp://me@box/`,
/// or `\\nas\media` (SMB). `None` if it isn't one.
pub fn to_url(text: &str) -> Option<String> {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix(r"\\") {
        let rest = rest.trim_end_matches('\\').replace('\\', "/");
        return (!rest.is_empty()).then(|| format!("smb://{rest}"));
    }
    let (scheme, rest) = t.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let scheme = if scheme == "ssh" { "sftp".to_string() } else { scheme };
    (SCHEMES.contains(&scheme.as_str()) && !rest.trim_matches('/').is_empty()).then(|| format!("{scheme}://{}", rest.trim_end_matches('/')))
}

/// `smb://nas/media` -> `smb://nas` (servers are remembered by this).
pub fn server_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}"))
}

/// Whether `url` names a share (or folder in one), not just a server.
pub fn is_share(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, rest)| rest.trim_matches('/').contains('/'))
}

/// How long connecting a share may take (servers can be slow to answer,
/// and SMB tries several dialects), and anything else asked of GVFS.
const MOUNT_LIMIT: std::time::Duration = std::time::Duration::from_secs(90);
const QUERY_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

fn gio() -> Command {
    let mut c = Command::new("gio");
    // Messages are parsed: keep them in English.
    c.env("LC_ALL", "C").env("LANG", "C");
    c
}

fn failure(out: &std::process::Output) -> String {
    let e = String::from_utf8_lossy(&out.stderr);
    let e = e.lines().rfind(|l| !l.trim().is_empty() && !l.contains('[') && !l.ends_with(':')).unwrap_or("").trim();
    let e = e.strip_prefix("gio: ").unwrap_or(e);
    if e.is_empty() { "The connection failed".into() } else { e.to_string() }
}

/// The outcome of a connection attempt.
pub enum Connected {
    Ok,
    /// The share wants (other) credentials.
    NeedsCredentials(String),
    Failed(String),
}

/// Connect a share. Without credentials nothing is typed for it, so a share
/// that asks for a password reports `NeedsCredentials`.
pub fn connect(url: &str, creds: Option<&Credentials>) -> Connected {
    let mut cmd = gio();
    cmd.arg("mount");
    if creds.is_some_and(|c| c.anonymous) {
        cmd.arg("-a");
    }
    cmd.arg(url);
    // SMB asks for user, domain, password; the others only a password (put
    // the user in the URL: sftp://user@host). Without credentials stdin is
    // empty, so a prompt ends at once and is reported as NeedsCredentials.
    let answers = match creds.filter(|c| !c.anonymous) {
        Some(c) if url.starts_with("smb://") => format!("{}\n{}\n{}\n", c.user, c.domain, c.password),
        Some(c) => format!("{}\n", c.password),
        None => String::new(),
    };
    let out = match super::sys::output_within(&mut cmd, Some(answers.as_bytes()), MOUNT_LIMIT) {
        Ok(o) => o,
        Err(e) if e.starts_with("could not run") => return Connected::Failed(format!("Could not run gio (install the GLib tools and gvfs-backends): {e}")),
        Err(e) => return Connected::Failed(e),
    };
    if out.status.success() {
        return Connected::Ok;
    }
    let msg = failure(&out);
    let lower = msg.to_ascii_lowercase();
    let all = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase() + &String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    if lower.contains("already mounted") {
        Connected::Ok
    } else if creds.is_none_or(|c| c.anonymous) && (all.contains("password") || all.contains("authentication") || lower.contains("permission denied")) {
        Connected::NeedsCredentials(msg)
    } else {
        Connected::Failed(msg)
    }
}

/// Where GVFS mounted `url` (its local path), once it is connected.
pub fn local_path(url: &str) -> Option<Vec<u8>> {
    let out = super::sys::output_within(gio().args(["info", "-a", "standard::name", url]), None, QUERY_LIMIT).ok()?;
    let text = out.stdout;
    text.split(|&b| b == b'\n').find_map(|l| l.strip_prefix(b"local path: ")).map(|p| p.trim_ascii().to_vec())
}

/// Disconnect a GVFS share (by its local path or URL), or unmount a kernel
/// network mount the user may unmount (`user` in fstab).
pub fn disconnect(path: &[u8], gvfs: bool) -> Result<(), String> {
    let os = super::sys::os(path);
    let out = if gvfs { super::sys::output_within(gio().arg("mount").arg("-u").arg(os), None, QUERY_LIMIT) } else { super::sys::output_within(Command::new("umount").arg(os).env("LC_ALL", "C"), None, QUERY_LIMIT) };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(failure(&o)),
        Err(e) => Err(e),
    }
}

/// The shared folders a server offers (`smb://server`), through GVFS's
/// network browsing.
pub fn shares(server: &str) -> Result<Vec<String>, String> {
    let out = super::sys::output_within(gio().args(["list", &format!("{server}/")]), None, QUERY_LIMIT)?;
    if !out.status.success() {
        return Err(failure(&out));
    }
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|l| !l.is_empty() && !l.ends_with('$')).map(String::from).collect();
    v.sort_by_key(|s| s.to_lowercase());
    Ok(v)
}

fn servers_file() -> std::path::PathBuf {
    session::config_dir().join("servers.txt")
}

/// Servers FEx remembers (opened before), as `scheme://host`.
pub fn load_servers() -> Vec<String> {
    std::fs::read_to_string(servers_file()).unwrap_or_default().lines().map(str::trim).filter(|l| to_url(l).is_some()).map(String::from).collect()
}

pub fn save_servers(servers: &[String]) {
    let f = servers_file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(f, servers.join("\n") + "\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_from_what_users_type() {
        assert_eq!(to_url(r"\\nas\media\films").as_deref(), Some("smb://nas/media/films"));
        assert_eq!(to_url("SMB://nas/media/").as_deref(), Some("smb://nas/media"));
        assert_eq!(to_url("ssh://me@box/home").as_deref(), Some("sftp://me@box/home"));
        assert_eq!(to_url("http://x/y"), None);
        assert_eq!(to_url("/home"), None);
        assert_eq!(server_of("smb://nas/media/x").as_deref(), Some("smb://nas"));
        assert!(is_share("smb://nas/media") && !is_share("smb://nas"));
    }
}
