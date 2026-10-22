//! OLE drop target for the main window.
//!
//! winit's own drop target only understands `CF_HDROP` (files that exist on
//! disk). This one also accepts *virtual* files, offered as
//! `FileGroupDescriptorW` + `FileContents`: Outlook attachments, files dragged
//! from browsers, from Explorer's view inside a zip, and so on. Their bytes are
//! read on the UI thread during the drop (the data object belongs to this
//! apartment), streamed straight into hidden staging files in the target
//! folder so a huge item can't exhaust memory, and the app renames them into
//! place on a worker.
//!
//! Every callback catches panics: unwinding into OLE would abort the process.
//!
//! Drops land on the folder under the pointer (a folder row, sidebar entry,
//! tab or path segment; the app publishes these zones every frame), else in
//! the open folder. Tabs dragged from other FEx windows are accepted too.

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use eframe::egui;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{E_NOINTERFACE, E_POINTER, E_UNEXPECTED, HWND, POINT, POINTL, S_OK};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop,
};
use windows_sys::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT};
use windows_sys::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, DragQueryFileW, FD_ATTRIBUTES, FD_FILESIZE, FD_WRITESTIME,
    FILEDESCRIPTORW,
};

use crate::{fileop, oledrag, win};

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_IDROPTARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);

/// A file that exists only inside the drag source.
pub struct VirtualFile {
    /// Relative path (may contain `\` for files inside dragged folders). Only
    /// safe to use when `data` is `Ok`; otherwise it is the raw name, for messages.
    pub name: String,
    pub is_dir: bool,
    pub mtime: u64,
    /// The contents, in a hidden staging file in the target folder (`None`
    /// for a folder), or why they couldn't be saved.
    pub data: Result<Option<PathBuf>, String>,
}

pub enum Dropped {
    /// Real files for folder `dest`, with the keys held at drop time.
    Files { paths: Vec<String>, dest: String, ctrl: bool, shift: bool },
    Virtual { files: Vec<VirtualFile>, dest: String },
    /// A tab dragged here from another FEx window.
    Tab(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Payload {
    Files,
    Virtual,
    Tab,
}

/// What is being dragged over the window right now.
#[derive(Clone)]
pub struct Hover {
    pub count: usize,
    pub payload: Payload,
    pub moving: bool,
    /// Pointer position in window points.
    pub pos: egui::Pos2,
    /// The folder it would land in (`None`: nowhere here takes it).
    pub dest: Option<String>,
}

#[derive(Default)]
struct State {
    /// The open folder, or `None` when the current view can't take files.
    target: Option<String>,
    /// Folders shown in parts of the window (rows, sidebar, tabs, path bar), in window points.
    zones: Vec<(egui::Rect, String)>,
    pixels_per_point: f32,
    hover: Option<Hover>,
    /// First dragged path, to tell a same-volume move from a copy.
    first_path: Option<String>,
    /// Whether `first_path` is on the same volume as this folder (asked once
    /// per folder, not on every pointer move).
    same_volume: Option<(String, bool)>,
    dropped: Vec<Dropped>,
}

impl State {
    /// The folder under `pos`, else the open folder.
    fn dest_at(&self, pos: egui::Pos2) -> Option<String> {
        self.zones.iter().find(|(r, _)| r.contains(pos)).map(|(_, d)| d.clone()).or_else(|| self.target.clone())
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The app's side of the drop target.
#[derive(Clone)]
pub struct Inbox(Arc<Mutex<State>>);

impl Inbox {
    /// Where drops go this frame: the open folder (`None` refuses drops
    /// outside zones: Recent, Network, archives) and folders under parts of
    /// the window.
    pub fn set_layout(&self, target: Option<&str>, zones: Vec<(egui::Rect, String)>, pixels_per_point: f32) {
        let mut state = lock(&self.0);
        if state.target.as_deref() != target {
            state.target = target.map(str::to_string);
        }
        state.zones = zones;
        state.pixels_per_point = pixels_per_point;
    }

    /// The drop zone under `pos`, to highlight.
    pub fn zone_at(&self, pos: egui::Pos2) -> Option<egui::Rect> {
        lock(&self.0).zones.iter().find(|(r, _)| r.contains(pos)).map(|(r, _)| *r)
    }

    pub fn hover(&self) -> Option<Hover> {
        lock(&self.0).hover.clone()
    }

    pub fn take(&self) -> Vec<Dropped> {
        std::mem::take(&mut lock(&self.0).dropped)
    }
}

// ---------------------------------------------------------------- COM glue

#[repr(C)]
struct DataObjectVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    get_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    get_data_here: usize,
    query_get_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC) -> HRESULT,
}

#[repr(C)]
struct DataObject {
    vtbl: *const DataObjectVtbl,
}

#[repr(C)]
struct StreamVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    read: unsafe extern "system" fn(*mut Stream, *mut c_void, u32, *mut u32) -> HRESULT,
}

#[repr(C)]
struct Stream {
    vtbl: *const StreamVtbl,
}

#[repr(C)]
struct TargetVtbl {
    query_interface: unsafe extern "system" fn(*mut Target, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Target) -> u32,
    release: unsafe extern "system" fn(*mut Target) -> u32,
    drag_enter: unsafe extern "system" fn(*mut Target, *mut DataObject, u32, POINTL, *mut u32) -> HRESULT,
    drag_over: unsafe extern "system" fn(*mut Target, u32, POINTL, *mut u32) -> HRESULT,
    drag_leave: unsafe extern "system" fn(*mut Target) -> HRESULT,
    drop: unsafe extern "system" fn(*mut Target, *mut DataObject, u32, POINTL, *mut u32) -> HRESULT,
}

#[repr(C)]
struct Target {
    vtbl: *const TargetVtbl,
    refs: AtomicU32,
    state: Arc<Mutex<State>>,
    ctx: egui::Context,
    hwnd: HWND,
    descriptor_format: u16,
    contents_format: u16,
    tab_format: u16,
}

static VTBL: TargetVtbl = TargetVtbl { query_interface, add_ref, release, drag_enter, drag_over, drag_leave, drop: on_drop };

unsafe extern "system" fn query_interface(this: *mut Target, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let riid = unsafe { &*riid };
    let eq = |a: &GUID, b: &GUID| a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4;
    if eq(riid, &IID_IUNKNOWN) || eq(riid, &IID_IDROPTARGET) {
        unsafe {
            add_ref(this);
            *out = this as *mut c_void;
        }
        S_OK
    } else {
        unsafe { *out = null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref(this: *mut Target) -> u32 {
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn release(this: *mut Target) -> u32 {
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

fn format(cf: u16, lindex: i32, tymed: i32) -> FORMATETC {
    FORMATETC { cfFormat: cf, ptd: null_mut(), dwAspect: DVASPECT_CONTENT, lindex, tymed: tymed as u32 }
}

fn has(data: *mut DataObject, cf: u16) -> bool {
    let f = format(cf, -1, TYMED_HGLOBAL);
    unsafe { ((*(*data).vtbl).query_get_data)(data, &f) == S_OK }
}

/// Fetch `cf` from the data object; the medium is released on drop.
struct Medium(STGMEDIUM);

impl Medium {
    fn get(data: *mut DataObject, f: FORMATETC) -> Option<Medium> {
        let mut m: STGMEDIUM = unsafe { std::mem::zeroed() };
        (unsafe { ((*(*data).vtbl).get_data)(data, &f, &mut m) } == S_OK).then_some(Medium(m))
    }

    /// Run `f` on the bytes of an HGLOBAL medium (without copying them).
    fn with_bytes<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
        if self.0.tymed != TYMED_HGLOBAL as u32 {
            return None;
        }
        unsafe {
            let h = self.0.u.hGlobal;
            let len = GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            if p.is_null() {
                return None;
            }
            // Unlocked however `f` ends.
            struct Unlock(windows_sys::Win32::Foundation::HGLOBAL);
            impl Drop for Unlock {
                fn drop(&mut self) {
                    unsafe { GlobalUnlock(self.0) };
                }
            }
            let _unlock = Unlock(h);
            Some(f(std::slice::from_raw_parts(p, len)))
        }
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        self.with_bytes(<[u8]>::to_vec)
    }
}

impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn hdrop_paths(data: *mut DataObject) -> Option<Vec<String>> {
    let medium = Medium::get(data, format(CF_HDROP, -1, TYMED_HGLOBAL))?;
    let hdrop = unsafe { medium.0.u.hGlobal };
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, null_mut(), 0) };
    let mut paths = Vec::with_capacity(count as usize);
    let mut buf: Vec<u16> = Vec::new();
    for i in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, i, null_mut(), 0) } as usize;
        buf.resize(len + 1, 0);
        unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) };
        let mut s = String::with_capacity(len);
        win::push_utf16(&mut s, &buf[..len]);
        paths.push(s);
    }
    Some(paths)
}

fn descriptors(data: *mut DataObject, cf: u16) -> Option<Vec<FILEDESCRIPTORW>> {
    let bytes = Medium::get(data, format(cf, -1, TYMED_HGLOBAL))?.bytes()?;
    let count = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    let size = size_of::<FILEDESCRIPTORW>();
    (0..count)
        .map(|i| {
            let at = 4 + i * size;
            let chunk = bytes.get(at..at + size)?;
            // SAFETY: FILEDESCRIPTORW is plain old data; the slice holds a whole one.
            Some(unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const FILEDESCRIPTORW) })
        })
        .collect()
}

/// A dropped name made safe to join onto the target folder: relative, no
/// `..`, no drive or stream syntax, no control characters or device names.
fn safe_relative(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    (!parts.is_empty() && parts.iter().all(|p| crate::fsys::safe_component(p))).then(|| parts.join("\\"))
}

static STAGE_ID: AtomicU64 = AtomicU64::new(0);

/// A new hidden staging file in `dir`.
fn stage_file(dir: &str) -> Result<(PathBuf, File), String> {
    use std::os::windows::fs::OpenOptionsExt;
    const HIDDEN: u32 = 0x2;
    for _ in 0..16 {
        let id = STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(dir).join(format!("~fex-drop-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).attributes(HIDDEN).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not save into {dir}: {e}")),
        }
    }
    Err(format!("could not save into {dir}"))
}

/// Stream one virtual file into a staging file in `dir`. Memory use stays
/// at one 1 MB chunk whatever the size; the disk is the limit.
fn read_contents(data: *mut DataObject, cf: u16, index: usize, d: &FILEDESCRIPTORW, dir: &str) -> Result<PathBuf, String> {
    let declared = (d.dwFlags as i32 & FD_FILESIZE != 0).then(|| (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64);
    if let (Some(size), Some((free, _))) = (declared, win::drive_space(dir)) {
        if size > free {
            return Err(format!("not enough space ({} needed)", crate::fsys::format_size(size)));
        }
    }
    let medium = Medium::get(data, format(cf, index as i32, TYMED_HGLOBAL | TYMED_ISTREAM))
        .ok_or("the source did not provide the file's contents")?;
    let (path, mut file) = stage_file(dir)?;
    let written: Result<(), String> = match medium.0.tymed as i32 {
        TYMED_HGLOBAL => medium
            .with_bytes(|bytes| {
                // HGLOBALs are often rounded up; the descriptor has the real size.
                let len = declared.map_or(bytes.len(), |size| bytes.len().min(size as usize));
                file.write_all(&bytes[..len]).map_err(|e| e.to_string())
            })
            .unwrap_or_else(|| Err("could not read the file's contents".into())),
        TYMED_ISTREAM => {
            let stream = unsafe { medium.0.u.pstm } as *mut Stream;
            let mut chunk = vec![0u8; 1 << 20];
            let mut total = 0u64;
            loop {
                let mut got = 0u32;
                let hr = unsafe { ((*(*stream).vtbl).read)(stream, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut got) };
                if hr < 0 {
                    break Err(format!("reading from the source failed (0x{hr:08x})"));
                }
                let got = (got as usize).min(chunk.len());
                if got == 0 {
                    break Ok(());
                }
                total += got as u64;
                // A source that keeps producing past what it declared is refused.
                if declared.is_some_and(|size| total > size) {
                    break Err("the source sent more data than it declared".into());
                }
                if let Err(e) = file.write_all(&chunk[..got]) {
                    break Err(e.to_string());
                }
            }
        }
        _ => Err("the source offers this item in a format FEx can't save (save it from the original app)".into()),
    };
    drop(file);
    match written {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

fn virtual_files(this: &Target, data: *mut DataObject, dir: &str) -> Option<Vec<VirtualFile>> {
    let list = descriptors(data, this.descriptor_format)?;
    Some(
        list.iter()
            .enumerate()
            .map(|(i, d)| {
                let units = d.cFileName; // copied out: the struct is packed
                let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
                let mut raw = String::new();
                win::push_utf16(&mut raw, &units[..len]);
                let is_dir = d.dwFlags as i32 & FD_ATTRIBUTES != 0 && d.dwFileAttributes & win::ATTR_DIR != 0;
                let mtime = if d.dwFlags as i32 & FD_WRITESTIME != 0 { win::ft_u64(d.ftLastWriteTime) } else { 0 };
                let Some(name) = safe_relative(&raw) else {
                    return VirtualFile { name: raw.clone(), is_dir, mtime, data: Err(format!("unsafe file name '{raw}'")) };
                };
                let data = if is_dir { Ok(None) } else { read_contents(data, this.contents_format, i, d, dir).map(Some) };
                VirtualFile { name, is_dir, mtime, data }
            })
            .collect(),
    )
}

/// The effect to show and perform, Explorer-style: Ctrl copies, Shift moves,
/// otherwise same volume moves; virtual files can only be copied; tabs move.
fn effect(state: &State, dest: Option<&str>, keys: u32, allowed: u32, payload: Payload) -> u32 {
    let Some(dest) = dest else { return DROPEFFECT_NONE };
    let wanted = match payload {
        Payload::Tab => DROPEFFECT_MOVE,
        Payload::Virtual => DROPEFFECT_COPY,
        Payload::Files if keys & MK_CONTROL != 0 => DROPEFFECT_COPY,
        Payload::Files if keys & MK_SHIFT != 0 => DROPEFFECT_MOVE,
        Payload::Files if state.same_volume.as_ref().is_some_and(|(d, same)| d == dest && *same) => DROPEFFECT_MOVE,
        Payload::Files => DROPEFFECT_COPY,
    };
    if allowed & wanted != 0 {
        wanted
    } else if payload != Payload::Tab && allowed & DROPEFFECT_COPY != 0 {
        DROPEFFECT_COPY
    } else {
        allowed & wanted
    }
}

/// Screen pixels → window points.
fn window_pos(this: &Target, state: &State, pt: POINTL) -> egui::Pos2 {
    let mut origin = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(this.hwnd, &mut origin) };
    let ppp = if state.pixels_per_point > 0.0 { state.pixels_per_point } else { 1.0 };
    egui::pos2((pt.x - origin.x) as f32 / ppp, (pt.y - origin.y) as f32 / ppp)
}

/// Update the hover for a pointer move; repaint only when something visible changed.
fn track(this: &Target, state: &mut State, keys: u32, pt: POINTL, allowed: u32) -> u32 {
    let Some(hover) = state.hover.clone() else { return DROPEFFECT_NONE };
    let pos = window_pos(this, state, pt);
    // Tabs open as tabs wherever they're dropped; they don't need a folder.
    let dest = if hover.payload == Payload::Tab { Some(String::new()) } else { state.dest_at(pos) };
    if let (Some(first), Some(d)) = (&state.first_path, &dest) {
        if state.same_volume.as_ref().is_none_or(|(cached, _)| cached != d) {
            state.same_volume = Some((d.clone(), fileop::same_volume(first, d)));
        }
    }
    let e = effect(state, dest.as_deref(), keys, allowed, hover.payload);
    let moving = e == DROPEFFECT_MOVE;
    let dest = dest.filter(|d| !d.is_empty() && e != DROPEFFECT_NONE);
    if hover.moving != moving || hover.dest != dest || (hover.pos - pos).length() > 0.5 {
        state.hover = Some(Hover { moving, dest, pos, ..hover });
        this.ctx.request_repaint();
    }
    e
}

/// A tab from another FEx window (our own tabs are rearranged in-window).
fn foreign_tab(this: &Target, data: *mut DataObject) -> Option<String> {
    let bytes = Medium::get(data, format(this.tab_format, -1, TYMED_HGLOBAL))?.bytes()?;
    oledrag::parse_tab(&bytes).filter(|(pid, _)| *pid != std::process::id()).map(|(_, location)| location)
}

/// Run a drop-target callback body; a panic refuses the drag instead of
/// unwinding into OLE (which would abort the process).
fn guarded(effect_out: *mut u32, f: impl FnOnce() -> HRESULT) -> HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        if !effect_out.is_null() {
            unsafe { *effect_out = DROPEFFECT_NONE };
        }
        E_UNEXPECTED
    })
}

unsafe extern "system" fn drag_enter(this: *mut Target, data: *mut DataObject, keys: u32, pt: POINTL, effect_out: *mut u32) -> HRESULT {
    guarded(effect_out, || unsafe { enter(&*this, data, keys, pt, effect_out) })
}

unsafe fn enter(this: &Target, data: *mut DataObject, keys: u32, pt: POINTL, effect_out: *mut u32) -> HRESULT {
    let (payload, count, paths) = if has(data, this.tab_format) {
        (Payload::Tab, foreign_tab(this, data).map_or(0, |_| 1), Vec::new())
    } else if has(data, CF_HDROP) {
        let paths = hdrop_paths(data).unwrap_or_default();
        (Payload::Files, paths.len(), paths)
    } else if has(data, this.descriptor_format) {
        (Payload::Virtual, descriptors(data, this.descriptor_format).map_or(0, |d| d.len()), Vec::new())
    } else {
        (Payload::Files, 0, Vec::new())
    };
    let mut state = lock(&this.state);
    state.first_path = paths.first().cloned();
    state.same_volume = None;
    state.hover = (count > 0).then_some(Hover { count, payload, moving: false, pos: egui::Pos2::ZERO, dest: None });
    let e = track(this, &mut state, keys, pt, unsafe { *effect_out });
    unsafe { *effect_out = e };
    this.ctx.request_repaint();
    S_OK
}

unsafe extern "system" fn drag_over(this: *mut Target, keys: u32, pt: POINTL, effect_out: *mut u32) -> HRESULT {
    guarded(effect_out, || unsafe {
        let this = &*this;
        let mut state = lock(&this.state);
        let e = track(this, &mut state, keys, pt, *effect_out);
        *effect_out = e;
        S_OK
    })
}

unsafe extern "system" fn drag_leave(this: *mut Target) -> HRESULT {
    guarded(null_mut(), || {
        let this = unsafe { &*this };
        let mut state = lock(&this.state);
        state.hover = None;
        state.first_path = None;
        state.same_volume = None;
        this.ctx.request_repaint();
        S_OK
    })
}

unsafe extern "system" fn on_drop(this: *mut Target, data: *mut DataObject, keys: u32, pt: POINTL, effect_out: *mut u32) -> HRESULT {
    guarded(effect_out, || unsafe { dropped(&*this, data, keys, pt, effect_out) })
}

unsafe fn dropped(this: &Target, data: *mut DataObject, keys: u32, pt: POINTL, effect_out: *mut u32) -> HRESULT {
    let allowed = unsafe { *effect_out };
    let (e, payload, dest) = {
        let mut state = lock(&this.state);
        let e = track(this, &mut state, keys, pt, allowed);
        let hover = state.hover.take();
        (e, hover.as_ref().map(|h| h.payload), hover.and_then(|h| h.dest))
    };
    // Read outside the lock: the source may pump messages while we read.
    let dropped = match (payload, dest) {
        _ if e == DROPEFFECT_NONE => None,
        (Some(Payload::Tab), _) => foreign_tab(this, data).map(Dropped::Tab),
        (Some(Payload::Files), Some(dest)) => {
            hdrop_paths(data).map(|paths| Dropped::Files { paths, dest, ctrl: keys & MK_CONTROL != 0, shift: keys & MK_SHIFT != 0 })
        }
        (Some(Payload::Virtual), Some(dest)) => virtual_files(this, data, &dest).map(|files| Dropped::Virtual { files, dest }),
        _ => None,
    };
    let mut state = lock(&this.state);
    state.first_path = None;
    state.same_volume = None;
    unsafe { *effect_out = if dropped.is_some() { e } else { DROPEFFECT_NONE } };
    state.dropped.extend(dropped);
    this.ctx.request_repaint();
    S_OK
}

fn clipboard_format(name: *const u16) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// Install the drop target on `hwnd`. Must run on the UI thread, which
/// becomes an OLE apartment (winit's own drop target must be disabled).
pub fn register(hwnd: HWND, ctx: egui::Context) -> Result<Inbox, String> {
    let state = Arc::new(Mutex::new(State::default()));
    let target = Box::into_raw(Box::new(Target {
        vtbl: &VTBL,
        refs: AtomicU32::new(1), // ours, held for the window's lifetime
        state: state.clone(),
        ctx,
        hwnd,
        descriptor_format: clipboard_format(CFSTR_FILEDESCRIPTORW),
        contents_format: clipboard_format(CFSTR_FILECONTENTS),
        tab_format: oledrag::tab_format(),
    }));
    unsafe {
        let hr = OleInitialize(null());
        if hr < 0 {
            release(target);
            return Err(format!("OLE could not be initialized (0x{hr:08x})"));
        }
        RevokeDragDrop(hwnd);
        let hr = RegisterDragDrop(hwnd, target as *mut c_void);
        if hr < 0 {
            release(target);
            return Err(format!("Could not accept dropped files (0x{hr:08x})"));
        }
    }
    Ok(Inbox(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_names_stay_inside_the_target() {
        assert_eq!(safe_relative("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_relative(r"folder/sub\file.txt").as_deref(), Some(r"folder\sub\file.txt"));
        assert_eq!(safe_relative(r"..\evil.exe"), None);
        assert_eq!(safe_relative(r"C:\Windows\evil.exe"), None);
        assert_eq!(safe_relative("file.txt:stream"), None);
        assert_eq!(safe_relative(""), None);
        assert_eq!(safe_relative("NUL.txt"), None);
        assert_eq!(safe_relative(r"dir\com1"), None);
        assert_eq!(safe_relative("tab\tname"), None);
        assert_eq!(safe_relative("console.log").as_deref(), Some("console.log"));
    }
}
