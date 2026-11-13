//! Service Control Manager integration: install, uninstall, and the entry
//! point the SCM calls (`fex service run`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, RegisterServiceCtrlHandlerExW,
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
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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
