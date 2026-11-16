//! FEx — File Explorer expert.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod archive;
#[cfg(windows)]
mod clip;
#[cfg(windows)]
mod fileop;
#[cfg(windows)]
mod fsys;
#[cfg(windows)]
mod history;
#[cfg(windows)]
mod index;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod oledrag;
#[cfg(windows)]
mod oledrop;
#[cfg(windows)]
mod ops;
mod memory;
mod pool;
#[cfg(windows)]
mod props;
#[cfg(windows)]
mod recent;
#[cfg(windows)]
mod recyclebin;
mod reserve;
#[cfg(windows)]
mod security;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod shellnew;
mod typeahead;
#[cfg(windows)]
mod walk;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod winsvc;
mod wire;

// Linux port prototype: its own engine and UI. Shares only the portable
// modules above that are not gated to Windows.
#[cfg(target_os = "linux")]
mod linux;

#[cfg(windows)]
use eframe::egui;

#[cfg(windows)]
/// Release builds have no console, so a panic would otherwise vanish without
/// a trace. Log it to `%LOCALAPPDATA%\FEx\crash.log` and tell the user.
fn install_crash_reporter() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info); // still print to stderr when there is a console
        let thread = std::thread::current();
        let report = format!(
            "FEx {} panicked on thread '{}': {info}\n\n{}\n\n",
            env!("CARGO_PKG_VERSION"),
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture()
        );
        let dir = std::env::var("LOCALAPPDATA").map(|d| format!(r"{d}\FEx")).unwrap_or_else(|_| ".".into());
        let log = format!(r"{dir}\crash.log");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, report.as_bytes()));
        // Worker-thread panics are contained (see ops.rs); only the UI thread is fatal.
        if thread.name() == Some("main") {
            win::message_box("FEx crashed", &format!("{info}\n\nDetails were saved to:\n{log}"));
        }
    }));
}

#[cfg(windows)]
/// Where a native crash's minidump goes, as NUL-terminated UTF-16. Prepared
/// up front: after an access violation, allocating may no longer work.
static DUMP_PATH: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();

#[cfg(windows)]
/// Native crashes (an access violation in unsafe code, a fault inside a
/// Windows API) never reach the panic hook. Write a minidump to
/// `%LOCALAPPDATA%\FEx\crash.dmp`, then let Windows end the process as usual.
fn install_dump_writer() {
    use windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
    let Ok(dir) = std::env::var("LOCALAPPDATA").map(|d| format!(r"{d}\FEx")) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let _ = DUMP_PATH.set(win::wide(&format!(r"{dir}\crash.dmp"), ""));
    unsafe { SetUnhandledExceptionFilter(Some(write_dump)) };
}

#[cfg(windows)]
unsafe extern "system" fn write_dump(info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MiniDumpNormal, MiniDumpWithIndirectlyReferencedMemory, MiniDumpWriteDump, MINIDUMP_EXCEPTION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    let Some(path) = DUMP_PATH.get() else { return EXCEPTION_CONTINUE_SEARCH };
    unsafe {
        let file = CreateFileW(path.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if file != INVALID_HANDLE_VALUE {
            let exception = MINIDUMP_EXCEPTION_INFORMATION { ThreadId: GetCurrentThreadId(), ExceptionPointers: info as *mut _, ClientPointers: 0 };
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file,
                MiniDumpNormal | MiniDumpWithIndirectlyReferencedMemory,
                &exception,
                std::ptr::null(),
                std::ptr::null(),
            );
            CloseHandle(file);
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

#[cfg(windows)]
/// The title bar and taskbar icon (generated from `assets/fex_icon.png` by build.rs).
fn window_icon() -> egui::IconData {
    const SIDE: u32 = 64;
    egui::IconData { rgba: include_bytes!(concat!(env!("OUT_DIR"), "/icon_64.rgba")).to_vec(), width: SIDE, height: SIDE }
}

#[cfg(windows)]
/// Command line: `fex [location] [--at x,y] [--size w,h]`. The options place
/// a window opened by tearing off a tab where the tab was dropped.
struct Args {
    start: Option<String>,
    at: Option<[f32; 2]>,
    size: Option<[f32; 2]>,
}

#[cfg(windows)]
fn parse_args() -> Args {
    let pair = |v: Option<String>| {
        let v = v?;
        let (a, b) = v.split_once(',')?;
        Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
    };
    let mut args = Args { start: None, at: None, size: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--at" => args.at = pair(it.next()),
            "--size" => args.size = pair(it.next()),
            _ => args.start = Some(arg),
        }
    }
    args
}

#[cfg(windows)]
fn main() -> eframe::Result {
    // `fex service ...`: the index service and its tools, no window.
    if std::env::args().nth(1).as_deref() == Some("service") {
        // Native crashes leave a minidump; panics are logged by winsvc.
        install_dump_writer();
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(winsvc::cli(&args));
    }
    install_crash_reporter();
    install_dump_writer();
    let args = parse_args();
    let session = session::Session::load().unwrap_or_default();
    // Without a location, carry on where the last window left off.
    let restore = args.start.is_none();
    let size = args.size.or(session.size).unwrap_or([1100.0, 700.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("FEx")
        .with_inner_size(size)
        .with_min_inner_size([520.0, 320.0])
        .with_icon(window_icon())
        // FEx installs its own OLE drop target (oledrop.rs), which also
        // takes virtual files; winit's would claim the window first.
        .with_drag_and_drop(false);
    let pos = args.at.or(session.pos.filter(|_| restore)).filter(|&[x, y]| win::on_screen(x, y));
    if let Some(pos) = pos {
        viewport = viewport.with_position(pos);
    }
    if restore && session.maximized {
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Ctrl+C/X/V operate on files; app.rs re-translates them for text fields.
        clipboard_shortcuts: false,
        ..Default::default()
    };
    let result = eframe::run_native(
        "FEx",
        options,
        Box::new(move |cc| Ok(Box::new(app::FexApp::new(cc, args.start, &session, restore)))),
    );
    // Release builds have no console: without this, a failure to start (no
    // OpenGL 2+ driver, as in some VMs and remote sessions) would be silent.
    if let Err(e) = &result {
        win::message_box("FEx could not start", &format!("{e}\n\nFEx draws with OpenGL; make sure a graphics driver is installed."));
    }
    result
}


#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}
