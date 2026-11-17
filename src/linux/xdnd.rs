//! Dragging files out of FEx to other programs: file managers, the desktop,
//! browsers, mail and chat clients, editors, other FEx windows (the Linux
//! side of `oledrag.rs`), through the XDND protocol every X11 toolkit speaks.
//!
//! When a drag of FEx's own items leaves its window, a thread takes over
//! with its own X connection. winit keeps the pointer's implicit grab, so
//! rather than fight for it the thread follows the pointer by querying it
//! (every 15 ms), finds the XDND-aware window under it, and talks to that
//! window: enter, position updates, then drop or leave when the button is
//! released. The files travel as `text/uri-list` (and plain paths for text
//! targets). Shift asks for a move, Ctrl for a copy; the receiving program
//! carries the operation out, as with any file-manager drag.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask, PropMode, SelectionNotifyEvent,
    Timestamp, Window, WindowClass, CLIENT_MESSAGE_EVENT, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::clip::to_uri;

const VERSION: u32 = 5;
const POLL: Duration = Duration::from_millis(15);
/// How long a target may take to answer a position or to finish a drop.
const STATUS_WAIT: Duration = Duration::from_millis(400);
const FINISH_WAIT: Duration = Duration::from_secs(3);

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndActionCopy,
        XdndActionMove,
        XdndActionList,
        XdndTypeList,
        URI_LIST: b"text/uri-list",
        UTF8_STRING,
        TEXT_PLAIN: b"text/plain",
        FEX_TIME,
        TARGETS,
    }
}

/// How a drag ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// A program took the files.
    Dropped,
    /// Released where nothing accepts files.
    Nowhere,
}

/// Run a drag of `paths` from the current pointer position until the
/// button is released. Blocks; run it on its own thread.
pub fn drag_files(paths: &[Vec<u8>]) -> Result<Outcome, String> {
    Drag::start(paths)?.run()
}

struct Drag {
    conn: RustConnection,
    root: Window,
    win: Window,
    atoms: Atoms,
    uris: Vec<u8>,
    text: Vec<u8>,
    time: Timestamp,
}

struct Target {
    /// The window the messages are about.
    window: Window,
    /// Where they are sent (its XdndProxy, or itself).
    deliver: Window,
    accepted: bool,
    /// A position was sent and not yet answered (since this instant).
    waiting: Option<Instant>,
}

impl Drag {
    fn start(paths: &[Vec<u8>]) -> Result<Drag, String> {
        let (conn, screen) = RustConnection::connect(None).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(0, win, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))
            .map_err(|e| e.to_string())?;
        let atoms = Atoms::new(&conn).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        let uris = paths.iter().map(|p| format!("{}\r\n", to_uri(p))).collect::<String>().into_bytes();
        let text = paths.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join("\n").into_bytes();
        let mut d = Drag { conn, root, win, atoms, uris, text, time: CURRENT_TIME };
        d.time = d.timestamp();
        let a = &d.atoms;
        d.conn.change_property32(PropMode::REPLACE, d.win, a.XdndActionList, AtomEnum::ATOM, &[a.XdndActionCopy, a.XdndActionMove]).map_err(|e| e.to_string())?;
        d.conn.change_property32(PropMode::REPLACE, d.win, a.XdndTypeList, AtomEnum::ATOM, &[a.URI_LIST, a.UTF8_STRING, a.TEXT_PLAIN]).map_err(|e| e.to_string())?;
        d.conn.set_selection_owner(d.win, a.XdndSelection, d.time).map_err(|e| e.to_string())?;
        d.conn.flush().map_err(|e| e.to_string())?;
        Ok(d)
    }

    /// A server timestamp (from a zero-length property change).
    fn timestamp(&mut self) -> Timestamp {
        let _ = self.conn.change_property8(PropMode::APPEND, self.win, self.atoms.FEX_TIME, AtomEnum::STRING, &[]);
        let _ = self.conn.flush();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(p))) if p.window == self.win => return p.time,
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => break,
            }
        }
        CURRENT_TIME
    }

    fn prop32(&self, w: Window, atom: Atom) -> Option<Vec<u32>> {
        let r = self.conn.get_property(false, w, atom, AtomEnum::ANY, 0, 16).ok()?.reply().ok()?;
        let v: Vec<u32> = r.value32()?.collect();
        (!v.is_empty()).then_some(v)
    }

    /// The deepest XDND-aware window under the pointer, descending from the
    /// root through window-manager frames.
    fn target_at(&self, start: Window) -> Option<(Window, Window, u32)> {
        let mut w = start;
        for _ in 0..16 {
            if w != self.root
                && let Some(v) = self.prop32(w, self.atoms.XdndAware)
            {
                let deliver = self.prop32(w, self.atoms.XdndProxy).map_or(w, |p| p[0]);
                return Some((w, deliver, v[0].min(VERSION)));
            }
            let child = self.conn.query_pointer(w).ok()?.reply().ok()?.child;
            if child == NONE {
                return None;
            }
            w = child;
        }
        None
    }

    fn send(&self, t: &Target, kind: Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent { response_type: CLIENT_MESSAGE_EVENT, format: 32, sequence: 0, window: t.window, type_: kind, data: data.into() };
        let _ = self.conn.send_event(false, t.deliver, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    fn run(mut self) -> Result<Outcome, String> {
        let a = self.atoms;
        let mut target: Option<Target> = None;
        let mut last_pos = (i32::MIN, i32::MIN, NONE);
        loop {
            self.pump(&mut target);
            let q = self.conn.query_pointer(self.root).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
            let (x, y) = (q.root_x as i32, q.root_y as i32);
            let action = if q.mask.contains(KeyButMask::SHIFT) { a.XdndActionMove } else { a.XdndActionCopy };
            let found = self.target_at(self.root);
            // A different window under the pointer: leave the old, enter the new.
            if found.map(|f| f.0) != target.as_ref().map(|t| t.window) {
                if let Some(t) = target.take() {
                    self.send(&t, a.XdndLeave, [self.win, 0, 0, 0, 0]);
                }
                if let Some((window, deliver, version)) = found {
                    let t = Target { window, deliver, accepted: false, waiting: None };
                    self.send(&t, a.XdndEnter, [self.win, version << 24, a.URI_LIST, a.UTF8_STRING, a.TEXT_PLAIN]);
                    target = Some(t);
                    last_pos = (i32::MIN, i32::MIN, NONE);
                }
            }
            let released = !q.mask.contains(KeyButMask::BUTTON1);
            if released {
                return self.finish(target);
            }
            if let Some(t) = target.as_mut() {
                let overdue = t.waiting.is_some_and(|s| s.elapsed() > STATUS_WAIT);
                if (t.waiting.is_none() || overdue) && (x, y, action) != last_pos {
                    let ev = ClientMessageEvent {
                        response_type: CLIENT_MESSAGE_EVENT,
                        format: 32,
                        sequence: 0,
                        window: t.window,
                        type_: a.XdndPosition,
                        data: [self.win, 0, ((x as u32) << 16) | (y as u32 & 0xffff), self.time, action].into(),
                    };
                    let _ = self.conn.send_event(false, t.deliver, EventMask::NO_EVENT, ev);
                    let _ = self.conn.flush();
                    t.waiting = Some(Instant::now());
                    last_pos = (x, y, action);
                }
            }
            std::thread::sleep(POLL);
        }
    }

    /// Handle what arrived: status replies, and requests for the data.
    fn pump(&mut self, target: &mut Option<Target>) {
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            self.event(ev, target);
        }
    }

    fn event(&mut self, ev: Event, target: &mut Option<Target>) {
        let a = self.atoms;
        match ev {
            Event::ClientMessage(m) if m.type_ == a.XdndStatus => {
                let d = m.data.as_data32();
                if let Some(t) = target.as_mut().filter(|t| t.window == d[0]) {
                    t.accepted = d[1] & 1 != 0;
                    t.waiting = None;
                }
            }
            Event::SelectionRequest(req) if req.selection == a.XdndSelection => {
                let property = if req.property == NONE { req.target } else { req.property };
                let ok = if req.target == a.URI_LIST {
                    self.conn.change_property8(PropMode::REPLACE, req.requestor, property, req.target, &self.uris).is_ok()
                } else if req.target == a.UTF8_STRING || req.target == a.TEXT_PLAIN || req.target == u32::from(AtomEnum::STRING) {
                    self.conn.change_property8(PropMode::REPLACE, req.requestor, property, req.target, &self.text).is_ok()
                } else if req.target == a.TARGETS {
                    self.conn.change_property32(PropMode::REPLACE, req.requestor, property, AtomEnum::ATOM, &[a.TARGETS, a.URI_LIST, a.UTF8_STRING, a.TEXT_PLAIN]).is_ok()
                } else {
                    false
                };
                let n = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: if ok { property } else { NONE },
                };
                let _ = self.conn.send_event(false, req.requestor, EventMask::NO_EVENT, n);
                let _ = self.conn.flush();
            }
            _ => {}
        }
    }

    /// The button was released: drop where accepted, else leave.
    fn finish(mut self, mut target: Option<Target>) -> Result<Outcome, String> {
        let a = self.atoms;
        // Give a pending status a moment, so a quick release still drops.
        let deadline = Instant::now() + STATUS_WAIT;
        while target.as_ref().is_some_and(|t| t.waiting.is_some()) && Instant::now() < deadline {
            self.pump(&mut target);
            std::thread::sleep(Duration::from_millis(5));
        }
        let Some(t) = target.take() else { return Ok(Outcome::Nowhere) };
        if !t.accepted {
            self.send(&t, a.XdndLeave, [self.win, 0, 0, 0, 0]);
            return Ok(Outcome::Nowhere);
        }
        self.send(&t, a.XdndDrop, [self.win, 0, self.time, 0, 0]);
        // Serve the target's data requests until it says it's done.
        let deadline = Instant::now() + FINISH_WAIT;
        let mut target = Some(t);
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::ClientMessage(m))) if m.type_ == a.XdndFinished => return Ok(Outcome::Dropped),
                Ok(Some(ev)) => self.event(ev, &mut target),
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(e.to_string()),
            }
        }
        // Old targets (version < 2) never send Finished; the data was served.
        Ok(Outcome::Dropped)
    }
}

impl Drop for Drag {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

/// Where the pointer is over FEx's window, in points, with the keyboard
/// modifiers held: for drops from other programs, while winit reports no
/// pointer motion (XDND carries it to the target only). One connection,
/// kept for later calls.
pub fn pointer(ctx: &eframe::egui::Context) -> Option<(eframe::egui::Pos2, eframe::egui::Modifiers)> {
    use std::sync::{Mutex, OnceLock};
    static CONN: OnceLock<Option<Mutex<(RustConnection, Window)>>> = OnceLock::new();
    let conn = CONN
        .get_or_init(|| {
            let (c, screen) = RustConnection::connect(None).ok()?;
            let root = c.setup().roots[screen].root;
            Some(Mutex::new((c, root)))
        })
        .as_ref()?;
    let guard = conn.lock().ok()?;
    let q = guard.0.query_pointer(guard.1).ok()?.reply().ok()?;
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let ppp = ctx.pixels_per_point();
    let pos = eframe::egui::pos2(q.root_x as f32 / ppp - inner.min.x, q.root_y as f32 / ppp - inner.min.y);
    let mods = eframe::egui::Modifiers { ctrl: q.mask.contains(KeyButMask::CONTROL), shift: q.mask.contains(KeyButMask::SHIFT), ..Default::default() };
    Some((pos, mods))
}
