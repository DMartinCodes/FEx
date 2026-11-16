//! Signed updates from GitHub releases (Windows and Linux).
//!
//! Trust: every release file is signed with minisign (Ed25519 over a
//! BLAKE2b hash) by a key that only the repository's protected `release`
//! environment can use. Builds carry the public keys
//! (`packaging/release-keys.pub`) and accept nothing else. A signature's
//! trusted comment, itself covered by the signature, names the version and
//! file (`fex 0.2.0 fex-linux-x64`), so one release file can't stand in for
//! another, and only a version newer than the installed one is accepted, so
//! an older, validly signed release can't be replayed as a downgrade.
//!
//! Flow: the app, as the user, learns the latest version from the latest
//! release's small signature file (a hint only), then downloads that
//! version's binary and signature and verifies them. Installing needs an
//! administrator (UAC, pkexec or sudo). The *installed* binary, in a folder
//! only administrators can write, reads the download into memory, verifies
//! it again with its own keys and writes those same bytes into place, so
//! nothing the user could have changed is trusted between check and
//! install. It restarts the index service (which saves its index when it
//! stops and loads it when it starts, so nothing is re-indexed) and rolls
//! back if the new service doesn't answer.

use std::path::{Path, PathBuf};

use minisign_verify::{PublicKey, Signature};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{apply, elevate_apply, relaunch};
#[cfg(windows)]
pub use windows::{apply, elevate_apply, install_copy, relaunch};

pub const REPO: &str = "DMartinCodes/FEx";
const KEYS: &str = include_str!("../../packaging/release-keys.pub");
const MAX_BINARY: usize = 128 << 20;
const MAX_SIG: usize = 4096;

/// This platform's release binary.
#[cfg(windows)]
pub const ASSET: &str = "fex-windows-x64.exe";
#[cfg(not(windows))]
pub const ASSET: &str = "fex-linux-x64";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// `1.2.3` or `v1.2.3`; nothing else.
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.strip_prefix('v').unwrap_or(s).split('.');
        let mut part = || it.next().filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?.parse().ok();
        let v = Version(part()?, part()?, part()?);
        it.next().is_none().then_some(v)
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is X.Y.Z")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn keys_from(text: &str) -> Vec<PublicKey> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("untrusted comment:"))
        .filter_map(|l| PublicKey::from_base64(l).ok())
        .collect()
}

/// The release keys this build trusts.
pub fn trusted_keys() -> Vec<PublicKey> {
    keys_from(KEYS)
}

/// What a signature says it signs, from its trusted comment.
fn signed_fields(sig: &Signature) -> Option<(Version, &str)> {
    let mut it = sig.trusted_comment().split(' ');
    (it.next()? == "fex").then_some(())?;
    let v = Version::parse(it.next()?)?;
    let file = it.next()?;
    it.next().is_none().then_some((v, file))
}

/// Verify `bin` against `sig_text` with any of `keys`; the signature must
/// name `file`. Returns the signed version.
pub fn verify_with(keys: &[PublicKey], bin: &[u8], sig_text: &str, file: &str) -> Result<Version, String> {
    if keys.is_empty() {
        return Err("this build has no release key, so it can't verify updates".into());
    }
    let sig = Signature::decode(sig_text).map_err(|e| format!("unreadable signature: {e}"))?;
    // Prehashed signatures only (allow_legacy = false).
    if !keys.iter().any(|k| k.verify(bin, &sig, false).is_ok()) {
        return Err("the signature doesn't match: not signed by a FEx release key, or the file was altered".into());
    }
    let (v, signed) = signed_fields(&sig).ok_or("the signature doesn't name a FEx release")?;
    if signed != file {
        return Err(format!("the signature is for {signed}, not {file}"));
    }
    Ok(v)
}

pub fn verify(bin: &[u8], sig_text: &str, file: &str) -> Result<Version, String> {
    verify_with(&trusted_keys(), bin, sig_text, file)
}

fn latest_url(file: &str) -> String {
    format!("https://github.com/{REPO}/releases/latest/download/{file}")
}

fn release_url(v: Version, file: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{v}/{file}")
}

/// The newest release if it's newer than this build, read from the latest
/// release's signature file. Only a hint: `download` verifies everything.
pub fn check() -> Result<Option<Version>, String> {
    let sig = fetch(&latest_url(&format!("{ASSET}.minisig")), MAX_SIG)?;
    let text = std::str::from_utf8(&sig).map_err(|_| "the release signature isn't text")?;
    let sig = Signature::decode(text).map_err(|e| format!("unreadable release signature: {e}"))?;
    let (v, _) = signed_fields(&sig).ok_or("the latest release's signature doesn't name a version")?;
    Ok((v > Version::current()).then_some(v))
}

/// A verified download in the user's update folder.
#[derive(Clone, Debug)]
pub struct Download {
    pub bin: PathBuf,
    pub sig: PathBuf,
}

/// Download release `v` and verify it.
pub fn download(v: Version) -> Result<Download, String> {
    let bin = fetch(&release_url(v, ASSET), MAX_BINARY)?;
    let sig = fetch(&release_url(v, &format!("{ASSET}.minisig")), MAX_SIG)?;
    let sig = String::from_utf8(sig).map_err(|_| "the release signature isn't text")?;
    let signed = verify(&bin, &sig, ASSET)?;
    if signed != v {
        return Err(format!("the release for {v} is signed as {signed}"));
    }
    if signed <= Version::current() {
        return Err(format!("{signed} isn't newer than this FEx ({})", Version::current()));
    }
    let dir = download_dir()?;
    let (bin_path, sig_path) = (dir.join(ASSET), dir.join(format!("{ASSET}.minisig")));
    std::fs::write(&bin_path, &bin).and_then(|()| std::fs::write(&sig_path, sig.as_bytes())).map_err(|e| format!("cannot save the download in {}: {e}", dir.display()))?;
    Ok(Download { bin: bin_path, sig: sig_path })
}

fn read_limited(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut out).map_err(|e| format!("{}: {e}", path.display()))?;
    if out.len() > max {
        return Err(format!("{} is too large", path.display()));
    }
    Ok(out)
}

/// The installer's check: read a download into memory and verify it with
/// this (installed) binary's keys. The caller writes exactly these bytes.
pub fn verified_bytes(bin: &Path, sig: &Path) -> Result<(Vec<u8>, Version), String> {
    let bytes = read_limited(bin, MAX_BINARY)?;
    let sig = String::from_utf8(read_limited(sig, MAX_SIG)?).map_err(|_| "the signature isn't text")?;
    let v = verify(&bytes, &sig, ASSET)?;
    if v <= Version::current() {
        return Err(format!("{v} isn't newer than the installed FEx ({}); not installing", Version::current()));
    }
    Ok((bytes, v))
}

/// A folder of the user's for downloads (the installer trusts nothing in it).
fn download_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let dir = std::env::temp_dir().join("FEx-update");
    #[cfg(not(windows))]
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("fex-update-{}", unsafe { libc::geteuid() }));
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
        // In a shared /tmp, someone else may have made it first.
        let m = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
            return Err(format!("{} isn't a folder of yours", dir.display()));
        }
    }
    #[cfg(windows)]
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Update checks are off for this machine or user: `FEX_NO_UPDATE_CHECK`
/// set, or the machine policy (Windows: `HKLM\SOFTWARE\Policies\FEx`
/// `DisableUpdateCheck` = 1; Linux: `/etc/fex/no-update-check` exists).
pub fn checks_disabled() -> bool {
    if std::env::var_os("FEX_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    #[cfg(windows)]
    {
        windows::policy_disables_checks()
    }
    #[cfg(not(windows))]
    {
        Path::new("/etc/fex/no-update-check").exists()
    }
}

// ------------------------------------------------------------ HTTP

/// GET an https URL (following redirects), at most `max` bytes. Uses the
/// system's TLS, certificates and proxy settings: curl on Linux.
#[cfg(not(windows))]
fn fetch(url: &str, max: usize) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--tlsv1.2", "--connect-timeout", "20", "--max-time", "600", "--max-filesize"])
        .arg(max.to_string())
        .arg(url)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(match out.status.code() {
            Some(22) => format!("not found: {url}"),
            _ => format!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        });
    }
    if out.stdout.len() > max {
        return Err("download too large".into());
    }
    Ok(out.stdout)
}

#[cfg(windows)]
use windows::fetch;

// ------------------------------------------------------------ GUI support

/// What the app shows about updates.
#[derive(Clone, Debug, Default)]
pub enum State {
    #[default]
    Quiet,
    Available(Version),
    Working(String),
    /// Installed; FEx restarts into it on request.
    Installed(Version),
    /// The version that failed to install, and why.
    Failed(Version, String),
}

/// Background checks (at start, then daily) and installs, for the app.
#[derive(Clone, Default)]
pub struct Updater {
    state: std::sync::Arc<std::sync::Mutex<State>>,
}

impl Updater {
    /// Start checking unless policy turns checks off; `wake` repaints.
    pub fn start(wake: impl Fn() + Send + 'static) -> Updater {
        let u = Updater::default();
        if checks_disabled() {
            return u;
        }
        let state = u.state.clone();
        std::thread::Builder::new()
            .name("fex-update-check".into())
            .spawn(move || {
                loop {
                    if let Ok(Some(v)) = check() {
                        let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if matches!(*s, State::Quiet | State::Available(_)) {
                            *s = State::Available(v);
                            wake();
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
                }
            })
            .ok();
        u
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = s;
    }

    /// Download, verify and install `v` (the system asks for an
    /// administrator), restarting the index service. `force` also updates
    /// while the service is building its first index (restarting that).
    pub fn install(&self, v: Version, force: bool, wake: impl Fn() + Send + 'static) {
        self.set(State::Working(format!("Downloading FEx {v}…")));
        let me = self.clone();
        std::thread::spawn(move || {
            let result = download(v).and_then(|d| {
                me.set(State::Working(format!("Installing FEx {v}…")));
                wake();
                elevate_apply(&d, force)
            });
            me.set(match result {
                Ok(()) => State::Installed(v),
                Err(e) => State::Failed(v, e),
            });
            wake();
        });
    }

    pub fn dismiss(&self) {
        self.set(State::Quiet);
    }
}

/// The status bar's update item (in a right-to-left layout).
pub fn status_widget(ui: &mut eframe::egui::Ui, u: &Updater) {
    let ctx = ui.ctx().clone();
    let wake = move || ctx.request_repaint();
    match u.state() {
        State::Quiet => {}
        State::Available(v) => {
            if ui
                .small_button(format!("Update to FEx {v}"))
                .on_hover_text(format!("FEx {} is installed. The update is a signed release, verified before it installs; Windows or Linux asks for an administrator. The index service keeps its index.", Version::current()))
                .clicked()
            {
                u.install(v, false, wake);
            }
        }
        State::Working(msg) => {
            ui.weak(msg);
            ui.spinner();
        }
        State::Installed(v) => {
            if ui.small_button(format!("Restart into FEx {v}")).clicked() {
                relaunch();
                ui.ctx().send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            }
        }
        State::Failed(v, e) => {
            if e.contains("first index") {
                if ui.small_button("Update anyway").on_hover_text(e.as_str()).clicked() {
                    u.install(v, true, wake);
                }
                if ui.small_button("Later").clicked() {
                    u.dismiss();
                }
            } else if ui.small_button("Update failed").on_hover_text(format!("{e}\n(click to dismiss)")).clicked() {
                u.dismiss();
            }
        }
    }
}

// ------------------------------------------------------------ CLI

const USAGE: &str = "usage: fex update            check, download, verify and install the latest release
                   (as administrator / root)
       fex update check      show whether a newer release exists
       fex update verify FILE SIG NAME [--key BASE64]
                             verify a release file's signature";

/// `fex update ...`. Returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    let fail = |e: String| {
        eprintln!("fex update: {e}");
        1
    };
    let force = args.iter().any(|a| a == "--force");
    let key = args.iter().position(|a| a == "--key").and_then(|i| args.get(i + 1));
    let mut positional = Vec::new();
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--key" => {
                it.next();
            }
            _ if a.starts_with("--") => {}
            _ => positional.push(a),
        }
    }
    match positional.first().copied() {
        None | Some("install") => {
            let v = match check() {
                Ok(Some(v)) => v,
                Ok(None) => {
                    println!("FEx {} is the latest release.", Version::current());
                    return 0;
                }
                Err(e) => return fail(e),
            };
            println!("Downloading FEx {v}…");
            match download(v).and_then(|d| apply(&d.bin, &d.sig, force)) {
                Ok(msg) => {
                    println!("{msg}");
                    0
                }
                Err(e) => fail(e),
            }
        }
        Some("check") => match check() {
            Ok(Some(v)) => {
                println!("FEx {v} is available (this is {}). Install it with `fex update` as administrator.", Version::current());
                0
            }
            Ok(None) => {
                println!("FEx {} is the latest release.", Version::current());
                0
            }
            Err(e) => fail(e),
        },
        // The elevated half of an app-started update (see `elevate_apply`).
        Some("apply") if positional.len() == 3 => {
            let result = apply(Path::new(positional[1]), Path::new(positional[2]), force);
            #[cfg(windows)]
            windows::report(&result);
            match result {
                Ok(msg) => {
                    println!("{msg}");
                    0
                }
                Err(e) => fail(e),
            }
        }
        Some("verify") if positional.len() == 4 => {
            let keys = match key {
                Some(k) => keys_from(k),
                None => trusted_keys(),
            };
            let (file, sig, name) = (Path::new(positional[1]), Path::new(positional[2]), positional[3]);
            let result = read_limited(file, MAX_BINARY).and_then(|bin| {
                let sig = String::from_utf8(read_limited(sig, MAX_SIG)?).map_err(|_| "the signature isn't text".to_string())?;
                verify_with(&keys, &bin, &sig, name)
            });
            match result {
                Ok(v) => {
                    println!("{}: good signature for {name} {v}", file.display());
                    0
                }
                Err(e) => fail(format!("{}: {e}", file.display())),
            }
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &[u8] = include_bytes!("../../testdata/update/payload");
    const LIN: &str = include_str!("../../testdata/update/payload.lin.minisig");
    const WIN: &str = include_str!("../../testdata/update/payload.win.minisig");
    const OLD: &str = include_str!("../../testdata/update/payload.old.minisig");
    const OTHER: &str = include_str!("../../testdata/update/payload.other.minisig");
    const KEY: &str = include_str!("../../testdata/update/test-key.pub");

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0"), Some(Version(0, 10, 0)));
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        for bad in ["1.2", "1.2.3.4", "1.2.x", "1.2.3-rc1", "", "v", "1..3", "+1.2.3"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        let _ = Version::current();
    }

    #[test]
    fn good_signature_names_version_and_file() {
        let keys = keys_from(KEY);
        assert_eq!(verify_with(&keys, PAYLOAD, LIN, "fex-linux-x64"), Ok(Version(9, 9, 9)));
        assert_eq!(verify_with(&keys, PAYLOAD, WIN, "fex-windows-x64.exe"), Ok(Version(9, 9, 9)));
        assert_eq!(verify_with(&keys, PAYLOAD, OLD, "fex-linux-x64"), Ok(Version(0, 0, 1)));
    }

    #[test]
    fn bad_signatures_are_refused() {
        let keys = keys_from(KEY);
        // Another platform's file.
        assert!(verify_with(&keys, PAYLOAD, WIN, "fex-linux-x64").unwrap_err().contains("is for fex-windows-x64.exe"));
        // Altered content.
        let mut altered = PAYLOAD.to_vec();
        altered[0] ^= 1;
        assert!(verify_with(&keys, &altered, LIN, "fex-linux-x64").is_err());
        // A key nobody trusts.
        assert!(verify_with(&keys, PAYLOAD, OTHER, "fex-linux-x64").is_err());
        // An altered trusted comment.
        let forged = LIN.replace("fex 9.9.9", "fex 9.9.8");
        assert!(verify_with(&keys, PAYLOAD, &forged, "fex-linux-x64").is_err());
        // No keys at all.
        assert!(verify_with(&[], PAYLOAD, LIN, "fex-linux-x64").unwrap_err().contains("no release key"));
        assert!(verify_with(&keys, PAYLOAD, "garbage", "fex-linux-x64").is_err());
    }

    #[test]
    fn installer_refuses_downgrades() {
        // Signed by a key the build doesn't trust: refused before anything.
        let dir = std::env::temp_dir().join(format!("fex-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, sig) = (dir.join("bin"), dir.join("sig"));
        std::fs::write(&bin, PAYLOAD).unwrap();
        std::fs::write(&sig, OLD).unwrap();
        assert!(verified_bytes(&bin, &sig).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_keys_file_parses() {
        // Every non-comment line must be a valid key: a typo would silently
        // leave a build unable to verify updates.
        for line in KEYS.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("untrusted comment:")) {
            assert!(PublicKey::from_base64(line).is_ok(), "bad key line: {line}");
        }
    }
}
