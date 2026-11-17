//! Installing an update on Linux: the installed, root-owned binary replaces
//! itself and restarts the index daemon (`fex-indexd`).

use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::{verified_bytes, Download};
use crate::linux::proto;

const UNIT: &str = "fex-indexd";
const DEFAULT_INSTALL: &str = "/usr/local/bin/fex";

/// Root-owned and writable by root alone (the file and its folder): only
/// then may the copy at `p` install updates, since anyone who could write
/// it could make root run their code.
fn root_only(p: &Path) -> Result<(), String> {
    let check = |p: &Path| -> Result<(), String> {
        let m = fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(format!("{} must be owned by root and writable by root only", p.display()));
        }
        Ok(())
    };
    check(p)?;
    check(p.parent().ok_or("no parent folder")?)
}

/// The installed copy that installs updates: this one if it's a root-only
/// install, else `/usr/local/bin/fex`.
pub fn installed_path() -> Result<PathBuf, String> {
    let me = std::env::current_exe().and_then(fs::canonicalize).map_err(|e| e.to_string())?;
    if root_only(&me).is_ok() {
        return Ok(me);
    }
    let default = PathBuf::from(DEFAULT_INSTALL);
    root_only(&default).map(|()| default).map_err(|_| format!("updates need FEx installed by root (e.g. {DEFAULT_INSTALL}); this copy is {}", me.display()))
}

fn systemctl(args: &[&str]) -> Result<bool, String> {
    // Quiet on machines without systemd (then the daemon isn't managed here).
    Command::new("systemctl").args(args).stderr(std::process::Stdio::null()).status().map(|s| s.success()).map_err(|e| format!("cannot run systemctl: {e}"))
}

/// Wait until the daemon answers a status request.
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

fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Install a downloaded release (as root): verify it, swap the binary
/// atomically, restart the daemon, and roll back if it doesn't answer.
/// Without `force`, refuses while the daemon builds its first index.
pub fn apply(bin: &Path, sig: &Path, force: bool) -> Result<String, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("installing needs root: run `sudo fex update`".into());
    }
    let target = installed_path()?;
    let (bytes, v) = verified_bytes(bin, sig)?;
    let dir = target.parent().ok_or("no install folder")?.to_path_buf();
    let new = dir.join(".fex.new");
    let old = dir.join("fex.old");
    let _ = fs::remove_file(&new);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(&new)?;
        f.write_all(&bytes)?;
        f.sync_all()
    };
    write().map_err(|e| format!("cannot write {}: {e}", new.display()))?;

    let active = systemctl(&["is-active", "--quiet", UNIT]).unwrap_or(false);
    if active && !force && proto::status().is_ok_and(|st| !st.ready) {
        let _ = fs::remove_file(&new);
        return Err("the index daemon is still building its first index; updating now would start it over. Try later, or pass --force".into());
    }
    if active {
        // The daemon saves its index when it stops.
        systemctl(&["stop", UNIT])?;
    }
    // Keep the old binary (a hard link) for rollback; the swap is atomic.
    let _ = fs::remove_file(&old);
    let kept = fs::hard_link(&target, &old).is_ok();
    if let Err(e) = fs::rename(&new, &target) {
        let _ = fs::remove_file(&new);
        if active {
            let _ = systemctl(&["start", UNIT]);
        }
        return Err(format!("cannot replace {}: {e}", target.display()));
    }
    sync_dir(&dir);
    if active {
        let _ = systemctl(&["daemon-reload"]);
        let started = systemctl(&["start", UNIT]).unwrap_or(false) && wait_ready(Duration::from_secs(60));
        if !started {
            if kept && fs::rename(&old, &target).is_ok() {
                sync_dir(&dir);
                let _ = systemctl(&["restart", UNIT]);
                return Err(format!("FEx {v} installed, but the index daemon didn't start; rolled back (see `journalctl -u {UNIT}`)"));
            }
            return Err(format!("FEx {v} installed, but the index daemon didn't start (see `journalctl -u {UNIT}`)"));
        }
    }
    Ok(format!("FEx {v} installed in {}{}", target.display(), if active { "; the index daemon restarted with its saved index" } else { "" }))
}

/// From the app: install a verified download as root (pkexec asks for the
/// password; it runs the installed copy, never the download).
pub fn elevate_apply(d: &Download, force: bool) -> Result<(), String> {
    let installer = installed_path()?;
    let mut cmd = Command::new("pkexec");
    cmd.arg(&installer).arg("update").arg("apply").arg(&d.bin).arg(&d.sig);
    if force {
        cmd.arg("--force");
    }
    let out = cmd.output().map_err(|_| format!("pkexec isn't available; install from a terminal with `sudo {} update`", installer.display()))?;
    match out.status.code() {
        Some(0) => Ok(()),
        // pkexec: 126 = not authorized or dismissed, 127 = no authentication agent.
        Some(126) => Err("not authorized".into()),
        Some(127) => Err(format!("no password prompt is available; install from a terminal with `sudo {} update`", installer.display())),
        // `fex update: <why>` on stderr.
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("fex update: ").to_string()),
    }
}

/// Start the installed FEx and let this process exit.
pub fn relaunch() {
    if let Ok(p) = installed_path().or_else(|_| std::env::current_exe()) {
        let _ = Command::new(p).spawn();
    }
}
