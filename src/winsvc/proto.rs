//! The index service's protocol, over the named pipe `\\.\pipe\FEx.Index`:
//! one request and one streamed reply per connection, framed as in
//! `wire.rs`. The layout follows the Linux daemon's (`linux/proto.rs`),
//! with UTF-8 paths and rows that carry their metadata (the Windows index
//! stores size, date and attributes with every name).
//!
//! ```text
//! request:  protocol u8 (`wire::PROTOCOL`), then
//!           op u8 = QUERY, regex u8, limit u32, scope bytes, query bytes,
//!           order u8 (NAME_ASC / NAME_DESC / UNORDERED), first u32,
//!           seq u64 (the client's query number: a higher one from the same
//!           process supersedes this query; 0 = never superseded)
//!           op u8 = STATUS
//! query reply:  OK, total u64, then chunks until one has `last` set
//!               NOT_COVERED, roots bytes ("\n"-separated) | ERROR, msg | BUSY, msg
//! chunk:    last u8, new folders u32 x bytes,
//!           rows u32 x (folder u32, attrs u32, size u64, mtime u64, name bytes)
//! ```
//! Before sending anything, the client checks that the pipe belongs to
//! SYSTEM or the Administrators group, so another user can't stand in for
//! the service (by creating the pipe first) and read the queries, and, when
//! the service is running, that the process serving this connection is the
//! service itself. (The pipe's ACL doesn't let users add instances of their
//! own, and every instance shares the first one's owner, so the owner check
//! alone couldn't tell a rogue instance apart.)

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{GetLastError, LocalFree, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION, PSID};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};

use crate::fsys::Row;
use crate::wire::{self, put_bytes, put_u32, put_u64, Reader};

pub const PIPE: &str = r"\\.\pipe\FEx.Index";

pub const QUERY: u8 = 1;
pub const STATUS: u8 = 2;

pub const OK: u8 = 0;
pub const ERROR: u8 = 1;
pub const NOT_COVERED: u8 = 2;
pub const BUSY: u8 = 3;

pub const NAME_ASC: u8 = 0;
pub const NAME_DESC: u8 = 1;
pub const UNORDERED: u8 = 2;

pub const MAX_REQUEST: usize = 64 * 1024;
/// Largest reply message a client accepts: real chunks are a few MB, so a
/// larger length means a broken or hostile server, not a reason to allocate.
const MAX_REPLY: usize = 256 << 20;
pub const MAX_LIMIT: u32 = 250_000;

/// Allow the service to impersonate the client (for its access checks).
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IMPERSONATION: u32 = 2 << 16;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether the pipe's owner is SYSTEM or the Administrators group.
fn owned_by_service(h: HANDLE) -> bool {
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let rc = unsafe { GetSecurityInfo(h, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd) };
    if rc != 0 {
        return false;
    }
    let ok = unsafe { IsWellKnownSid(owner, WinLocalSystemSid) != 0 || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0 };
    unsafe { LocalFree(sd) };
    ok
}

/// Whether this connection's server process is the running index service.
/// When the service isn't running (`fex service debug`), only the owner
/// check applies.
fn served_by_service(h: HANDLE) -> bool {
    let Some(service) = super::scm::running_pid() else { return true };
    let mut server = 0u32;
    unsafe { GetNamedPipeServerProcessId(h, &mut server) != 0 && server == service }
}

fn connect() -> io::Result<File> {
    let name = wide(PIPE);
    for attempt in 0..2 {
        let h = unsafe {
            CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION, std::ptr::null_mut())
        };
        if h != INVALID_HANDLE_VALUE {
            // SAFETY: a fresh handle we own; File closes it.
            let f = unsafe { File::from_raw_handle(h) };
            if !owned_by_service(f.as_raw_handle()) {
                return Err(io::Error::other("the index pipe isn't owned by the service"));
            }
            if !served_by_service(f.as_raw_handle()) {
                return Err(io::Error::other("the index pipe isn't served by the FEx index service"));
            }
            return Ok(f);
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY || attempt == 1 {
            return Err(io::Error::last_os_error());
        }
        // All instances busy for a moment (between connections): wait for one.
        unsafe { WaitNamedPipeW(name.as_ptr(), 2000) };
    }
    Err(io::Error::other("index pipe busy"))
}

pub enum Reply {
    /// The first chunk and the exact match count; `rest` yields the others.
    Rows { rows: Vec<Row>, total: usize, rest: Stream },
    /// The service doesn't index the scope (its roots are given).
    NotCovered(Vec<String>),
    Busy(String),
    Error(String),
}

pub struct Stream {
    pipe: Option<File>,
    dirs: Vec<Arc<str>>,
}

impl Stream {
    fn chunk(&mut self, body: &[u8], skip_header: bool) -> io::Result<Vec<Row>> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad reply");
        let mut r = Reader::new(body);
        if skip_header {
            r.u8()?;
            r.u64()?;
        }
        let last = r.u8()? != 0;
        for _ in 0..r.u32()? {
            self.dirs.push(std::str::from_utf8(r.bytes()?).map_err(|_| bad())?.into());
        }
        let n = r.u32()? as usize;
        let mut rows = Vec::with_capacity(n.min(MAX_LIMIT as usize));
        for _ in 0..n {
            let d = r.u32()? as usize;
            let attrs = r.u32()?;
            let size = r.u64()?;
            let mtime = r.u64()?;
            let name = std::str::from_utf8(r.bytes()?).map_err(|_| bad())?;
            let dir = self.dirs.get(d).cloned().ok_or_else(bad)?;
            rows.push(Row { name: name.into(), dir: Some(dir), size, mtime, attrs, sel: false });
        }
        if last {
            self.pipe = None;
        }
        Ok(rows)
    }

    /// The next chunk, or `None` once the reply is complete.
    pub fn next(&mut self) -> io::Result<Option<Vec<Row>>> {
        let Some(p) = self.pipe.as_mut() else { return Ok(None) };
        let body = wire::recv(p, MAX_REPLY)?;
        self.chunk(&body, false).map(Some)
    }
}

/// Ask the service for matches under `scope`. `Err` when it isn't running.
pub fn query(scope: &str, text: &str, regex: bool, limit: u32, order: u8, first: u32, seq: u64) -> io::Result<Reply> {
    let mut p = connect()?;
    let mut req = vec![wire::PROTOCOL, QUERY, regex as u8];
    put_u32(&mut req, limit.min(MAX_LIMIT));
    put_bytes(&mut req, scope.as_bytes());
    put_bytes(&mut req, text.as_bytes());
    req.push(order);
    put_u32(&mut req, first);
    put_u64(&mut req, seq);
    wire::send(&mut p, &req)?;
    let body = wire::recv(&mut p, MAX_REPLY)?;
    let mut r = Reader::new(&body);
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match r.u8()? {
        OK => {
            let total = r.u64()? as usize;
            let mut rest = Stream { pipe: Some(p), dirs: Vec::new() };
            let rows = rest.chunk(&body, true)?;
            Ok(Reply::Rows { rows, total, rest })
        }
        NOT_COVERED => Ok(Reply::NotCovered(text(r.bytes()?).lines().map(String::from).collect())),
        BUSY => Ok(Reply::Busy(text(r.bytes()?))),
        _ => Ok(Reply::Error(text(r.bytes()?))),
    }
}

#[derive(Debug, Default, Clone)]
pub struct Status {
    /// Drive roots the service indexes (`C:\`, ...).
    pub roots: Vec<String>,
    /// Roots with an index ready to answer.
    pub ready: Vec<String>,
    pub entries: u64,
    /// When the indexes were last crawled or caught up (ns since the epoch).
    pub verified_ns: u64,
    pub activity: String,
    pub paused: String,
    pub reserve: String,
    /// Memory use and the memory level right now.
    pub memory: String,
    /// The service's FEx version.
    pub version: String,
}

impl Status {
    pub fn covers(&self, path: &str) -> bool {
        self.ready.iter().any(|r| crate::index::in_scope(path, r))
    }
}

pub fn encode_status(st: &Status) -> Vec<u8> {
    let mut out = vec![OK];
    put_bytes(&mut out, st.roots.join("\n").as_bytes());
    put_bytes(&mut out, st.ready.join("\n").as_bytes());
    put_u64(&mut out, st.entries);
    put_u64(&mut out, st.verified_ns);
    for s in [&st.activity, &st.paused, &st.reserve, &st.memory, &st.version] {
        put_bytes(&mut out, s.as_bytes());
    }
    out
}

pub fn status() -> io::Result<Status> {
    let mut p = connect()?;
    wire::send(&mut p, &[wire::PROTOCOL, STATUS])?;
    let body = wire::recv(&mut p, MAX_REQUEST)?;
    let mut r = Reader::new(&body);
    if r.u8()? != OK {
        return Err(io::Error::other("status refused"));
    }
    let lines = |b: &[u8]| String::from_utf8_lossy(b).lines().map(String::from).collect::<Vec<_>>();
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    Ok(Status {
        roots: lines(r.bytes()?),
        ready: lines(r.bytes()?),
        entries: r.u64()?,
        verified_ns: r.u64()?,
        activity: text(r.bytes()?),
        paused: text(r.bytes()?),
        reserve: text(r.bytes()?),
        memory: r.bytes().map(text).unwrap_or_default(),
        version: r.bytes().map(text).unwrap_or_default(),
    })
}
