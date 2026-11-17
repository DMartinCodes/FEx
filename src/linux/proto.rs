//! The index daemon's protocol, over a Unix stream socket: one request and
//! one reply per connection. Integers are little-endian; byte strings are a
//! u32 length followed by the bytes (paths are bytes, never decoded).
//!
//! ```text
//! request:  protocol u8 (`wire::PROTOCOL`), then
//!           op u8 = QUERY, regex u8, limit u32, scope bytes, query bytes,
//!           order u8 (NAME_ASC / NAME_DESC / UNORDERED), first u32,
//!           seq u64 (the client's query number: a higher one from the same
//!           process supersedes this query; 0 = never superseded)
//!           op u8 = STATUS
//! query reply:  OK, total u64, then chunks until one has `last` set
//!               NOT_COVERED, root bytes  |  ERROR, message bytes  |  BUSY, message bytes
//! chunk (one message each): last u8, new folders u32 x bytes,
//!               rows u32 x (folder u32, flags u8, name bytes)
//! status reply: OK, then the `Status` fields in order
//! ```
//! Streaming: the first chunk holds the `first` rows that sort first by the
//! requested order (a screenful), so the client can show them while the rest
//! arrive; folder ids count up across chunks, each path sent once.
//! Rows carry no sizes or dates: the client stats what it shows, as the
//! user, which is also the final permission check.

use std::io;
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

pub use crate::wire::{put_bytes, put_u32, put_u64, Reader};

pub fn send(stream: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    crate::wire::send(stream, body)
}

pub fn recv(stream: &mut UnixStream, max: usize) -> io::Result<Vec<u8>> {
    crate::wire::recv(stream, max)
}

// ------------------------------------------------------------ client

/// Row order the client wants first (see `QUERY`).
pub const NAME_ASC: u8 = 0;
pub const NAME_DESC: u8 = 1;
pub const UNORDERED: u8 = 2;

pub enum Reply {
    /// The first chunk and the exact match count; `rest` yields the others.
    Rows { rows: Vec<Row>, total: usize, rest: Stream },
    /// The daemon's index doesn't cover the scope (its root is given).
    NotCovered(Vec<u8>),
    Busy(String),
    Error(String),
}

/// The remaining chunks of a streamed reply.
pub struct Stream {
    s: Option<UnixStream>,
    dirs: Vec<Arc<[u8]>>,
}

impl Stream {
    fn chunk(&mut self, body: &[u8], skip_header: bool) -> io::Result<Vec<Row>> {
        let mut r = Reader::new(body);
        if skip_header {
            r.u8()?;
            r.u64()?;
        }
        let last = r.u8()? != 0;
        let nd = r.u32()? as usize;
        for _ in 0..nd {
            self.dirs.push(r.bytes()?.into());
        }
        let n = r.u32()? as usize;
        let mut rows = Vec::with_capacity(n.min(MAX_LIMIT as usize));
        for _ in 0..n {
            let d = r.u32()? as usize;
            let flags = r.u8()?;
            let name = r.bytes()?;
            let dir = self.dirs.get(d).cloned().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad folder index"))?;
            rows.push(Row { name: name.into(), dir: Some(dir), size: 0, mtime: 0, flags, sel: false, id: 0 });
        }
        if last {
            self.s = None;
        }
        Ok(rows)
    }

    /// The next chunk, or `None` once the reply is complete.
    pub fn next(&mut self) -> io::Result<Option<Vec<Row>>> {
        let Some(s) = self.s.as_mut() else { return Ok(None) };
        let body = recv(s, usize::MAX >> 1)?;
        self.chunk(&body, false).map(Some)
    }

    /// Everything that's left, in one vector.
    pub fn collect_into(mut self, rows: &mut Vec<Row>) -> io::Result<()> {
        while let Some(more) = self.next()? {
            rows.extend(more);
        }
        Ok(())
    }
}

fn connect() -> io::Result<UnixStream> {
    let s = UnixStream::connect(socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(s)
}

/// Ask the daemon for matches under `scope`. The reply's first chunk holds
/// the `first` rows that come first in `order`. `Err` when there is no daemon.
pub fn query(scope: &[u8], text: &str, regex: bool, limit: u32, order: u8, first: u32, seq: u64) -> io::Result<Reply> {
    let mut s = connect()?;
    let mut req = vec![crate::wire::PROTOCOL, QUERY, regex as u8];
    put_u32(&mut req, limit.min(MAX_LIMIT));
    put_bytes(&mut req, scope);
    put_bytes(&mut req, text.as_bytes());
    req.push(order);
    put_u32(&mut req, first);
    put_u64(&mut req, seq);
    send(&mut s, &req)?;
    let body = recv(&mut s, usize::MAX >> 1)?;
    let mut r = Reader::new(&body);
    match r.u8()? {
        OK => {
            let total = r.u64()? as usize;
            let mut rest = Stream { s: Some(s), dirs: Vec::new() };
            let rows = rest.chunk(&body, true)?;
            Ok(Reply::Rows { rows, total, rest })
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
    /// The CPU reserve and background priority right now.
    pub reserve: String,
    /// Memory use and the memory level right now.
    pub memory: String,
    /// The daemon's FEx version.
    pub version: String,
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
    put_bytes(&mut out, st.reserve.as_bytes());
    put_bytes(&mut out, st.memory.as_bytes());
    put_bytes(&mut out, st.version.as_bytes());
    out
}

pub fn status() -> io::Result<Status> {
    let mut s = connect()?;
    send(&mut s, &[crate::wire::PROTOCOL, STATUS])?;
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
        reserve: r.bytes().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        memory: r.bytes().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        version: r.bytes().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
    })
}
