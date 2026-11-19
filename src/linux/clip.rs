//! File clipboard on X11, in the formats the desktop's file managers use,
//! so copy/cut/paste works between FEx and Nautilus, Nemo, Caja, Thunar,
//! Dolphin and PCManFM (the Linux side of `clip.rs`):
//! * `x-special/gnome-copied-files`: `copy` or `cut`, then one file URI per line;
//! * `text/uri-list` (with `application/x-kde-cutselection` for KDE's cut);
//! * plain text (the paths), for pasting into a terminal or editor.
//!
//! A thread of its own owns an X connection and an invisible window. It
//! answers other programs' requests while FEx holds the clipboard, and
//! follows ownership changes through XFixes, so `has_files` and `sequence`
//! cost nothing on the UI thread. Reads from another program wait at most a
//! second, so a hung owner can't freeze FEx.

use std::os::fd::AsRawFd;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property, SelectionNotifyEvent,
    SelectionRequestEvent, Timestamp, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

const READ_TIMEOUT: Duration = Duration::from_secs(1);

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        TIMESTAMP,
        INCR,
        UTF8_STRING,
        TEXT,
        TEXT_PLAIN: b"text/plain",
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        URI_LIST: b"text/uri-list",
        GNOME_FILES: b"x-special/gnome-copied-files",
        KDE_CUT: b"application/x-kde-cutselection",
        FEX_DATA,
        FEX_TARGETS,
        FEX_TIME,
    }
}

/// Percent-encode a path as a `file://` URI.
pub fn to_uri(path: &[u8]) -> String {
    let mut s = String::from("file://");
    for &b in path {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// The path of a `file://` URI (`file:/x`, `file:///x` and `file://localhost/x`).
pub fn from_uri(uri: &[u8]) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix(b"file://").or_else(|| uri.strip_prefix(b"file:"))?;
    let rest = rest.strip_prefix(b"localhost").unwrap_or(rest);
    if !rest.starts_with(b"/") {
        return None; // another host
    }
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if rest[i] == b'%'
            && i + 2 < rest.len()
            && let (Some(h), Some(l)) = (hex(rest[i + 1]), hex(rest[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(rest[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Paths from a `text/uri-list` (comments and other schemes skipped).
pub fn parse_uri_list(data: &[u8]) -> Vec<Vec<u8>> {
    data.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).filter(|l| !l.is_empty() && !l.starts_with(b"#")).filter_map(from_uri).collect()
}

/// Files and whether they were cut, from `x-special/gnome-copied-files`.
fn parse_gnome(data: &[u8]) -> Option<(Vec<Vec<u8>>, bool)> {
    let mut lines = data.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let cut = match lines.next()? {
        b"cut" => true,
        b"copy" => false,
        _ => return None,
    };
    let files: Vec<Vec<u8>> = lines.filter_map(from_uri).collect();
    (!files.is_empty()).then_some((files, cut))
}

struct Held {
    files: Vec<Vec<u8>>,
    cut: bool,
    time: Timestamp,
}

#[derive(Default)]
struct Shared {
    /// Bumped whenever the clipboard changes hands.
    seq: u32,
    has_files: bool,
    /// The sequence number of FEx's own clipboard contents, while it holds them.
    ours: Option<u32>,
}

enum Cmd {
    Set { files: Vec<Vec<u8>>, cut: bool, reply: Sender<Result<u32, String>> },
    Files(Sender<Option<(Vec<Vec<u8>>, bool)>>),
    Text(Sender<Option<String>>),
    Release(u32),
}

struct Handle {
    tx: Mutex<Sender<Cmd>>,
    wake: i32,
    shared: Arc<Mutex<Shared>>,
}

static HANDLE: OnceLock<Option<Handle>> = OnceLock::new();

fn handle() -> Option<&'static Handle> {
    HANDLE
        .get_or_init(|| {
            let (tx, rx) = channel();
            let shared = Arc::new(Mutex::new(Shared::default()));
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
                return None;
            }
            let worker = Worker::connect(fds[0], rx, shared.clone()).ok()?;
            std::thread::Builder::new().name("clipboard".into()).spawn(move || worker.run()).ok()?;
            Some(Handle { tx: Mutex::new(tx), wake: fds[1], shared })
        })
        .as_ref()
}

fn send(cmd: Cmd) -> bool {
    let Some(h) = handle() else { return false };
    let ok = h.tx.lock().unwrap_or_else(PoisonError::into_inner).send(cmd).is_ok();
    unsafe { libc::write(h.wake, [1u8].as_ptr().cast(), 1) };
    ok
}

/// Put files on the clipboard (`cut`: the paste moves them). Returns the
/// clipboard sequence number for these contents.
pub fn set_files(files: &[Vec<u8>], cut: bool) -> Result<u32, String> {
    let (reply, rx) = channel();
    if !send(Cmd::Set { files: files.to_vec(), cut, reply }) {
        return Err("The clipboard is unavailable (no X11 connection)".into());
    }
    rx.recv_timeout(READ_TIMEOUT * 2).unwrap_or_else(|_| Err("The clipboard didn't respond".into()))
}

/// Files on the clipboard, and whether they were cut.
pub fn get_files() -> Option<(Vec<Vec<u8>>, bool)> {
    let (reply, rx) = channel();
    send(Cmd::Files(reply)).then_some(())?;
    rx.recv_timeout(READ_TIMEOUT * 3).ok().flatten()
}

/// Text on the clipboard, for pasting into text fields.
pub fn get_text() -> Option<String> {
    let (reply, rx) = channel();
    send(Cmd::Text(reply)).then_some(())?;
    rx.recv_timeout(READ_TIMEOUT * 3).ok().flatten()
}

/// Whether the clipboard offers files (kept current in the background).
pub fn has_files() -> bool {
    handle().is_some_and(|h| h.shared.lock().unwrap_or_else(PoisonError::into_inner).has_files)
}

/// Changes whenever something else is put on the clipboard.
pub fn sequence() -> u32 {
    handle().map_or(0, |h| h.shared.lock().unwrap_or_else(PoisonError::into_inner).seq)
}

/// After a cut-and-paste finished: empty the clipboard if it still holds
/// what was cut (as file managers do), so a second paste can't fail.
pub fn clear_if_unchanged(seq: u32) {
    send(Cmd::Release(seq));
}

struct Worker {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    wake: i32,
    rx: Receiver<Cmd>,
    shared: Arc<Mutex<Shared>>,
    held: Option<Held>,
    max_bytes: usize,
}

impl Worker {
    fn connect(wake: i32, rx: Receiver<Cmd>, shared: Arc<Mutex<Shared>>) -> Result<Worker, String> {
        let (conn, screen) = RustConnection::connect(None).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(0, win, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))
            .map_err(|e| e.to_string())?;
        let atoms = Atoms::new(&conn).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        conn.extension_information(xfixes::X11_EXTENSION_NAME).ok().flatten().ok_or("XFixes is missing")?;
        conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        let mask = SelectionEventMask::SET_SELECTION_OWNER | SelectionEventMask::SELECTION_WINDOW_DESTROY | SelectionEventMask::SELECTION_CLIENT_CLOSE;
        conn.xfixes_select_selection_input(win, atoms.CLIPBOARD, mask).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        let max_bytes = conn.maximum_request_bytes().saturating_sub(64);
        Ok(Worker { conn, win, atoms, wake, rx, shared, held: None, max_bytes })
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(mut self) {
        // Learn what's on the clipboard at start.
        self.query_targets();
        loop {
            let mut fds = [
                libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.wake, events: libc::POLLIN, revents: 0 },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            let mut drain = [0u8; 64];
            while unsafe { libc::read(self.wake, drain.as_mut_ptr().cast(), drain.len()) } > 0 {}
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(ev)) => self.event(ev),
                    Ok(None) => break,
                    Err(_) => return, // the X server went away
                }
            }
            while let Ok(cmd) = self.rx.try_recv() {
                self.command(cmd);
            }
            if self.conn.flush().is_err() {
                return;
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Set { files, cut, reply } => {
                let _ = reply.send(self.take_ownership(files, cut));
            }
            Cmd::Files(reply) => {
                let files = match &self.held {
                    Some(h) => Some((h.files.clone(), h.cut)),
                    None => self.read_files(),
                };
                let _ = reply.send(files);
            }
            Cmd::Text(reply) => {
                let text = match &self.held {
                    Some(h) => Some(h.files.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect::<Vec<_>>().join("\n")),
                    None => self.read(self.atoms.UTF8_STRING).or_else(|| self.read(AtomEnum::STRING.into())).map(|b| String::from_utf8_lossy(&b).into_owned()),
                };
                let _ = reply.send(text.filter(|t| !t.is_empty()));
            }
            Cmd::Release(seq) => {
                if self.shared().ours == Some(seq) && self.held.is_some() {
                    let _ = self.conn.set_selection_owner(NONE, self.atoms.CLIPBOARD, CURRENT_TIME);
                }
            }
        }
    }

    /// A server timestamp, from a zero-length property change (ICCCM asks
    /// for a real time when taking a selection).
    fn timestamp(&mut self) -> Timestamp {
        let _ = self.conn.change_property8(PropMode::APPEND, self.win, self.atoms.FEX_TIME, AtomEnum::STRING, &[]);
        let _ = self.conn.flush();
        let deadline = Instant::now() + READ_TIMEOUT;
        while let Some(ev) = self.wait_event(deadline) {
            if let Event::PropertyNotify(p) = &ev
                && p.atom == self.atoms.FEX_TIME
                && p.window == self.win
            {
                return p.time;
            }
            self.event(ev);
        }
        CURRENT_TIME
    }

    fn take_ownership(&mut self, files: Vec<Vec<u8>>, cut: bool) -> Result<u32, String> {
        let time = self.timestamp();
        self.conn.set_selection_owner(self.win, self.atoms.CLIPBOARD, time).map_err(|e| e.to_string())?;
        let owner = self.conn.get_selection_owner(self.atoms.CLIPBOARD).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        if owner.owner != self.win {
            return Err("Another program kept the clipboard".into());
        }
        self.held = Some(Held { files, cut, time });
        let mut s = self.shared();
        s.seq = s.seq.wrapping_add(1);
        s.ours = Some(s.seq);
        s.has_files = true;
        Ok(s.seq)
    }

    /// Wait for the next event until `deadline`.
    fn wait_event(&mut self, deadline: Instant) -> Option<Event> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(ev)) => return Some(ev),
                Ok(None) => {}
                Err(_) => return None,
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            let mut fd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as i32) };
        }
    }

    /// Ask the owner for the clipboard as `target`; the bytes, if it has them.
    fn read(&mut self, target: Atom) -> Option<Vec<u8>> {
        let prop = self.atoms.FEX_DATA;
        self.conn.delete_property(self.win, prop).ok()?;
        self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, target, prop, CURRENT_TIME).ok()?;
        self.conn.flush().ok()?;
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let ev = self.wait_event(deadline)?;
            match ev {
                Event::SelectionNotify(n) if n.requestor == self.win && n.selection == self.atoms.CLIPBOARD && n.target == target => {
                    if n.property == NONE {
                        return None;
                    }
                    return self.read_property(prop, deadline);
                }
                other => self.event(other),
            }
        }
    }

    /// Read (and delete) a property, following the INCR protocol for big ones.
    fn read_property(&mut self, prop: Atom, deadline: Instant) -> Option<Vec<u8>> {
        let reply = self.conn.get_property(true, self.win, prop, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()?;
        if reply.type_ != self.atoms.INCR {
            return Some(reply.value);
        }
        let _ = self.conn.flush();
        let mut out = Vec::new();
        loop {
            let ev = self.wait_event(deadline + READ_TIMEOUT * 4)?;
            match ev {
                Event::PropertyNotify(p) if p.window == self.win && p.atom == prop && p.state == Property::NEW_VALUE => {
                    let chunk = self.conn.get_property(true, self.win, prop, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()?;
                    if chunk.value.is_empty() {
                        return Some(out);
                    }
                    out.extend_from_slice(&chunk.value);
                    let _ = self.conn.flush();
                }
                other => self.event(other),
            }
        }
    }

    fn read_files(&mut self) -> Option<(Vec<Vec<u8>>, bool)> {
        if let Some(found) = self.read(self.atoms.GNOME_FILES).and_then(|d| parse_gnome(&d)) {
            return Some(found);
        }
        let files = parse_uri_list(&self.read(self.atoms.URI_LIST)?);
        if files.is_empty() {
            return None;
        }
        let cut = self.read(self.atoms.KDE_CUT).is_some_and(|d| d.first() == Some(&b'1'));
        Some((files, cut))
    }

    /// Ask the new owner which formats it offers (answered asynchronously).
    fn query_targets(&mut self) {
        let _ = self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, self.atoms.TARGETS, self.atoms.FEX_TARGETS, CURRENT_TIME);
        let _ = self.conn.flush();
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::SelectionRequest(req) => self.answer(req),
            Event::SelectionClear(c) if c.selection == self.atoms.CLIPBOARD => {
                self.held = None;
                self.shared().ours = None;
            }
            Event::XfixesSelectionNotify(n) if n.selection == self.atoms.CLIPBOARD => {
                let ours = n.owner == self.win && n.owner != NONE;
                {
                    let mut s = self.shared();
                    if !ours || s.ours.is_none() {
                        s.seq = s.seq.wrapping_add(1);
                    }
                    s.has_files = ours;
                    if !ours {
                        s.ours = None;
                    }
                }
                if !ours {
                    self.held = None;
                    if n.owner != NONE {
                        self.query_targets();
                    }
                }
            }
            Event::SelectionNotify(n) if n.requestor == self.win && n.property == self.atoms.FEX_TARGETS => {
                let targets: Vec<Atom> = self
                    .conn
                    .get_property(true, self.win, self.atoms.FEX_TARGETS, AtomEnum::ATOM, 0, 4096)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().map(|v| v.collect()))
                    .unwrap_or_default();
                let files = targets.iter().any(|&t| t == self.atoms.GNOME_FILES || t == self.atoms.URI_LIST);
                self.shared().has_files = files;
            }
            Event::SelectionNotify(n) if n.requestor == self.win && n.target == self.atoms.TARGETS && n.property == NONE => {
                self.shared().has_files = false;
            }
            _ => {}
        }
    }

    /// Serve another program's request for what FEx holds.
    fn answer(&mut self, req: SelectionRequestEvent) {
        let a = &self.atoms;
        let property = if req.property == NONE { req.target } else { req.property };
        let ok = match (&self.held, req.selection == a.CLIPBOARD) {
            (Some(h), true) => {
                let text_targets = [a.UTF8_STRING, a.TEXT, a.TEXT_PLAIN, a.TEXT_PLAIN_UTF8, AtomEnum::STRING.into()];
                if req.target == a.TARGETS {
                    let mut list = vec![a.TARGETS, a.TIMESTAMP, a.GNOME_FILES, a.URI_LIST, a.KDE_CUT];
                    list.extend(text_targets);
                    self.conn.change_property32(PropMode::REPLACE, req.requestor, property, AtomEnum::ATOM, &list).is_ok()
                } else if req.target == a.TIMESTAMP {
                    self.conn.change_property32(PropMode::REPLACE, req.requestor, property, AtomEnum::INTEGER, &[h.time]).is_ok()
                } else {
                    let data: Option<Vec<u8>> = if req.target == a.GNOME_FILES {
                        let mut s = (if h.cut { "cut" } else { "copy" }).to_string();
                        for f in &h.files {
                            s.push('\n');
                            s.push_str(&to_uri(f));
                        }
                        Some(s.into_bytes())
                    } else if req.target == a.URI_LIST {
                        Some(h.files.iter().map(|f| format!("{}\r\n", to_uri(f))).collect::<String>().into_bytes())
                    } else if req.target == a.KDE_CUT {
                        Some(if h.cut { b"1".to_vec() } else { b"0".to_vec() })
                    } else if text_targets.contains(&req.target) {
                        Some(h.files.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect::<Vec<_>>().join("\n").into_bytes())
                    } else {
                        None
                    };
                    // Contents beyond one request would need INCR; refuse rather than truncate.
                    match data.filter(|d| d.len() <= self.max_bytes) {
                        Some(d) => self.conn.change_property8(PropMode::REPLACE, req.requestor, property, req.target, &d).is_ok(),
                        None => false,
                    }
                }
            }
            _ => false,
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: if ok { property } else { NONE },
        };
        let _ = self.conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_any_bytes() {
        let p = b"/home/me/caf\xe9 & co/#1%.txt".to_vec();
        let uri = to_uri(&p);
        assert_eq!(uri, "file:///home/me/caf%E9%20&%20co/%231%25.txt");
        assert_eq!(from_uri(uri.as_bytes()), Some(p));
        assert_eq!(from_uri(b"file://localhost/x"), Some(b"/x".to_vec()));
        assert_eq!(from_uri(b"file://otherhost/x"), None);
        assert_eq!(from_uri(b"https://x/y"), None);
    }

    #[test]
    fn parses_file_manager_formats() {
        assert_eq!(parse_gnome(b"cut\nfile:///a\nfile:///b%20c"), Some((vec![b"/a".to_vec(), b"/b c".to_vec()], true)));
        assert_eq!(parse_gnome(b"copy\nfile:///a\n"), Some((vec![b"/a".to_vec()], false)));
        assert_eq!(parse_gnome(b"file:///a"), None);
        assert_eq!(parse_uri_list(b"# comment\r\nfile:///a\r\nhttp://x\r\nfile:///b\r\n"), vec![b"/a".to_vec(), b"/b".to_vec()]);
    }
}
