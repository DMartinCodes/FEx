//! The Windows index service (`fex service ...`): the counterpart of the
//! Linux index daemon. It keeps every fixed drive's index ready and current
//! (saved indexes, the NTFS change journal, the index's own watcher), runs
//! background work in background mode with a CPU reserve, and answers the
//! app over a named pipe with per-user filtering.
//!
//! ```text
//! fex service install     install and start (administrator)
//! fex service uninstall   stop and remove (administrator)
//! fex service debug       run in this console (administrator); Ctrl+C ends it unsaved
//! fex service status      what the running service is doing
//! fex service query TEXT  search through the service
//! fex service profile     measure this machine and show the CPU reserve
//! fex service log         the end of the service's log
//! fex service run         the service's own entry point (started by Windows)
//! ```

mod access;
mod profile;
pub mod proto;
mod scm;
mod server;
mod throttle;
mod usn;

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Drive roots of fixed disks (`C:\`, ...).
fn fixed_drives() -> Vec<String> {
    const DRIVE_FIXED: u32 = 3;
    crate::win::drives()
        .into_iter()
        .filter(|d| unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(crate::win::wide(d, "").as_ptr()) } == DRIVE_FIXED)
        .collect()
}

fn config() -> server::Config {
    let data = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let reserve = std::env::var("FEX_RESERVE").ok().and_then(|v| v.parse::<f64>().ok()).map(|p| (p / 100.0).clamp(0.0, 0.5));
    server::Config { state_dir: data.join("FEx").join("Service"), roots: fixed_drives(), reserve, save_every: Duration::from_secs(600) }
}

/// Release builds have no console of their own: print to the one we were
/// started from.
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Run `fex service <args>`; returns the process exit code.
/// Panics on any service thread go to the service log (there's no console
/// or desktop to show them on), then to the default hook.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        server::log(&format!("panic on thread '{}': {info}\r\n{}", thread.name().unwrap_or("<unnamed>"), std::backtrace::Backtrace::force_capture()));
        default(info);
    }));
}

pub fn cli(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("run") {
        attach_console();
    }
    if matches!(args.first().map(String::as_str), Some("run" | "debug")) {
        log_panics();
    }
    let fail = |e: String| {
        eprintln!("{e}");
        1
    };
    match args.first().map(String::as_str) {
        Some("run") => scm::dispatch().map_or_else(fail, |()| 0),
        Some("install") => scm::install().map_or_else(fail, |()| {
            println!("{} installed and started.", scm::NAME);
            0
        }),
        Some("uninstall") => scm::uninstall().map_or_else(fail, |()| {
            println!("{} stopped and removed.", scm::NAME);
            0
        }),
        Some("debug") => {
            server::run(config(), || println!("listening on {}", proto::PIPE));
            0
        }
        Some("log") => {
            let path = server::log_path(&config().state_dir);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let lines: Vec<&str> = text.lines().collect();
                    for l in &lines[lines.len().saturating_sub(60)..] {
                        println!("{l}");
                    }
                    0
                }
                Err(e) => fail(format!("{}: {e}", path.display())),
            }
        }
        Some("profile") => {
            let p = profile::measure(&fixed_drives(), None);
            println!("{}", p.describe());
            0
        }
        Some("status") => match proto::status() {
            Ok(st) => {
                let age = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |n| n.as_secs().saturating_sub(st.verified_ns / 1_000_000_000));
                println!("drives:    {} (ready: {})", st.roots.join(" "), st.ready.join(" "));
                println!("index:     {} entries", st.entries);
                println!("verified:  {age} s ago");
                println!("activity:  {}{}", st.activity, if st.paused.is_empty() { String::new() } else { format!(" (paused: {})", st.paused) });
                println!("cpu:       {}", st.reserve);
                println!("log:       {}", server::log_path(&config().state_dir).display());
                0
            }
            Err(e) => fail(format!("the index service isn't running: {e}")),
        },
        Some("query") => {
            let text = args[1..].join(" ");
            let scope = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| r"C:\".into());
            let t = Instant::now();
            match proto::query(&scope, &text, false, proto::MAX_LIMIT, proto::NAME_ASC, 200, 0) {
                Ok(proto::Reply::Rows { mut rows, total, mut rest }) => {
                    let first = t.elapsed();
                    let shown: Vec<String> = rows.iter().take(20).map(|r| r.full_path(&scope)).collect();
                    while let Ok(Some(more)) = rest.next() {
                        rows.extend(more);
                    }
                    println!("{total} matches · first screen in {:.1} ms · all {} rows in {:.1} ms", first.as_secs_f64() * 1e3, rows.len(), t.elapsed().as_secs_f64() * 1e3);
                    for s in shown {
                        println!("{s}");
                    }
                    0
                }
                Ok(proto::Reply::NotCovered(roots)) => fail(format!("the service indexes {}, not this folder", roots.join(" "))),
                Ok(proto::Reply::Busy(m) | proto::Reply::Error(m)) => fail(m),
                Err(e) => fail(format!("the index service isn't running: {e}")),
            }
        }
        _ => fail("usage: fex service install | uninstall | status | query TEXT | log | profile | debug | run".into()),
    }
}
