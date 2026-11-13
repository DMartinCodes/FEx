//! The index daemon's protocol, over a Unix stream socket: one request and
//! one reply per connection. Integers are little-endian; byte strings are a
//! u32 length followed by the bytes (paths are bytes, never decoded).
//!
//! ```text
//! request:  op u8 = QUERY, regex u8, limit u32, scope bytes, query bytes
//!           op u8 = STATUS
//! query reply:  OK, total u64, folders u32 x bytes, rows u32 x (folder u32, flags u8, name bytes)
//!               NOT_COVERED, root bytes  |  ERROR, message bytes  |  BUSY, message bytes
//! status reply: OK, then the `Status` fields in order
//! ```
//! Rows carry no sizes or dates: the client stats what it shows, as the
//! user, which is also the final permission check.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::fsys::Row;

pub const QUERY: u8 = 1;
pub const STATUS: u8 = 2;

pub const OK: u8 = 0;
pub const ERROR: u8 = 1;
pub const NOT_COVERED: u8 = 2;
/// The daemon has no index yet (first crawl running).
pub const BUSY: u8 = 3;

/// Requests larger than this are refused.
pub const MAX_REQUEST: usize = 64 * 1024;
pub const MAX_LIMIT: u32 = 250_000;

/// `$FEX_INDEXD_SOCKET`, else the system daemon's socket.
pub fn socket_path() -> PathBuf {
    std::env::var_os("FEX_INDEXD_SOCKET").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/fex/indexd.sock"))
}

// ------------------------------------------------------------ encoding

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// Reads fields from a received message; every read is bounds-checked.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated message"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// A whole message: u32 length, then the body.
pub fn send(stream: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    let mut msg = Vec::with_capacity(body.len() + 4);
    put_u32(&mut msg, body.len() as u32);
    msg.extend_from_slice(body);
    stream.write_all(&msg)
}

pub fn recv(stream: &mut UnixStream, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(body)
}

// ------------------------------------------------------------ client

pub enum Reply {
    Rows { rows: Vec<Row>, total: usize },
    /// The daemon's index doesn't cover the scope (its root is given).
    NotCovered(Vec<u8>),
    Busy(String),
    Error(String),
}

fn connect() -> io::Result<UnixStream> {
    let s = UnixStream::connect(socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(s)
}

/// Ask the daemon for matches under `scope`. `Err` when there is no daemon.
pub fn query(scope: &[u8], text: &str, regex: bool, limit: u32) -> io::Result<Reply> {
    let mut s = connect()?;
    let mut req = vec![QUERY, regex as u8];
    put_u32(&mut req, limit.min(MAX_LIMIT));
    put_bytes(&mut req, scope);
    put_bytes(&mut req, text.as_bytes());
    send(&mut s, &req)?;
    let body = recv(&mut s, usize::MAX >> 1)?;
    let mut r = Reader::new(&body);
    match r.u8()? {
        OK => {
            let total = r.u64()? as usize;
            let nd = r.u32()? as usize;
            let mut dirs: Vec<Arc<[u8]>> = Vec::with_capacity(nd.min(1 << 20));
            for _ in 0..nd {
                dirs.push(r.bytes()?.into());
            }
            let n = r.u32()? as usize;
            let mut rows = Vec::with_capacity(n.min(MAX_LIMIT as usize));
            for _ in 0..n {
                let d = r.u32()? as usize;
                let flags = r.u8()?;
                let name = r.bytes()?;
                let dir = dirs.get(d).cloned().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad folder index"))?;
                rows.push(Row { name: name.into(), dir: Some(dir), size: 0, mtime: 0, flags, sel: false });
            }
            Ok(Reply::Rows { rows, total })
        }
        NOT_COVERED => Ok(Reply::NotCovered(r.bytes()?.to_vec())),
        BUSY => Ok(Reply::Busy(String::from_utf8_lossy(r.bytes()?).into_owned())),
        _ => Ok(Reply::Error(String::from_utf8_lossy(r.bytes()?).into_owned())),
    }
}

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub root: Vec<u8>,
    pub ready: bool,
    pub entries: u64,
    pub dirs: u64,
    /// When the indexed data was crawled or last fully verified (ns since epoch).
    pub verified_ns: u64,
    pub generation: u64,
    pub mapped_bytes: u64,
    pub heap_bytes: u64,
    /// Filesystems watched with fanotify (else periodic sweeps only).
    pub watched: u32,
    /// What background work is doing ("idle", "crawling", "sweeping", ...).
    pub activity: String,
    /// Why background work is paused, if it is.
    pub paused: String,
}

pub fn encode_status(st: &Status) -> Vec<u8> {
    let mut out = vec![OK];
    put_bytes(&mut out, &st.root);
    out.push(st.ready as u8);
    for v in [st.entries, st.dirs, st.verified_ns, st.generation, st.mapped_bytes, st.heap_bytes] {
        put_u64(&mut out, v);
    }
    put_u32(&mut out, st.watched);
    put_bytes(&mut out, st.activity.as_bytes());
    put_bytes(&mut out, st.paused.as_bytes());
    out
}

pub fn status() -> io::Result<Status> {
    let mut s = connect()?;
    send(&mut s, &[STATUS])?;
    let body = recv(&mut s, MAX_REQUEST)?;
    let mut r = Reader::new(&body);
    if r.u8()? != OK {
        return Err(io::Error::other("status refused"));
    }
    Ok(Status {
        root: r.bytes()?.to_vec(),
        ready: r.u8()? != 0,
        entries: r.u64()?,
        dirs: r.u64()?,
        verified_ns: r.u64()?,
        generation: r.u64()?,
        mapped_bytes: r.u64()?,
        heap_bytes: r.u64()?,
        watched: r.u32()?,
        activity: String::from_utf8_lossy(r.bytes()?).into_owned(),
        paused: String::from_utf8_lossy(r.bytes()?).into_owned(),
    })
}
