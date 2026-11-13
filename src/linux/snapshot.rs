//! Memory-mappable index snapshots.
//!
//! The index's arrays hold no pointers (names in one arena, offsets into
//! it), so a snapshot is those arrays written to a file as they are. Loading
//! maps the file and points the index at it: no parsing, no copying, and the
//! kernel's page cache keeps it warm between processes.
//!
//! Every offset is validated once at load, so a damaged or truncated file is
//! rejected instead of crashing a query later. Files are written to a temp
//! name, synced, then renamed over the old one, so a crash never leaves half
//! a snapshot and an existing mapping of the old file stays valid.
//!
//! Layout (little-endian, the native order on x86-64 and arm64):
//! ```text
//! 0   magic "FEXSNAP1"   8  version u32   12 endian marker u32
//! 16  created ns u64     24 entries u64   32 dirs u64   40 errors u64
//! 48  build ns u64       56 section count u32, reserved u32
//! 64  sections: (offset u64, length u64) x SECTIONS, data 64-byte aligned
//! ```

use std::fs::File;
use std::io::{self, Write};
use std::ops::Deref;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"FEXSNAP1";
const VERSION: u32 = 1;
const ENDIAN: u32 = 0x0A0B_0C0D;
const HEADER: usize = 64;
pub const SECTIONS: usize = 10;
const ALIGN: usize = 64;

/// Plain data that can be viewed straight from file bytes.
///
/// # Safety
/// Every bit pattern must be a valid value, with no padding.
pub unsafe trait Pod: Copy + 'static {}
unsafe impl Pod for u8 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for i64 {}

/// A read-only mapping of a whole file, unmapped on drop.
pub struct Mapping {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapping is read-only and never changes while mapped (snapshot
// files are replaced by rename, never rewritten in place).
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

impl Mapping {
    pub fn open(path: &Path) -> io::Result<Mapping> {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Mapping { ptr: std::ptr::NonNull::dangling().as_ptr(), len: 0 });
        }
        use std::os::fd::AsRawFd;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // The whole file is validated right away, so read it in ahead.
        unsafe { libc::madvise(ptr, len, libc::MADV_WILLNEED) };
        Ok(Mapping { ptr: ptr as *const u8, len })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// An index array: owned (built in memory) or a section of a mapped snapshot.
pub enum Buf<T: Pod> {
    Owned(Vec<T>),
    Mapped { map: Arc<Mapping>, off: usize, len: usize },
}

impl<T: Pod> Default for Buf<T> {
    fn default() -> Self {
        Buf::Owned(Vec::new())
    }
}

impl<T: Pod> Deref for Buf<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        match self {
            Buf::Owned(v) => v,
            // SAFETY: `load` checked that the section lies inside the mapping
            // and is aligned for T (the mapping itself is page-aligned).
            Buf::Mapped { map, off, len } => unsafe { std::slice::from_raw_parts(map.ptr.add(*off) as *const T, *len) },
        }
    }
}

impl<T: Pod> Buf<T> {
    /// For appending; a mapped array is copied into memory first.
    pub fn vec(&mut self) -> &mut Vec<T> {
        if let Buf::Mapped { .. } = self {
            *self = Buf::Owned(self.to_vec());
        }
        match self {
            Buf::Owned(v) => v,
            Buf::Mapped { .. } => unreachable!(),
        }
    }

    /// Heap bytes owned (mapped arrays live in the page cache instead).
    pub fn heap_bytes(&self) -> usize {
        match self {
            Buf::Owned(v) => v.capacity() * size_of::<T>(),
            Buf::Mapped { .. } => 0,
        }
    }

    pub fn mapped_bytes(&self) -> usize {
        match self {
            Buf::Owned(_) => 0,
            Buf::Mapped { len, .. } => len * size_of::<T>(),
        }
    }
}

fn as_bytes<T: Pod>(v: &[T]) -> &[u8] {
    // SAFETY: Pod types have no padding.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, size_of_val(v)) }
}

/// Header fields besides the arrays.
#[derive(Clone, Copy, Default, Debug)]
pub struct Info {
    pub created_ns: u64,
    pub entries: u64,
    pub dirs: u64,
    pub errors: u64,
    pub build_ns: u64,
}

/// Write `sections` (raw bytes, in order) to `path` atomically.
pub fn write(path: &Path, info: Info, sections: &[&[u8]; SECTIONS]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    let mut header = Vec::with_capacity(HEADER + SECTIONS * 16);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&ENDIAN.to_le_bytes());
    for v in [info.created_ns, info.entries, info.dirs, info.errors, info.build_ns] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    header.extend_from_slice(&(SECTIONS as u32).to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    let mut off = (HEADER + SECTIONS * 16).next_multiple_of(ALIGN);
    let mut layout = Vec::new();
    for s in sections {
        header.extend_from_slice(&(off as u64).to_le_bytes());
        header.extend_from_slice(&(s.len() as u64).to_le_bytes());
        layout.push(off);
        off = (off + s.len()).next_multiple_of(ALIGN);
    }
    let mut w = io::BufWriter::with_capacity(1 << 20, &mut f);
    w.write_all(&header)?;
    let mut pos = header.len();
    for (s, &at) in sections.iter().zip(&layout) {
        w.write_all(&vec![0u8; at - pos])?;
        w.write_all(s)?;
        pos = at + s.len();
    }
    w.flush()?;
    drop(w);
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn section<T: Pod>(v: &[T]) -> &[u8] {
    as_bytes(v)
}

/// A loaded snapshot: header fields and the location of each section.
pub struct Loaded {
    pub map: Arc<Mapping>,
    pub info: Info,
    sections: [(usize, usize); SECTIONS],
}

impl Loaded {
    pub fn open(path: &Path) -> Result<Loaded, String> {
        let map = Arc::new(Mapping::open(path).map_err(|e| format!("{}: {e}", path.display()))?);
        let b = map.bytes();
        let bad = |why: &str| format!("{}: not a usable snapshot ({why})", path.display());
        if b.len() < HEADER + SECTIONS * 16 || &b[..8] != MAGIC {
            return Err(bad("bad header"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if u32_at(8) != VERSION {
            return Err(bad("other version"));
        }
        if u32::from_ne_bytes(b[12..16].try_into().unwrap()) != ENDIAN {
            return Err(bad("other byte order"));
        }
        if u32_at(56) as usize != SECTIONS {
            return Err(bad("section count"));
        }
        let info = Info { created_ns: u64_at(16), entries: u64_at(24), dirs: u64_at(32), errors: u64_at(40), build_ns: u64_at(48) };
        let mut sections = [(0, 0); SECTIONS];
        for (i, s) in sections.iter_mut().enumerate() {
            let (off, len) = (u64_at(HEADER + i * 16) as usize, u64_at(HEADER + i * 16 + 8) as usize);
            if off % 8 != 0 || off.checked_add(len).is_none_or(|end| end > b.len()) {
                return Err(bad("section out of bounds"));
            }
            *s = (off, len);
        }
        Ok(Loaded { map, info, sections })
    }

    /// Section `i` as a typed array (its length must divide evenly).
    pub fn buf<T: Pod>(&self, i: usize) -> Result<Buf<T>, String> {
        let (off, len) = self.sections[i];
        if len % size_of::<T>() != 0 {
            return Err(format!("snapshot section {i} has a partial element"));
        }
        Ok(Buf::Mapped { map: self.map.clone(), off, len: len / size_of::<T>() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejects_damage() {
        let dir = std::env::temp_dir().join(format!("fex-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.snap");
        let words: Vec<u32> = vec![1, 2, 3];
        let times: Vec<i64> = vec![-5, 7];
        let empty: &[u8] = &[];
        let mut secs: [&[u8]; SECTIONS] = [empty; SECTIONS];
        secs[0] = b"/root";
        secs[3] = section(&words);
        secs[9] = section(&times);
        write(&path, Info { entries: 3, ..Default::default() }, &secs).unwrap();
        let l = Loaded::open(&path).unwrap();
        assert_eq!(l.info.entries, 3);
        assert_eq!(&*l.buf::<u8>(0).unwrap(), b"/root");
        assert_eq!(&*l.buf::<u32>(3).unwrap(), &[1, 2, 3]);
        assert_eq!(&*l.buf::<i64>(9).unwrap(), &[-5, 7]);
        assert!(l.buf::<u32>(0).is_err()); // 5 bytes is not whole u32s

        let mut raw = std::fs::read(&path).unwrap();
        raw.truncate(raw.len() - 8);
        std::fs::write(&path, &raw).unwrap();
        assert!(Loaded::open(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
