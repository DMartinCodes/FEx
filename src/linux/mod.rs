//! Linux port prototype.
//!
//! Its own engine (getdents64/statx, a names-only index, inotify) and a
//! reduced UI on X11, sharing the platform-neutral modules (`pool`,
//! `typeahead`) with the Windows build. Nothing here is compiled on Windows.
//!
//! `fex --bench <folder> [query ...]` measures the engine without a window.

mod app;
mod daemon;
mod fanotify;
mod fsys;
mod index;
mod profile;
mod proto;
mod snapshot;
mod sys;
mod throttle;
mod walk;
mod watch;

use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use eframe::egui;

const USAGE: &str = "usage: fex [folder]
       fex daemon [--root /] [--state-dir /var/lib/fex] [--socket PATH] [--workers N]
                  [--sweep-every SECS] [--save-every SECS] [--io-limit PCT] [--reserve PCT]
                  [--no-fanotify] [--include-network]
       fex query [--regex] [--scope DIR] [--limit N] [--show N] <text>
       fex status
       fex profile [--root /] [--state-dir /var/lib/fex] [--save]
       fex --bench <folder> [--runs N] [query ...]
           query: keywords (globs with * ?, type:ext), or re:<regex>
       fex --bench-interference <folder> [--secs N]
           (a CPU-bound job on every core, alone and beside crawlers at normal
           and idle priority)
       fex --bench-snapshot <folder> [--snapshot FILE] [--runs N]
           (as root, also measures cold-cache times by dropping the page cache)
       A folder named like a subcommand opens as ./daemon etc.";

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
        Some("--bench-snapshot") => bench_snapshot(&args[1..]),
        Some("--bench-interference") => bench_interference(&args[1..]),
        Some("daemon") => cli_daemon(&args[1..]),
        Some("query") => cli_query(&args[1..]),
        Some("status") => cli_status(),
        Some("profile") => cli_profile(&args[1..]),
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

// ------------------------------------------------------------ daemon CLI

fn arg_value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn cli_daemon(args: &[String]) -> ExitCode {
    let mut cfg = daemon::Config {
        root: b"/".to_vec(),
        state_dir: std::path::PathBuf::from("/var/lib/fex"),
        socket: proto::socket_path(),
        sweep_every: Duration::from_secs(1800),
        save_every: Duration::from_secs(600),
        fanotify: true,
        include_network: false,
        workers: walk::cores(),
        io_limit: 10.0,
        reserve: None,
    };
    let mut it = args.iter();
    let parsed: Result<(), String> = (|| {
        while let Some(a) = it.next() {
            let secs = |v: &String| v.parse::<u64>().map(Duration::from_secs).map_err(|_| format!("{a}: expected seconds"));
            match a.as_str() {
                "--root" => cfg.root = fsys::resolve_input(arg_value(&mut it, a)?, b"/"),
                "--state-dir" => cfg.state_dir = arg_value(&mut it, a)?.into(),
                "--socket" => cfg.socket = arg_value(&mut it, a)?.into(),
                "--sweep-every" => cfg.sweep_every = secs(arg_value(&mut it, a)?)?,
                "--save-every" => cfg.save_every = secs(arg_value(&mut it, a)?)?,
                "--workers" => cfg.workers = arg_value(&mut it, a)?.parse().map_err(|_| "--workers: expected a number")?,
                "--io-limit" => cfg.io_limit = arg_value(&mut it, a)?.parse().map_err(|_| "--io-limit: expected a percentage")?,
                "--reserve" => {
                    let pct: f64 = arg_value(&mut it, a)?.parse().map_err(|_| "--reserve: expected a percentage")?;
                    cfg.reserve = Some((pct / 100.0).clamp(0.0, 0.5));
                }
                "--no-fanotify" => cfg.fanotify = false,
                "--include-network" => cfg.include_network = true,
                other => return Err(format!("unknown option {other}")),
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        eprintln!("{e}\n{USAGE}");
        return ExitCode::FAILURE;
    }
    daemon::run(cfg)
}

/// Rows in the first chunk of a streamed reply: a tall window's worth.
const SCREENFUL: u32 = 200;

fn cli_query(args: &[String]) -> ExitCode {
    let (mut regex, mut limit, mut show) = (false, proto::MAX_LIMIT, 20usize);
    let mut scope = None;
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--regex" => regex = true,
            "--scope" => scope = it.next().cloned(),
            "--limit" => limit = it.next().and_then(|v| v.parse().ok()).unwrap_or(limit),
            "--show" => show = it.next().and_then(|v| v.parse().ok()).unwrap_or(show),
            w => words.push(w.to_string()),
        }
    }
    let cwd = std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or_else(|_| b"/".to_vec());
    let scope = scope.map_or(b"/".to_vec(), |s| fsys::resolve_input(&s, &cwd));
    let t = Instant::now();
    match proto::query(&scope, &words.join(" "), regex, limit, proto::NAME_ASC, SCREENFUL) {
        Err(e) => {
            eprintln!("no index daemon at {}: {e}", proto::socket_path().display());
            ExitCode::FAILURE
        }
        Ok(proto::Reply::Rows { mut rows, total, rest }) => {
            let first = t.elapsed();
            let shown: Vec<fsys::Row> = rows.iter().take(show).cloned().collect();
            if let Err(e) = rest.collect_into(&mut rows) {
                eprintln!("reply cut short: {e}");
            }
            let all = t.elapsed();
            let t = Instant::now();
            fsys::fill_meta(&mut rows);
            let meta = t.elapsed();
            println!(
                "{total} matches · first screen in {} · all {} rows in {} · metadata {}",
                ms(first),
                rows.len(),
                ms(all),
                ms(meta)
            );
            for r in &shown {
                println!("{}", String::from_utf8_lossy(&r.full_path(b"")));
            }
            ExitCode::SUCCESS
        }
        Ok(proto::Reply::NotCovered(root)) => {
            eprintln!("the daemon indexes {}, which doesn't cover that scope", String::from_utf8_lossy(&root));
            ExitCode::FAILURE
        }
        Ok(proto::Reply::Busy(m) | proto::Reply::Error(m)) => {
            eprintln!("{m}");
            ExitCode::FAILURE
        }
    }
}

fn cli_status() -> ExitCode {
    match proto::status() {
        Err(e) => {
            eprintln!("no index daemon at {}: {e}", proto::socket_path().display());
            ExitCode::FAILURE
        }
        Ok(st) => {
            let age = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|n| n.as_secs().saturating_sub(st.verified_ns / 1_000_000_000))
                .unwrap_or(0);
            println!("root:      {}", String::from_utf8_lossy(&st.root));
            println!("index:     {}", if st.ready { format!("{} entries, {} folders (generation {})", st.entries, st.dirs, st.generation) } else { "being built".into() });
            println!("memory:    {:.1} MB mapped snapshot + {:.1} MB heap", st.mapped_bytes as f64 / 1048576.0, st.heap_bytes as f64 / 1048576.0);
            println!("verified:  {age} s ago");
            println!("watching:  {}", if st.watched > 0 { format!("{} filesystem(s) with fanotify", st.watched) } else { "periodic sweeps only".into() });
            println!("activity:  {}{}", st.activity, if st.paused.is_empty() { String::new() } else { format!(" (paused: {})", st.paused) });
            if !st.reserve.is_empty() {
                println!("cpu:       {}", st.reserve);
            }
            ExitCode::SUCCESS
        }
    }
}

/// Measure this machine and show (and with --save, store) the reserve the
/// daemon would use. Run at install; the daemon also does it on first start.
fn cli_profile(args: &[String]) -> ExitCode {
    let (mut root, mut state, mut save) = (b"/".to_vec(), std::path::PathBuf::from("/var/lib/fex"), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => root = it.next().map_or(root, |r| fsys::resolve_input(r, b"/")),
            "--state-dir" => state = it.next().map_or(state, std::path::PathBuf::from),
            "--save" => save = true,
            other => {
                eprintln!("unknown option {other}\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    let skip: std::collections::HashSet<Vec<u8>> = sys::crawl_skips();
    let p = profile::measure(&root, &skip, None);
    println!("{}", p.describe());
    if save {
        let _ = std::fs::create_dir_all(&state);
        match p.save(&state.join("profile")) {
            Ok(()) => println!("saved to {}", state.join("profile").display()),
            Err(e) => {
                eprintln!("{}: {e}", state.join("profile").display());
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

// ------------------------------------------------------------ snapshot benchmark

/// Drop the kernel's page, dentry and inode caches (root only).
fn drop_caches() -> bool {
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:").map(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())).flatten())
        .map_or(0.0, |kb| kb / 1024.0)
}

/// The questions behind the daemon: how long until the first search
/// result, from a crawl versus from a snapshot, warm and cold; and what
/// verifying a snapshot (the ctime sweep) costs.
fn bench_snapshot(args: &[String]) -> ExitCode {
    let mut runs = 3usize;
    let mut root = None;
    let mut snap = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--runs" => runs = it.next().and_then(|n| n.parse().ok()).filter(|&n| n > 0).unwrap_or(runs),
            "--snapshot" => snap = it.next().map(std::path::PathBuf::from),
            _ => root = Some(a.clone()),
        }
    }
    let Some(root) = root else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let cwd = std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or_else(|_| b"/".to_vec());
    let root = fsys::resolve_input(&root, &cwd);
    let snap = snap.unwrap_or_else(|| std::env::temp_dir().join(format!("fex-bench-{}.snap", std::process::id())));
    let m = index::Matcher::new("conf", false).unwrap().unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let cores = walk::cores();
    let first_result = |idx: &index::Index| {
        let t = Instant::now();
        let (mut rows, total) = idx.query(&m, &root, 250_000, &|| false).unwrap();
        let q = t.elapsed();
        let t = Instant::now();
        fsys::fill_meta(&mut rows);
        (q, t.elapsed(), total)
    };
    println!("FEx snapshot benchmark: {} · {cores} threads · median of {runs}", String::from_utf8_lossy(&root));

    let rss0 = rss_mb();
    let t = Instant::now();
    let built = index::Build::new(&root, false).run().unwrap();
    let crawl_warmup = t.elapsed();
    let rss_built = rss_mb();
    let t = Instant::now();
    if let Err(e) = built.save(&snap) {
        eprintln!("{}: {e}", snap.display());
        return ExitCode::FAILURE;
    }
    let save = t.elapsed();
    let size = std::fs::metadata(&snap).map_or(0, |m| m.len());
    println!(
        "index: {} entries, {} folders · heap {:.1} MB (RSS +{:.1} MB) · snapshot {:.1} MB written in {}",
        built.entries,
        built.dirs,
        built.bytes() as f64 / 1048576.0,
        rss_built - rss0,
        size as f64 / 1048576.0,
        ms(save)
    );
    drop(built);
    let _ = crawl_warmup;

    let can_drop = drop_caches();
    let modes: &[bool] = if can_drop { &[false, true] } else { &[false] };
    println!("\n{:<8} {:<34} {:>12} {:>10} {:>12}", "cache", "path to first result", "ready", "query", "+ metadata");
    for &cold in modes {
        let label = if cold { "cold" } else { "warm" };
        let (mut crawl_t, mut load_t, mut q1, mut q2, mut f1, mut f2) = (vec![], vec![], vec![], vec![], vec![], vec![]);
        for _ in 0..runs {
            if cold {
                drop_caches();
            }
            let t = Instant::now();
            let idx = index::Build::new(&root, false).run().unwrap();
            crawl_t.push(t.elapsed());
            let (q, f, _) = first_result(&idx);
            q1.push(q);
            f1.push(f);
            drop(idx);
            if cold {
                drop_caches();
            }
            let t = Instant::now();
            let idx = index::Index::load(&snap).unwrap();
            load_t.push(t.elapsed());
            let (q, f, _) = first_result(&idx);
            q2.push(q);
            f2.push(f);
        }
        println!("{label:<8} {:<34} {:>12} {:>10} {:>12}", "crawl, then search", ms(median(crawl_t)), ms(median(q1)), ms(median(f1)));
        println!("{label:<8} {:<34} {:>12} {:>10} {:>12}", "map snapshot, then search", ms(median(load_t)), ms(median(q2)), ms(median(f2)));
    }
    let rss_before = rss_mb();
    let idx = index::Index::load(&snap).unwrap();
    println!("\nmapped snapshot: RSS +{:.1} MB, heap {:.1} MB, page cache {:.1} MB", rss_mb() - rss_before, idx.bytes() as f64 / 1048576.0, idx.mapped_bytes() as f64 / 1048576.0);

    println!("\n{:<8} {:<34} {:>12}", "cache", "verify snapshot (ctime sweep)", "time");
    for &cold in modes {
        for workers in [1, cores] {
            let mut times = vec![];
            let mut changed = 0;
            for _ in 0..runs {
                if cold {
                    drop_caches();
                }
                let t = Instant::now();
                changed = idx.sweep(workers, &cancel, &|| {}, &|| {}).len();
                times.push(t.elapsed());
            }
            println!(
                "{:<8} {:<34} {:>12}",
                if cold { "cold" } else { "warm" },
                format!("{} folders, {workers} thread(s), {changed} changed", idx.dirs),
                ms(median(times))
            );
        }
    }
    if !can_drop {
        println!("\n(not root: cold-cache rows skipped)");
    }
    let _ = std::fs::remove_file(&snap);
    ExitCode::SUCCESS
}

// ------------------------------------------------------------ interference benchmark

/// Does background indexing slow real work down? A CPU-bound job runs on
/// every core (normal priority) for a fixed time, alone and with a crawler
/// looping beside it at normal priority and at the daemon's idle priority.
/// The last phase runs the idle crawler alone: an idle machine should give
/// it everything.
fn bench_interference(args: &[String]) -> ExitCode {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
    let mut secs = 8u64;
    let mut root = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(secs),
            _ => root = Some(a.clone()),
        }
    }
    let Some(root) = root else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let cwd = std::env::current_dir().map(|d| d.as_os_str().as_bytes().to_vec()).unwrap_or_else(|_| b"/".to_vec());
    let root = fsys::resolve_input(&root, &cwd);
    let cores = walk::cores();
    let skip = sys::crawl_skips();
    let _ = index::Build::new(&root, false).run(); // warm the caches: this measures CPU contention

    // (job iterations per second, crawler entries per second)
    let (skip, root) = (&skip, &root);
    // crawler: None, or Some(reserve): Some(-1.0) normal priority, Some(0.0)
    // idle priority alone, Some(r) idle priority with an r reserve.
    let phase = |job: bool, crawler: Option<f64>| -> (f64, f64) {
        let (stop, work, entries) = (&AtomicBool::new(false), &AtomicU64::new(0), &AtomicUsize::new(0));
        std::thread::scope(|sc| {
            if job {
                for _ in 0..cores {
                    sc.spawn(move || {
                        let mut x = 0x9E37_79B9_7F4A_7C15u64;
                        while !stop.load(Relaxed) {
                            for _ in 0..100_000 {
                                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407));
                            }
                            work.fetch_add(1, Relaxed);
                        }
                    });
                }
            }
            if let Some(reserve) = crawler {
                sc.spawn(move || {
                    let idle = reserve >= 0.0;
                    let t = throttle::Throttle::start(f64::INFINITY, reserve.max(0.0), cores);
                    let pace = || t.wait();
                    let init: &(dyn Fn() + Sync) = if idle { &throttle::background_thread } else { &|| {} };
                    let pace: &(dyn Fn() + Sync) = if idle { &pace } else { &|| {} };
                    // Progress counts every entry seen, finished crawl or not.
                    while !stop.load(Relaxed) {
                        let c = index::Crawl { workers: cores, cancel: stop, skip, with_meta: false, seen: entries, init, pace };
                        let _ = index::build(root, &c);
                    }
                });
            }
            std::thread::sleep(Duration::from_secs(secs));
            stop.store(true, Relaxed);
        });
        (work.load(Relaxed) as f64 / secs as f64, entries.load(Relaxed) as f64 / secs as f64)
    };
    println!("FEx interference benchmark: crawling {} on {cores} threads, {secs} s per phase, warm cache\n", String::from_utf8_lossy(root));
    let (base, _) = phase(true, None);
    let (_, alone) = phase(false, Some(0.0));
    println!("{:<46} {:>12} {:>16} {:>14}", "phase", "job (rel.)", "crawl M ent./s", "crawl share");
    println!("{:<46} {:>12} {:>16} {:>14}", "job alone", "100.0%", "-", "-");
    for (label, reserve) in [
        ("job + crawler, normal priority", -1.0),
        ("job + crawler, idle priority, no reserve", 0.0),
        ("job + crawler, idle priority, 0.5% reserve", 0.005),
        ("job + crawler, idle priority, 5% reserve", 0.05),
    ] {
        let (job, crawl) = phase(true, Some(reserve));
        println!("{label:<46} {:>11.1}% {:>16.3} {:>13.2}%", job / base * 100.0, crawl / 1e6, crawl / alone * 100.0);
    }
    println!("{:<46} {:>12} {:>16.3} {:>14}", "idle-priority crawler alone", "-", alone / 1e6, "100%");
    ExitCode::SUCCESS
}
