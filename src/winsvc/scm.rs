//! Service Control Manager integration: install, uninstall, and the entry
//! point the SCM calls (`fex service run`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfigW, QueryServiceConfigW, QueryServiceStatus, QUERY_SERVICE_CONFIGW, SERVICE_NO_CHANGE, SC_MANAGER_CONNECT, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, RegisterServiceCtrlHandlerExW,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};

use super::server;

pub const NAME: &str = "FExIndex";
const DISPLAY: &str = "FEx Index Service";
const DESCRIPTION: &str = "Keeps a name index of local drives ready for FEx, using idle CPU and background I/O. Each user sees only names in folders they can list.";

static STATUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let handle = STATUS.load(Relaxed);
    if handle.is_null() {
        return;
    }
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 30_000,
    };
    unsafe { SetServiceStatus(handle, &st) };
}

unsafe extern "system" fn handler(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOP_PENDING);
            server::request_stop();
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(NAME);
    let handle = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    STATUS.store(handle, Relaxed);
    set_state(SERVICE_START_PENDING);
    let result = std::panic::catch_unwind(|| server::run(super::config(), || set_state(SERVICE_RUNNING)));
    if result.is_err() {
        server::log("the service failed (panic)");
    }
    set_state(SERVICE_STOPPED);
}

/// Hand this process to the SCM (`fex service run`, as the service's
/// command line). Returns when the service stops.
pub fn dispatch() -> Result<(), String> {
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let e = unsafe { GetLastError() };
        return Err(if e == 1063 {
            "`fex service run` is started by Windows; use `fex service install`, or `fex service debug` to run in this console".into()
        } else {
            std::io::Error::from_raw_os_error(e as i32).to_string()
        });
    }
    Ok(())
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn manager() -> Result<Handle, String> {
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if h.is_null() {
        return Err(format!("cannot open the service manager (run as administrator): {}", std::io::Error::last_os_error()));
    }
    Ok(Handle(h))
}

/// Install as an automatic (delayed start) service running as LocalSystem,
/// and start it.
/// The service runs a copy in `%ProgramFiles%\FEx` (made here), never the
/// file install was run from: a SYSTEM service must not run a binary that
/// a standard user could replace (Downloads, the desktop, ...).
pub fn install() -> Result<(), String> {
    let exe = crate::update::install_copy()?;
    let command = format!("\"{}\" service run", exe.display());
    let scm = manager()?;
    let (name, display, cmd) = (wide(NAME), wide(DISPLAY), wide(&command));
    let svc = unsafe {
        CreateServiceW(
            scm.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            cmd.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(), // LocalSystem
            std::ptr::null(),
        )
    };
    if svc.is_null() {
        return Err(format!("cannot create the service: {}", std::io::Error::last_os_error()));
    }
    let svc = Handle(svc);
    let mut desc = wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW { lpDescription: desc.as_mut_ptr() };
    // Delayed start: after boot has settled, which suits background work.
    let delayed = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: 1 };
    unsafe {
        ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, &info as *const _ as *const c_void);
        ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, &delayed as *const _ as *const c_void);
        if StartServiceW(svc.0, 0, std::ptr::null()) == 0 {
            return Err(format!("installed, but could not start: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let scm = manager()?;
    let name = wide(NAME);
    let svc = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if svc.is_null() {
        return Err(format!("the service isn't installed: {}", std::io::Error::last_os_error()));
    }
    let svc = Handle(svc);
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    unsafe {
        ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st);
        if DeleteService(svc.0) == 0 {
            return Err(format!("cannot remove the service: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ for updates

/// The installed service: its command line and whether it's running.
pub struct Installed {
    pub command: String,
    pub running: bool,
}

fn open(access: u32) -> Result<Option<Handle>, String> {
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if scm.is_null() {
        return Err(format!("cannot open the service manager: {}", std::io::Error::last_os_error()));
    }
    let scm = Handle(scm);
    let name = wide(NAME);
    let svc = unsafe { OpenServiceW(scm.0, name.as_ptr(), access) };
    Ok((!svc.is_null()).then_some(Handle(svc)))
}

fn state(svc: &Handle) -> Option<SERVICE_STATUS_CURRENT_STATE> {
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    (unsafe { QueryServiceStatus(svc.0, &mut st) } != 0).then_some(st.dwCurrentState)
}

/// A NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// The installed service, if any.
pub fn installed() -> Result<Option<Installed>, String> {
    let Some(svc) = open(SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)? else { return Ok(None) };
    let mut need = 0u32;
    unsafe { QueryServiceConfigW(svc.0, std::ptr::null_mut(), 0, &mut need) };
    let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
    if unsafe { QueryServiceConfigW(svc.0, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need) } == 0 {
        return Err(format!("cannot read the service's configuration: {}", std::io::Error::last_os_error()));
    }
    let cfg = unsafe { &*buf.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    let command = unsafe { from_wide_ptr(cfg.lpBinaryPathName) };
    let running = !matches!(state(&svc), Some(SERVICE_STOPPED) | None);
    Ok(Some(Installed { command, running }))
}

/// Stop the service and wait (it saves its indexes as it stops).
pub fn stop_and_wait(limit: std::time::Duration) -> Result<(), String> {
    let Some(svc) = open(SERVICE_ALL_ACCESS)? else { return Ok(()) };
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) };
    let t = std::time::Instant::now();
    while t.elapsed() < limit {
        if matches!(state(&svc), Some(SERVICE_STOPPED)) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err("the index service didn't stop in time".into())
}

pub fn start() -> Result<(), String> {
    let Some(svc) = open(SERVICE_ALL_ACCESS)? else { return Err("the index service isn't installed".into()) };
    if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
        return Err(format!("cannot start the index service: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Point the service at another command line.
pub fn set_command(command: &str) -> Result<(), String> {
    let Some(svc) = open(SERVICE_ALL_ACCESS)? else { return Err("the index service isn't installed".into()) };
    let cmd = wide(command);
    let ok = unsafe {
        ChangeServiceConfigW(svc.0, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE, cmd.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null())
    };
    if ok == 0 {
        return Err(format!("cannot change the service's command: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}
