//! Win32 primitives for changing the filesystem: copy, move, delete, create,
//! attributes, and the storage queries the transfer engine uses to pick a
//! thread count.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::AtomicI32;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    PNP_VetoOutstandingOpen, PNP_VetoTypeUnknown, PNP_VetoWindowsApp, PNP_VetoWindowsService, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SHARING_VIOLATION, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileExW, CreateDirectoryW, CreateFileW, CreateSymbolicLinkW, DeleteFileW, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FlushFileBuffers, GetCompressedFileSizeW, GetDriveTypeW,
    GetFileAttributesExW, GetFileExInfoStandard, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    MoveFileExW, RemoveDirectoryW, SetFileAttributesW,
    COPY_FILE_FAIL_IF_EXISTS, COPY_FILE_NO_BUFFERING, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, FILE_SHARE_READ, FILE_SHARE_WRITE,
    MOVEFILE_REPLACE_EXISTING, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY,
    OPEN_EXISTING, PROGRESS_CANCEL, PROGRESS_CONTINUE, WIN32_FILE_ATTRIBUTE_DATA, WIN32_FIND_DATAW,
};
use windows_sys::Win32::Storage::FileSystem::{BusType1394, BusTypeMmc, BusTypeSd, BusTypeUsb, STORAGE_BUS_TYPE};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_EJECT_MEDIA,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_MEDIA_REMOVAL, IOCTL_STORAGE_QUERY_PROPERTY, PREVENT_MEDIA_REMOVAL,
    PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
    StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::Shell::{
    SHFileOperationW, SHObjectProperties, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, FO_DELETE,
    SHFILEOPSTRUCTW, SHOP_FILEPATH,
};

use crate::win::{self, wide};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

fn check(ok: i32) -> Result<(), u32> {
    if ok != 0 { Ok(()) } else { Err(last_error()) }
}

/// Metadata of one entry without opening it (reads the directory record).
pub struct Stat {
    pub attrs: u32,
    pub size: u64,
    pub mtime: u64,
}

pub fn stat(path: &str) -> Option<Stat> {
    let w = wide(path, "");
    // SAFETY: WIN32_FIND_DATAW is plain old data.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(w.as_ptr(), FindExInfoBasic, &mut data as *mut _ as *mut c_void, FindExSearchNameMatch, null(), 0)
    };
    if h == INVALID_HANDLE_VALUE {
        // Drive and share roots have no directory record of their own.
        return win::attributes(path).map(|attrs| Stat { attrs, size: 0, mtime: 0 });
    }
    unsafe { FindClose(h) };
    Some(Stat {
        attrs: data.dwFileAttributes,
        size: (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64,
        mtime: win::ft_u64(data.ftLastWriteTime),
    })
}

/// Timestamps and attributes in a single call.
pub struct Details {
    pub attrs: u32,
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
}

pub fn details(path: &str) -> Result<Details, u32> {
    let mut d = WIN32_FILE_ATTRIBUTE_DATA::default();
    check(unsafe { GetFileAttributesExW(wide(path, "").as_ptr(), GetFileExInfoStandard, &mut d as *mut _ as *mut c_void) })?;
    Ok(Details {
        attrs: d.dwFileAttributes,
        created: win::ft_u64(d.ftCreationTime),
        accessed: win::ft_u64(d.ftLastAccessTime),
        modified: win::ft_u64(d.ftLastWriteTime),
    })
}

/// Allocated size (differs from logical size for compressed/sparse files).
pub fn size_on_disk(path: &str) -> Option<u64> {
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(wide(path, "").as_ptr(), &mut high) };
    if low == u32::MAX && last_error() != 0 {
        return None;
    }
    Some((high as u64) << 32 | low as u64)
}

pub fn set_attributes(path: &str, attrs: u32) -> Result<(), u32> {
    check(unsafe { SetFileAttributesW(wide(path, "").as_ptr(), attrs) })
}

/// Create a directory; succeeds if it already exists.
pub fn create_dir(path: &str) -> Result<(), u32> {
    match check(unsafe { CreateDirectoryW(wide(path, "").as_ptr(), null()) }) {
        Err(ERROR_ALREADY_EXISTS) => Ok(()),
        r => r,
    }
}

pub fn remove_dir(path: &str) -> Result<(), u32> {
    check(unsafe { RemoveDirectoryW(wide(path, "").as_ptr()) })
}

/// Delete a file, clearing the read-only attribute if that's what blocks it.
pub fn delete_file(path: &str) -> Result<(), u32> {
    let w = wide(path, "");
    if unsafe { DeleteFileW(w.as_ptr()) } != 0 {
        return Ok(());
    }
    let err = last_error();
    match stat(path) {
        Some(s) if s.attrs & FILE_ATTRIBUTE_READONLY != 0 => {
            set_attributes(path, s.attrs & !FILE_ATTRIBUTE_READONLY)?;
            check(unsafe { DeleteFileW(w.as_ptr()) })
        }
        _ => Err(err),
    }
}

/// Rename/move within one volume. Never copies.
pub fn rename(src: &str, dst: &str, replace: bool) -> Result<(), u32> {
    let flags = if replace { MOVEFILE_REPLACE_EXISTING } else { 0 };
    check(unsafe { MoveFileExW(wide(src, "").as_ptr(), wide(dst, "").as_ptr(), flags) })
}

pub fn create_symbolic_link(link: &str, target: &str, is_dir: bool) -> Result<(), u32> {
    let mut flags = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE;
    if is_dir {
        flags |= SYMBOLIC_LINK_FLAG_DIRECTORY;
    }
    if unsafe { CreateSymbolicLinkW(wide(link, "").as_ptr(), wide(target, "").as_ptr(), flags) } {
        Ok(())
    } else {
        Err(last_error())
    }
}

struct ProgressCtx<'a> {
    on_progress: &'a mut dyn FnMut(u64),
}

unsafe extern "system" fn progress_cb(
    _total: i64,
    transferred: i64,
    _stream_size: i64,
    _stream_transferred: i64,
    _stream: u32,
    _reason: u32,
    _src: HANDLE,
    _dst: HANDLE,
    data: *const c_void,
) -> u32 {
    // SAFETY: `data` is the &mut ProgressCtx passed to CopyFileExW below, alive for the call.
    let ctx = unsafe { &mut *(data as *mut ProgressCtx) };
    // A panic must not unwind into CopyFileEx: cancel the copy instead.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.on_progress)(transferred as u64))) {
        Ok(()) => PROGRESS_CONTINUE,
        Err(_) => PROGRESS_CANCEL,
    }
}

/// Copy one file, overwriting `dst`. `on_progress` receives bytes copied so
/// far for this file. Setting `cancel` to 1 aborts mid-file.
///
/// CopyFileEx is used deliberately: it pipelines large I/Os, preserves
/// attributes and alternate streams, and uses SMB server-side copy offload
/// when both ends are on the same server.
pub fn copy_file(
    src: &str,
    dst: &str,
    unbuffered: bool,
    fail_if_exists: bool,
    cancel: &AtomicI32,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), u32> {
    let (s, d) = (wide(src, ""), wide(dst, ""));
    let mut flags = if unbuffered { COPY_FILE_NO_BUFFERING } else { 0 };
    if fail_if_exists {
        flags |= COPY_FILE_FAIL_IF_EXISTS;
    }
    let mut ctx = ProgressCtx { on_progress };
    let run = |ctx: &mut ProgressCtx| unsafe {
        CopyFileExW(s.as_ptr(), d.as_ptr(), Some(progress_cb), ctx as *mut ProgressCtx as *const c_void, cancel.as_ptr(), flags)
    };
    if run(&mut ctx) != 0 {
        return Ok(());
    }
    let err = last_error();
    // A read-only, hidden or system destination blocks overwriting (CopyFileEx
    // refuses with "access denied", unlike Explorer); clear those and retry
    // once. The copy then carries the source's attributes.
    const BLOCKING: u32 = FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
    if !fail_if_exists && let Some(st) = stat(dst).filter(|st| st.attrs & BLOCKING != 0) {
        set_attributes(dst, st.attrs & !BLOCKING)?;
        let retry = check(run(&mut ctx));
        if retry.is_err() {
            let _ = set_attributes(dst, st.attrs);
        }
        return retry;
    }
    Err(err)
}

/// Mount root of the volume holding `path` (`C:\`, `\\server\share\`, ...).
pub fn volume_root(path: &str) -> Option<String> {
    let mut buf = [0u16; 1024];
    if unsafe { GetVolumePathNameW(wide(path, "").as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let mut s = String::new();
    win::push_utf16(&mut s, &buf[..len]);
    Some(s)
}

pub fn same_volume(a: &str, b: &str) -> bool {
    match (volume_root(a), volume_root(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y),
        _ => false,
    }
}

pub fn is_network(path: &str) -> bool {
    path.starts_with(r"\\")
        || volume_root(path).is_some_and(|r| unsafe { GetDriveTypeW(wide(&r, "").as_ptr()) } == DRIVE_REMOTE)
}

fn drive_type(root: &str) -> u32 {
    unsafe { GetDriveTypeW(wide(root, "").as_ptr()) }
}

/// Device path of the volume mounted at `root` (`\\?\Volume{guid}`, without
/// the trailing slash, which would open the root directory instead).
fn volume_device(root: &str) -> Option<Vec<u16>> {
    let mut vol = [0u16; 64];
    if unsafe { GetVolumeNameForVolumeMountPointW(wide(root, "").as_ptr(), vol.as_mut_ptr(), vol.len() as u32) } == 0 {
        return None;
    }
    let len = vol.iter().position(|&c| c == 0)?;
    let mut dev: Vec<u16> = vol[..len.saturating_sub(1)].to_vec();
    dev.push(0);
    Some(dev)
}

/// An open device handle, closed on drop.
struct Device(HANDLE);

impl Device {
    fn open(path: *const u16, access: u32) -> Result<Device, u32> {
        let h = unsafe { CreateFileW(path, access, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut()) };
        if h == INVALID_HANDLE_VALUE { Err(last_error()) } else { Ok(Device(h)) }
    }

    /// DeviceIoControl with a plain input and output buffer.
    fn control<I, O>(&self, code: u32, input: Option<&I>, output: Option<&mut O>) -> Result<u32, u32> {
        let mut got = 0u32;
        let (ip, il) = input.map_or((null(), 0), |i| (i as *const I as *const c_void, size_of::<I>() as u32));
        let (op, ol) = output.map_or((null_mut(), 0), |o| (o as *mut O as *mut c_void, size_of::<O>() as u32));
        check(unsafe { DeviceIoControl(self.0, code, ip, il, op, ol, &mut got, null_mut()) })?;
        Ok(got)
    }

    fn device_number(&self) -> Option<STORAGE_DEVICE_NUMBER> {
        let mut n = STORAGE_DEVICE_NUMBER { DeviceType: 0, DeviceNumber: 0, PartitionNumber: 0 };
        self.control::<(), _>(IOCTL_STORAGE_GET_DEVICE_NUMBER, None, Some(&mut n)).ok().map(|_| n)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn bus_type(device: &Device) -> Option<STORAGE_BUS_TYPE> {
    let query = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    // The descriptor is followed by variable-length strings; 1 KB holds them.
    let mut out = [0u64; 128];
    device.control(IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), Some(&mut out)).ok()?;
    let desc = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    Some(desc.BusType)
}

/// Whether the drive at `path` can be safely removed: removable media,
/// optical drives, and fixed disks attached over USB, FireWire or SD (never
/// the Windows drive). Opens the device, so call it off the UI thread.
#[allow(non_upper_case_globals)] // Win32 constant names in patterns
pub fn ejectable(path: &str) -> bool {
    let Some(root) = volume_root(path) else { return false };
    let system = std::env::var("SystemDrive").is_ok_and(|d| root.to_ascii_uppercase().starts_with(&d.to_ascii_uppercase()));
    match drive_type(&root) {
        DRIVE_REMOVABLE | DRIVE_CDROM => !system,
        DRIVE_FIXED if !system => volume_device(&root)
            .and_then(|dev| Device::open(dev.as_ptr(), 0).ok())
            .and_then(|d| bus_type(&d))
            .is_some_and(|bus| matches!(bus, BusTypeUsb | BusType1394 | BusTypeSd | BusTypeMmc)),
        _ => false,
    }
}

/// The disk device instance holding volume device number `number`.
fn disk_instance(number: &STORAGE_DEVICE_NUMBER) -> Option<u32> {
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVINTERFACE_DISK, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set == INVALID_HANDLE_VALUE as isize {
            return None;
        }
        let mut found = None;
        for index in 0.. {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &GUID_DEVINTERFACE_DISK, index, &mut iface) == 0 {
                break;
            }
            let mut detail = [0u64; 256]; // SP_DEVICE_INTERFACE_DETAIL_DATA_W + path, 8-byte aligned
            let header = detail.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*header).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            let size = size_of_val(&detail) as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, header, size, null_mut(), &mut info) == 0 {
                continue;
            }
            let disk = Device::open((*header).DevicePath.as_ptr(), 0).ok().and_then(|d| d.device_number());
            if disk.is_some_and(|d| d.DeviceNumber == number.DeviceNumber && d.DeviceType == number.DeviceType) {
                found = Some(info.DevInst);
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// Ask Plug and Play to stop the device so it can be unplugged, as the
/// "Safely Remove Hardware" icon does. Tries the device's parent (the USB
/// mass storage device) first, then the disk itself.
#[allow(non_upper_case_globals)]
fn request_eject(disk: u32) -> Result<(), String> {
    let mut parent = 0u32;
    let has_parent = unsafe { CM_Get_Parent(&mut parent, disk, 0) } == CR_SUCCESS;
    let mut last = String::from("Windows could not stop the device");
    for target in [has_parent.then_some(parent), Some(disk)].into_iter().flatten() {
        for _ in 0..3 {
            let mut veto = PNP_VetoTypeUnknown;
            let mut name = [0u16; 260];
            let rc = unsafe { CM_Request_Device_EjectW(target, &mut veto, name.as_mut_ptr(), name.len() as u32, 0) };
            if rc == CR_SUCCESS && veto == PNP_VetoTypeUnknown {
                return Ok(());
            }
            let len = name.iter().position(|&c| c == 0).unwrap_or(0);
            let mut who = String::new();
            win::push_utf16(&mut who, &name[..len]);
            last = match veto {
                PNP_VetoOutstandingOpen => "Files on the drive are still open in another program".into(),
                PNP_VetoWindowsApp | PNP_VetoWindowsService if !who.is_empty() => format!("The drive is in use by {who}"),
                PNP_VetoTypeUnknown => format!("Windows could not stop the device (error {rc})"),
                _ => "The drive is in use; close programs using it and try again".into(),
            };
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Err(last)
}

/// Safely eject the drive holding `path`. Locking the volume first proves
/// nothing has files open on it and lets cached writes be flushed; the
/// device is then stopped through Plug and Play so it can be unplugged.
/// Optical drives and card readers get a media eject instead, so an internal
/// reader isn't switched off.
#[allow(non_upper_case_globals)]
pub fn eject(path: &str) -> Result<(), String> {
    let root = volume_root(path).ok_or("This drive is no longer available")?;
    let kind = drive_type(&root);
    let dev = volume_device(&root).ok_or("This drive is no longer available")?;
    let volume = Device::open(dev.as_ptr(), GENERIC_READ | GENERIC_WRITE).map_err(win::error_text)?;

    // Another program may hold a handle for a moment (antivirus, indexer).
    let mut locked = Err(0);
    for _ in 0..20 {
        locked = volume.control::<(), ()>(FSCTL_LOCK_VOLUME, None, None);
        match locked {
            Err(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION) => std::thread::sleep(Duration::from_millis(100)),
            _ => break,
        }
    }
    if let Err(e) = locked {
        return Err(match e {
            ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION => "Files on the drive are still open in another program".into(),
            e => win::error_text(e),
        });
    }
    check(unsafe { FlushFileBuffers(volume.0) }).map_err(win::error_text)?;
    volume.control::<(), ()>(FSCTL_DISMOUNT_VOLUME, None, None).map_err(win::error_text)?;

    let media_only = kind == DRIVE_CDROM || matches!(bus_type(&volume), Some(BusTypeSd | BusTypeMmc));
    if media_only {
        let allow = PREVENT_MEDIA_REMOVAL { PreventMediaRemoval: false };
        let _ = volume.control::<_, ()>(IOCTL_STORAGE_MEDIA_REMOVAL, Some(&allow), None);
        return volume.control::<(), ()>(IOCTL_STORAGE_EJECT_MEDIA, None, None).map(|_| ()).map_err(win::error_text);
    }
    let number = volume.device_number().ok_or("Could not identify the drive's device")?;
    // Closing releases the lock; the volume stays dismounted for the eject.
    drop(volume);
    let disk = disk_instance(&number).ok_or("Could not find the drive's device")?;
    request_eject(disk)
}

/// Whether the disk under `path` is rotational (seeks are expensive).
/// `None` when it can't be determined (network, spanned volumes, ...).
pub fn seek_penalty(path: &str) -> Option<bool> {
    let dev = volume_device(&volume_root(path)?)?;
    unsafe {
        let h = CreateFileW(dev.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut());
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version u32, Size u32, IncursSeekPenalty u8.
        let mut out = [0u32; 3];
        let mut got = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            out.as_mut_ptr() as *mut c_void,
            size_of_val(&out) as u32,
            &mut got,
            null_mut(),
        );
        CloseHandle(h);
        (ok != 0 && got >= 9).then(|| out[2] & 0xFF != 0)
    }
}

/// Send to the Recycle Bin (or delete permanently). The shell still warns if
/// an item is too large to recycle rather than silently destroying it.
pub fn recycle(paths: &[String], permanent: bool) -> Result<(), String> {
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut flags = FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING;
    if !permanent {
        flags |= FOF_ALLOWUNDO;
    }
    let mut op = SHFILEOPSTRUCTW {
        hwnd: null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: null(),
        fFlags: flags as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: null_mut(),
        lpszProgressTitle: null(),
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if op.fAnyOperationsAborted != 0 {
        Err("Delete was cancelled".into())
    } else if rc != 0 {
        Err(format!("Delete failed (shell error {rc:#x})"))
    } else {
        Ok(())
    }
}

/// Open Windows' own property sheet on its Security tab. Must be called on
/// the UI thread (the sheet attaches to its message loop).
pub fn show_security_dialog(path: &str) -> bool {
    let page: Vec<u16> = "Security".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SHObjectProperties(GetActiveWindow(), SHOP_FILEPATH as u32, wide(path, "").as_ptr(), page.as_ptr()) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicI32;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fex-fileop-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copy_refuses_a_destination_that_appeared_after_planning() {
        let dir = scratch("late-destination");
        let src = dir.join("source.txt");
        let dst = dir.join("destination.txt");
        fs::write(&src, b"source").unwrap();
        fs::write(&dst, b"existing").unwrap();
        let (src, dst) = (src.to_string_lossy(), dst.to_string_lossy());
        let mut progress = |_| {};

        assert!(copy_file(&src, &dst, false, true, &AtomicI32::new(0), &mut progress).is_err());
        assert_eq!(fs::read(dst.as_ref()).unwrap(), b"existing");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_overwrites_hidden_and_system_destinations() {
        let dir = scratch("hidden-destination");
        let src = dir.join("source.ini");
        fs::write(&src, b"new").unwrap();
        for (name, attrs) in [("hidden.ini", FILE_ATTRIBUTE_HIDDEN), ("system.ini", FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)] {
            let dst = dir.join(name);
            fs::write(&dst, b"old").unwrap();
            set_attributes(&dst.to_string_lossy(), attrs).unwrap();
            let mut progress = |_| {};
            copy_file(&src.to_string_lossy(), &dst.to_string_lossy(), false, false, &AtomicI32::new(0), &mut progress).unwrap();
            assert_eq!(fs::read(&dst).unwrap(), b"new", "{name}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_readonly_overwrite_restores_destination_attribute() {
        let dir = scratch("readonly-retry");
        let dst = dir.join("destination.txt");
        let src = dir.join("missing-source.txt");
        fs::write(&dst, b"existing").unwrap();
        let mut permissions = fs::metadata(&dst).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&dst, permissions).unwrap();
        let (src, dst) = (src.to_string_lossy(), dst.to_string_lossy());
        let mut progress = |_| {};

        assert!(copy_file(&src, &dst, false, false, &AtomicI32::new(0), &mut progress).is_err());
        assert_ne!(stat(&dst).unwrap().attrs & FILE_ATTRIBUTE_READONLY, 0);
        let attrs = win::attributes(&dst).unwrap();
        set_attributes(&dst, attrs & !FILE_ATTRIBUTE_READONLY).unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
