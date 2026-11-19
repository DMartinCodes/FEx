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
//!
//! NTFS only: ReFS (Dev Drives among others) logs version 3 records with
//! 128-bit file IDs, which this reader doesn't parse. Rather than follow such
//! a journal and silently miss changes (permission changes included), its
//! volumes count as having no journal: crawled at each start, and checked
//! folder by folder at query time.

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

/// A volume shared by the pool's threads during a catch-up.
struct SharedVolume<'a>(&'a Volume);

// SAFETY: a volume handle may be used from several threads at once
// (OpenFileById and DeviceIoControl take it as a plain argument).
unsafe impl Sync for SharedVolume<'_> {}

impl SharedVolume<'_> {
    // A method, so closures capture the wrapper rather than its field.
    fn volume(&self) -> &Volume {
        self.0
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `C:\` -> the volume device `\\.\C:`.
/// Whether the volume at `root` (`C:\`) is NTFS.
fn is_ntfs(letter: char) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = wide(&format!(r"{letter}:\"));
    let mut fs = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as u32)
    };
    let len = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    ok != 0 && String::from_utf16_lossy(&fs[..len]) == "NTFS"
}

fn open_volume(root: &str) -> Option<Volume> {
    let letter = root.chars().next().filter(|c| c.is_ascii_alphabetic() && root[1..].starts_with(':'))?;
    if !is_ntfs(letter) {
        return None;
    }
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

/// One journal record (the fields the service uses).
pub struct Record {
    pub parent: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Read records from `from` up to `until`, those with any of `reasons`.
/// Returns where reading stopped, or `None` if the journal can't be read
/// from there (recreated, or wrapped past `from`).
fn read_records(vol: &Volume, journal: u64, from: i64, until: i64, reasons: u32, mut each: impl FnMut(Record)) -> Option<i64> {
    let mut start = from;
    let mut buf = vec![0u64; 8 * 1024]; // 64 KB, 8-byte aligned
    while start < until {
        let req = READ_USN_JOURNAL_DATA_V0 { StartUsn: start, ReasonMask: reasons, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: journal };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(vol.0, FSCTL_READ_USN_JOURNAL, &req as *const _ as *const _, size_of::<READ_USN_JOURNAL_DATA_V0>() as u32, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut got, std::ptr::null_mut())
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: `got` bytes were written by the driver.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, got as usize) };
        if bytes.len() < 8 {
            break;
        }
        let next = i64::from_le_bytes(bytes[..8].try_into().unwrap());
        // USN_RECORD_V2: RecordLength u32 @0, MajorVersion u16 @4,
        // ParentFileReferenceNumber u64 @16, Reason u32 @40,
        // FileAttributes u32 @52, FileNameLength u16 @56, FileNameOffset u16 @58.
        let mut off = 8;
        while off + 60 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            if len < 60 || off + len > bytes.len() {
                break;
            }
            let r = &bytes[off..off + len];
            if u16::from_le_bytes(r[4..6].try_into().unwrap()) == 2 {
                let (nlen, noff) = (u16::from_le_bytes(r[56..58].try_into().unwrap()) as usize, u16::from_le_bytes(r[58..60].try_into().unwrap()) as usize);
                let name = r.get(noff..noff + nlen).map(|n| {
                    let units: Vec<u16> = n.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    String::from_utf16_lossy(&units)
                });
                each(Record {
                    parent: u64::from_le_bytes(r[16..24].try_into().unwrap()),
                    reason: u32::from_le_bytes(r[40..44].try_into().unwrap()),
                    attrs: u32::from_le_bytes(r[52..56].try_into().unwrap()),
                    name: name.unwrap_or_default(),
                });
            }
            off += len;
        }
        if next <= start {
            break;
        }
        start = next;
    }
    Some(start)
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
    let mut overflow = false;
    let read = read_records(&vol, since.journal, since.next, now.NextUsn, u32::MAX, |r| {
        if parents.len() <= MAX_FOLDERS {
            parents.insert(r.parent);
        } else {
            overflow = true;
        }
    });
    if read.is_none() || overflow {
        return CatchUp::Lost;
    }
    // Resolving each folder is a file open: spread them over the pool.
    let frns: Vec<u64> = parents.into_iter().collect();
    let found: Vec<std::sync::Mutex<Vec<String>>> = (0..frns.len().div_ceil(256)).map(|_| Default::default()).collect();
    let shared = SharedVolume(&vol);
    crate::pool::for_each(found.len(), &|c| {
        let mine: Vec<String> = frns[c * 256..((c + 1) * 256).min(frns.len())].iter().filter_map(|&frn| path_of(shared.volume(), frn)).collect();
        *found[c].lock().unwrap_or_else(std::sync::PoisonError::into_inner) = mine;
    });
    CatchUp::Dirs(found.into_iter().flat_map(|m| m.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)).collect())
}

/// Follows a volume's journal from now on (for cache invalidation).
pub struct Tail {
    vol: Volume,
    pos: Position,
}

pub enum Polled {
    Records(Vec<(Record, Option<String>)>),
    /// Records were lost (the journal was recreated): assume anything changed.
    Lost,
}

impl Tail {
    pub fn start(root: &str) -> Option<Tail> {
        let vol = open_volume(root)?;
        let d = query(&vol)?;
        Some(Tail { vol, pos: Position { journal: d.UsnJournalID, next: d.NextUsn } })
    }

    /// Records with any of `reasons` since the last poll (`dirs_only`: of
    /// folders only, before any path is resolved), each with its parent
    /// folder's current path when it still exists.
    pub fn poll(&mut self, reasons: u32, dirs_only: bool) -> Polled {
        let Some(now) = query(&self.vol) else { return Polled::Lost };
        if now.UsnJournalID != self.pos.journal || self.pos.next < now.FirstUsn {
            self.pos = Position { journal: now.UsnJournalID, next: now.NextUsn };
            return Polled::Lost;
        }
        let mut out = Vec::new();
        const DIRECTORY: u32 = 0x10;
        match read_records(&self.vol, self.pos.journal, self.pos.next, now.NextUsn, reasons, |r| {
            if !dirs_only || r.attrs & DIRECTORY != 0 {
                out.push(r);
            }
        }) {
            Some(next) => self.pos.next = next.max(self.pos.next),
            None => {
                self.pos = Position { journal: now.UsnJournalID, next: now.NextUsn };
                return Polled::Lost;
            }
        }
        let mut parents: std::collections::HashMap<u64, Option<String>> = std::collections::HashMap::new();
        let records = out
            .into_iter()
            .map(|r| {
                let p = parents.entry(r.parent).or_insert_with(|| path_of(&self.vol, r.parent)).clone();
                (r, p)
            })
            .collect();
        Polled::Records(records)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Journal positions saved by released versions must keep loading (or
    /// the service re-crawls instead of catching up).
    #[test]
    fn loads_released_position_format() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("formats").join("windows-usn-position.usn");
        let pos = load(&p).expect("the saved journal position no longer loads");
        assert_eq!((pos.journal, pos.next), (133456789012, 987654321));
    }

    /// Runs where the journal is readable (an administrator on NTFS, as on
    /// CI runners); elsewhere it reports and passes.
    #[test]
    fn catches_up_and_tails_folder_changes() {
        let base = std::env::temp_dir().join(format!("fex-usn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // The temp folder may be given in 8.3 form (RUNNER~1); the journal
        // resolves full names, as the index (crawled from the root) stores.
        let full = std::fs::canonicalize(&base).unwrap();
        let full = full.to_string_lossy();
        let base_s = full.strip_prefix(r"\\?\").unwrap_or(&full).to_string();
        let root = format!("{}\\", &base_s[..2]);
        let Some(pos) = position(&root) else {
            eprintln!("no readable change journal on {root}; skipped");
            return;
        };
        let mut tail = Tail::start(&root).expect("journal readable");
        std::fs::create_dir(base.join("made_after")).unwrap();
        std::fs::write(base.join("made_after").join("x.txt"), b"1").unwrap();
        match changed_since(&root, pos) {
            CatchUp::Dirs(dirs) => {
                let want = base_s.to_lowercase();
                assert!(dirs.iter().any(|d| d.to_lowercase() == want), "{want} not among {} changed folders", dirs.len());
            }
            CatchUp::Lost => panic!("journal lost"),
        }
        let created = 0x100; // USN_REASON_FILE_CREATE
        match tail.poll(created, true) {
            Polled::Records(rs) => assert!(
                rs.iter().any(|(r, p)| r.name == "made_after" && r.attrs & 0x10 != 0 && p.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&base_s))),
                "folder creation not seen in {} records",
                rs.len()
            ),
            Polled::Lost => panic!("tail lost"),
        }
        let _ = std::fs::remove_dir_all(base);
    }
}
