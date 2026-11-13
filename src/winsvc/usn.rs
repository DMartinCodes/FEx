//! Catching up with the NTFS change journal.
//!
//! NTFS logs every change to a volume in its USN journal. The service saves
//! the journal's position with each index; on the next start it reads what
//! happened since, re-reads just the folders involved, and is current again
//! without crawling or verifying anything. If the journal was recreated or
//! has wrapped past the saved position, the changes are lost and the drive
//! is crawled again (in the background, serving the saved index meanwhile).
//!
//! Needs administrator rights (opening the volume), which the service has.

use std::collections::HashSet;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdType, GetFinalPathNameByHandleW, OpenFileById, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0};

/// More changed folders than this: crawling is cheaper than resolving them.
const MAX_FOLDERS: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub journal: u64,
    pub next: i64,
}

struct Volume(HANDLE);

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `C:\` -> the volume device `\\.\C:`.
fn open_volume(root: &str) -> Option<Volume> {
    let letter = root.chars().next().filter(|c| c.is_ascii_alphabetic() && root[1..].starts_with(':'))?;
    let name = wide(&format!(r"\\.\{letter}:"));
    let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    (h != INVALID_HANDLE_VALUE).then_some(Volume(h))
}

fn query(vol: &Volume) -> Option<USN_JOURNAL_DATA_V0> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(vol.0, FSCTL_QUERY_USN_JOURNAL, std::ptr::null(), 0, &mut data as *mut _ as *mut _, size_of::<USN_JOURNAL_DATA_V0>() as u32, &mut got, std::ptr::null_mut())
    };
    (ok != 0).then_some(data)
}

/// The journal's current position on the drive at `root`, if it has one.
pub fn position(root: &str) -> Option<Position> {
    let vol = open_volume(root)?;
    query(&vol).map(|d| Position { journal: d.UsnJournalID, next: d.NextUsn })
}

pub enum CatchUp {
    /// Folders whose listings changed since the position.
    Dirs(Vec<String>),
    /// The journal no longer reaches back that far.
    Lost,
}

/// Everything that changed on `root`'s volume since `since`, as folders.
pub fn changed_since(root: &str, since: Position) -> CatchUp {
    let Some(vol) = open_volume(root) else { return CatchUp::Lost };
    let Some(now) = query(&vol) else { return CatchUp::Lost };
    if now.UsnJournalID != since.journal || since.next < now.FirstUsn || since.next > now.NextUsn {
        return CatchUp::Lost;
    }
    // Every change counts (names, sizes, dates, attributes): the index keeps
    // metadata, and re-reading a folder refreshes all of it.
    let mut parents: HashSet<u64> = HashSet::new();
    let mut start = since.next;
    let mut buf = vec![0u64; 8 * 1024]; // 64 KB, 8-byte aligned
    while start < now.NextUsn {
        let req = READ_USN_JOURNAL_DATA_V0 { StartUsn: start, ReasonMask: u32::MAX, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: since.journal };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(vol.0, FSCTL_READ_USN_JOURNAL, &req as *const _ as *const _, size_of::<READ_USN_JOURNAL_DATA_V0>() as u32, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut got, std::ptr::null_mut())
        };
        if ok == 0 {
            return CatchUp::Lost;
        }
        // SAFETY: `got` bytes were written by the driver.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, got as usize) };
        if bytes.len() < 8 {
            break;
        }
        let next = i64::from_le_bytes(bytes[..8].try_into().unwrap());
        // USN_RECORD_V2: RecordLength u32 @0, MajorVersion u16 @4,
        // FileReferenceNumber u64 @8, ParentFileReferenceNumber u64 @16.
        let mut off = 8;
        while off + 24 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            if len < 24 || off + len > bytes.len() {
                break;
            }
            if u16::from_le_bytes(bytes[off + 4..off + 6].try_into().unwrap()) == 2 {
                parents.insert(u64::from_le_bytes(bytes[off + 16..off + 24].try_into().unwrap()));
            }
            off += len;
        }
        if parents.len() > MAX_FOLDERS {
            return CatchUp::Lost;
        }
        if next <= start {
            break;
        }
        start = next;
    }
    CatchUp::Dirs(parents.into_iter().filter_map(|frn| path_of(&vol, frn)).collect())
}

/// The current path of a folder, by its file reference number. Folders
/// that no longer exist have none (their parent's change covers them).
fn path_of(vol: &Volume, frn: u64) -> Option<String> {
    let mut id = FILE_ID_DESCRIPTOR { dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: FileIdType, ..Default::default() };
    id.Anonymous.FileId = frn as i64;
    let h = unsafe {
        OpenFileById(vol.0, &id, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), FILE_FLAG_BACKUP_SEMANTICS)
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) } as usize;
    if n >= buf.len() {
        buf.resize(n + 1, 0);
        n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) } as usize;
    }
    unsafe { CloseHandle(h) };
    if n == 0 || n >= buf.len() {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    // `\\?\C:\dir` -> `C:\dir`; a drive root keeps its backslash.
    let p = path.strip_prefix(r"\\?\").unwrap_or(&path);
    Some(if p.len() > 3 { p.trim_end_matches('\\').to_string() } else { p.to_string() })
}

pub fn save(path: &Path, pos: Position) -> std::io::Result<()> {
    let tmp = path.with_extension("usn.tmp");
    std::fs::write(&tmp, format!("{} {}\n", pos.journal, pos.next))?;
    std::fs::rename(tmp, path)
}

pub fn load(path: &Path) -> Option<Position> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut it = text.split_whitespace();
    Some(Position { journal: it.next()?.parse().ok()?, next: it.next()?.parse().ok()? })
}
