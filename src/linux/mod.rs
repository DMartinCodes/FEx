//! Linux port prototype.
//!
//! Its own engine (getdents64/statx, a names-only index, inotify) and a
//! reduced UI on X11, sharing the platform-neutral modules (`pool`,
//! `typeahead`) with the Windows build. Nothing here is compiled on Windows.
//!
//! `fex --bench <folder> [query ...]` measures the engine without a window.

mod app;
mod fsys;
mod index;
mod sys;
mod walk;
mod watch;

use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use eframe::egui;

const USAGE: &str = "usage: fex [folder]
       fex --bench <folder> [--runs N] [query ...]
           query: keywords (globs with * ?, type:ext), or re:<regex>";

/// Panics go to `$XDG_STATE_HOME/fex/crash.log` (default ~/.local/state).
fn install_crash_reporter() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let dir = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(sys::os(&sys::home())).join(".local/state"))
            .join("fex");
        let report = format!(
            "FEx {} panicked on thread '{}': {info}\n\n{}\n\n",
            env!("CARGO_PKG_VERSION"),
            std::thread::current().name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, report.as_bytes()));
    }));
}

fn window_icon() -> egui::IconData {
    const SIDE: u32 = 64;
    egui::IconData { rgba: include_bytes!(concat!(env!("OUT_DIR"), "/icon_64.rgba")).to_vec(), width: SIDE, height: SIDE }
}

pub fn main() -> ExitCode {
    install_crash_reporter();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--bench") => bench(&args[1..]),
        _ => gui(args.into_iter().next()),
    }
}

/// winit loads its X11 libraries at run time and panics if one is missing.
/// Check first, so the user learns which package to install.
fn preflight() -> Result<(), String> {
    if std::env::var_os("DISPLAY").is_none() {
        return Err("DISPLAY is not set: FEx needs an X11 session (or XWayland).".into());
    }
    let libs = [
        ("libX11.so.6", "libx11-6", "libX11"),
        ("libXcursor.so.1", "libxcursor1", "libXcursor"),
        ("libXrandr.so.2", "libxrandr2", "libXrandr"),
        ("libXi.so.6", "libxi6", "libXi"),
        ("libxkbcommon-x11.so.0", "libxkbcommon-x11-0", "libxkbcommon-x11"),
        ("libGL.so.1", "libgl1", "mesa-libGL"),
    ];
    let missing: Vec<_> = libs
        .iter()
        .filter(|(lib, _, _)| {
            let c = std::ffi::CString::new(*lib).unwrap();
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if !h.is_null() {
                unsafe { libc::dlclose(h) };
            }
            h.is_null()
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let names = |i: usize| missing.iter().map(|m| [m.0, m.1, m.2][i]).collect::<Vec<_>>().join(" ");
    Err(format!(
        "Missing libraries: {}\n  Ubuntu/Debian: sudo apt install {}\n  Rocky/RHEL:    sudo dnf install {}",
        names(0),
        names(1),
        names(2)
    ))
}

fn gui(start: Option<String>) -> ExitCode {
    if let Err(e) = preflight() {
        eprintln!("FEx could not start. {e}");
        return ExitCode::FAILURE;
    }
    let viewport = egui::ViewportBuilder::default()
        .with_title("FEx")
        .with_app_id("fex")
        .with_inner_size([1100.0, 700.0])
        .with_min_inner_size([520.0, 320.0])
        .with_icon(window_icon());
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    match eframe::run_native("FEx", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, start))))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FEx could not start: {e}\nFEx draws with OpenGL through X11; check that DISPLAY is set and a GL driver (Mesa) is installed.");
            ExitCode::FAILURE
        }
    }
}

// ------------------------------------------------------------ benchmark

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

/// Crawl `root` both ways (names only, names plus statx), run queries
/// against the names-only index, fill result metadata, and list the largest
/// folder, printing medians. The first crawl warms the kernel's caches and
/// isn't counted, so numbers are warm-cache figures.
fn bench(args: &[String]) -> ExitCode {
    let mut runs = 3usize;
    let mut root = None;
    let mut queries: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--runs" => runs = it.next().and_then(|n| n.parse().ok()).filter(|&n| n > 0).unwrap_or(runs),
            _ if root.is_none() => root = Some(a.clone()),
            _ => queries.push(a.clone()),
        }
    }
    let Some(root) = root else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let cwd = std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or_else(|_| b"/".to_vec());
    let root = fsys::resolve_input(&root, &cwd);
    if !sys::is_dir(&root) {
        eprintln!("{}: not a folder", String::from_utf8_lossy(&root));
        return ExitCode::FAILURE;
    }
    if queries.is_empty() {
        queries = ["conf", "lib*.so*", "type:h", "zzqx_nothing", r"re:^lib.*\.so\.[0-9]+$"].map(String::from).to_vec();
    }
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!(
        "FEx engine benchmark: {} · {} threads · kernel {} · median of {runs}",
        String::from_utf8_lossy(&root),
        walk::cores(),
        kernel.trim()
    );

    let _ = index::Build::new(&root, false).run(); // warm-up
    let mut names_index = None;
    for with_meta in [false, true] {
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..runs {
            let t = Instant::now();
            last = index::Build::new(&root, with_meta).run();
            times.push(t.elapsed());
        }
        let idx = last.expect("not cancelled");
        let t = median(times);
        println!(
            "crawl {:<16} {:>10}  {} entries, {} dirs, {} unreadable · {:.2} M entries/s · {:.0} MB",
            if with_meta { "names + statx" } else { "names only" },
            ms(t),
            idx.entries,
            idx.dirs,
            idx.errors,
            idx.entries as f64 / t.as_secs_f64() / 1e6,
            idx.bytes() as f64 / 1048576.0
        );
        if !with_meta {
            names_index = Some(idx);
        }
    }
    let idx = names_index.expect("built above");

    println!("\n{:<28} {:>10} {:>10} {:>16}", "query (names-only index)", "matches", "query", "+ metadata");
    for q in &queries {
        let (text, regex) = match q.strip_prefix("re:") {
            Some(r) => (r, true),
            None => (q.as_str(), false),
        };
        let m = match index::Matcher::new(text, regex) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                println!("{q:<28} error: {e}");
                continue;
            }
        };
        let mut times = Vec::new();
        let mut found = (Vec::new(), 0);
        for _ in 0..runs.max(10) {
            let t = Instant::now();
            found = idx.query(&m, &root, 250_000, &|| false).expect("not cancelled");
            times.push(t.elapsed());
        }
        let (mut rows, total) = found;
        let t = Instant::now();
        fsys::fill_meta(&mut rows);
        let fill = t.elapsed();
        println!("{q:<28} {total:>10} {:>10} {:>16}", ms(median(times)), format!("{} ({} rows)", ms(fill), rows.len()));
    }

    if let Some((dir, n)) = idx.largest_dir() {
        let mut times = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            let _ = fsys::list_dir(&dir);
            times.push(t.elapsed());
        }
        println!("\nlargest folder: {} ({n} entries) listed with metadata in {}", String::from_utf8_lossy(&dir), ms(median(times)));
    }
    ExitCode::SUCCESS
}
