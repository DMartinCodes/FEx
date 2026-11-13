//! Message framing shared by the index daemon (Linux, Unix socket) and the
//! index service (Windows, named pipe): little-endian integers, byte
//! strings as a u32 length plus the bytes, and a reader whose every read is
//! bounds-checked, so a malformed message is an error, never a panic.

use std::io::{self, Read, Write};

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

/// Send one message: u32 length, then the body.
pub fn send(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let mut msg = Vec::with_capacity(body.len() + 4);
    put_u32(&mut msg, body.len() as u32);
    msg.extend_from_slice(body);
    w.write_all(&msg)
}

/// Receive one message of at most `max` bytes.
pub fn recv(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_truncation() {
        let mut m = Vec::new();
        put_u32(&mut m, 7);
        put_bytes(&mut m, b"abc");
        put_u64(&mut m, u64::MAX);
        let mut r = Reader::new(&m);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.bytes().unwrap(), b"abc");
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert!(r.u8().is_err());
        let mut framed = Vec::new();
        send(&mut framed, &m).unwrap();
        assert_eq!(recv(&mut &framed[..], 1024).unwrap(), m);
        assert!(recv(&mut &framed[..], 4).is_err());
    }
}
