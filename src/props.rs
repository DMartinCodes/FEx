//! Properties window. Built to cost as little as possible:
//! * sizes and attributes of selected files come from the listing (no I/O);
//! * a single item costs one `GetFileAttributesEx` (plus one size-on-disk
//!   query for files), made off the UI thread so a slow share can't hang it;
//! * folder sizes are computed by a parallel walk that is cancelled when the
//!   window closes;
//! * permissions are only read when that section is expanded.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::panic::catch_unwind;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use eframe::egui::{self, Color32, RichText, Ui};

use crate::fileop::{self, Details};
use crate::fsys::{self, Row};
use crate::security::{self, AclInfo};
use crate::{walk, win};

#[derive(Default)]
struct Usage {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

pub struct Item {
    pub path: String,
    pub name: String,
    pub attrs: u32,
    pub size: u64,
}

/// Details of a single item, read on a worker: (details, size on disk).
type DetailsSlot = Arc<Mutex<Option<(Result<Details, String>, Option<u64>)>>>;

pub struct Props {
    items: Vec<Item>,
    /// Single item only: full details, size on disk.
    details: Option<Result<Details, String>>,
    on_disk: Option<u64>,
    pending: Option<DetailsSlot>,
    usage: Option<Arc<Usage>>,
    acl: Option<Arc<Mutex<Option<Result<AclInfo, String>>>>>,
    message: Option<String>,
}

impl Drop for Props {
    fn drop(&mut self) {
        if let Some(u) = &self.usage {
            u.cancel.store(true, Relaxed);
        }
    }
}

impl Props {
    pub fn new(items: Vec<Item>, ctx: egui::Context) -> Props {
        let mut p = Props { items, details: None, on_disk: None, pending: None, usage: None, acl: None, message: None };
        if let [one] = p.items.as_slice() {
            let slot: DetailsSlot = Arc::default();
            let (s, path, is_dir, ctx) = (slot.clone(), one.path.clone(), one.attrs & win::ATTR_DIR != 0, ctx.clone());
            thread::spawn(move || {
                let result = catch_unwind(|| {
                    let details = fileop::details(&path).map_err(win::error_text);
                    (details, if is_dir { None } else { fileop::size_on_disk(&path) })
                })
                .unwrap_or_else(|panic| (Err(format!("Internal error: {}", win::panic_text(&*panic))), None));
                *s.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                ctx.request_repaint();
            });
            p.pending = Some(slot);
        }
        let dirs: Vec<Arc<str>> = p.items.iter().filter(|i| i.attrs & win::ATTR_DIR != 0).map(|i| Arc::from(i.path.as_str())).collect();
        if !dirs.is_empty() {
            let usage = Arc::new(Usage::default());
            let u = usage.clone();
            thread::spawn(move || {
                let workers = if dirs.iter().any(|d| d.starts_with(r"\\")) { 16 } else { walk::cores() };
                let (_, failed) = walk::walk(&dirs, workers, &u.cancel, || (), |_, _, entry| match entry {
                    Ok((_, e)) if e.attrs & win::ATTR_DIR != 0 => {
                        u.dirs.fetch_add(1, Relaxed);
                    }
                    Ok((_, e)) => {
                        u.files.fetch_add(1, Relaxed);
                        u.bytes.fetch_add(e.size, Relaxed);
                    }
                    Err(_) => {
                        u.errors.fetch_add(1, Relaxed);
                    }
                });
                u.errors.fetch_add(failed as u64, Relaxed);
                u.done.store(true, Relaxed);
                ctx.request_repaint();
            });
            p.usage = Some(usage);
        }
        p
    }

    pub fn busy(&self) -> bool {
        self.usage.as_ref().is_some_and(|u| !u.done.load(Relaxed))
    }

    /// Take the single item's details once its worker has read them.
    fn collect_details(&mut self) {
        let Some(slot) = &self.pending else { return };
        let Some((details, on_disk)) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        if let (Ok(d), [one]) = (&details, self.items.as_mut_slice()) {
            one.attrs = d.attrs; // the open folder's own attributes arrive this way
        }
        self.details = Some(details);
        self.on_disk = on_disk;
        self.pending = None;
    }

    /// Draw the window; returns false once closed.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.collect_details();
        let mut open = true;
        let title = match self.items.as_slice() {
            [one] => format!("{} — Properties", one.name),
            many => format!("{} items — Properties", many.len()),
        };
        egui::Window::new(title)
            .id(egui::Id::unique("properties"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .show(ctx, |ui| self.contents(ui));
        open
    }

    fn contents(&mut self, ui: &mut Ui) {
        let single = self.items.len() == 1;
        egui::Grid::new("props_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            if let [one] = self.items.as_slice() {
                let is_dir = one.attrs & win::ATTR_DIR != 0;
                ui.weak("Name");
                ui.label(&one.name);
                ui.end_row();
                ui.weak("Location");
                ui.label(fsys::parent(&one.path).unwrap_or_default());
                ui.end_row();
                ui.weak("Type");
                let row = Row { name: one.name.as_str().into(), dir: None, size: 0, mtime: 0, attrs: one.attrs, sel: false };
                ui.label(row.type_name());
                ui.end_row();
                if !is_dir {
                    ui.weak("Size");
                    ui.label(size_text(one.size));
                    ui.end_row();
                    if let Some(d) = self.on_disk {
                        ui.weak("Size on disk");
                        ui.label(size_text(d));
                        ui.end_row();
                    }
                }
            } else {
                let dirs = self.items.iter().filter(|i| i.attrs & win::ATTR_DIR != 0).count();
                ui.weak("Selected");
                ui.label(format!("{} files, {dirs} folders", self.items.len() - dirs));
                ui.end_row();
                ui.weak("Location");
                ui.label(fsys::parent(&self.items[0].path).unwrap_or_default());
                ui.end_row();
            }

            let files_bytes: u64 = self.items.iter().filter(|i| i.attrs & win::ATTR_DIR == 0).map(|i| i.size).sum();
            let file_count = self.items.iter().filter(|i| i.attrs & win::ATTR_DIR == 0).count() as u64;
            if let Some(u) = &self.usage {
                let done = u.done.load(Relaxed);
                ui.weak(if single { "Size" } else { "Total size" });
                ui.horizontal(|ui| {
                    ui.label(size_text(files_bytes + u.bytes.load(Relaxed)));
                    if !done {
                        ui.weak("counting…");
                    }
                });
                ui.end_row();
                ui.weak("Contains");
                let errs = u.errors.load(Relaxed);
                ui.label(format!(
                    "{} files, {} folders{}",
                    file_count + u.files.load(Relaxed),
                    u.dirs.load(Relaxed),
                    if errs > 0 { format!(" ({errs} folders unreadable)") } else { String::new() }
                ));
                ui.end_row();
            } else if !single {
                ui.weak("Total size");
                ui.label(size_text(files_bytes));
                ui.end_row();
            }

            match &self.details {
                Some(Ok(d)) => {
                    for (label, t) in [("Created", d.created), ("Modified", d.modified), ("Accessed", d.accessed)] {
                        ui.weak(label);
                        ui.label(win::format_time(t));
                        ui.end_row();
                    }
                }
                Some(Err(e)) => {
                    ui.weak("Details");
                    ui.colored_label(Color32::from_rgb(230, 80, 80), e);
                    ui.end_row();
                }
                None => {}
            }
        });

        ui.separator();
        self.attributes(ui);
        ui.separator();
        self.permissions(ui);
        if let Some(m) = &self.message {
            ui.colored_label(Color32::from_rgb(230, 80, 80), m);
        }
    }

    fn attributes(&mut self, ui: &mut Ui) {
        const READONLY: u32 = 0x1;
        const HIDDEN: u32 = 0x2;
        ui.horizontal(|ui| {
            ui.weak("Attributes");
            for (label, bit) in [("Read-only", READONLY), ("Hidden", HIDDEN)] {
                let mut on = self.items.iter().all(|i| i.attrs & bit != 0);
                if ui.checkbox(&mut on, label).changed() {
                    for item in &mut self.items {
                        let new = if on { item.attrs | bit } else { item.attrs & !bit };
                        // Only these bits are settable; NORMAL stands for "none of them".
                        const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;
                        let settable = match new & SETTABLE {
                            0 => 0x80,
                            v => v,
                        };
                        match fileop::set_attributes(&item.path, settable) {
                            Ok(()) => item.attrs = new,
                            Err(e) => self.message = Some(format!("{}: {}", item.name, win::error_text(e))),
                        }
                    }
                }
            }
            if let Some(Ok(d)) = &self.details {
                if d.attrs & 0x800 != 0 {
                    ui.label(RichText::new("Compressed").weak());
                }
                if d.attrs & 0x4000 != 0 {
                    ui.label(RichText::new("Encrypted").weak());
                }
            }
        });
    }

    fn permissions(&mut self, ui: &mut Ui) {
        let path = self.items[0].path.clone();
        let resp = egui::CollapsingHeader::new("Permissions").default_open(false).show(ui, |ui| {
            let slot = self.acl.get_or_insert_with(|| {
                let slot = Arc::new(Mutex::new(None));
                let (s, p, ctx) = (slot.clone(), path.clone(), ui.ctx().clone());
                thread::spawn(move || {
                    let info = catch_unwind(|| security::read(&p)).unwrap_or_else(|panic| Err(format!("Internal error: {}", win::panic_text(&*panic))));
                    *s.lock().unwrap_or_else(PoisonError::into_inner) = Some(info);
                    ctx.request_repaint();
                });
                slot
            });
            match &*slot.lock().unwrap_or_else(PoisonError::into_inner) {
                None => {
                    ui.weak("reading…");
                }
                Some(Err(e)) => {
                    ui.colored_label(Color32::from_rgb(230, 80, 80), e);
                }
                Some(Ok(info)) => {
                    if self.items.len() > 1 {
                        ui.weak(format!("Showing {}", self.items[0].name));
                    }
                    ui.horizontal(|ui| {
                        ui.weak("Owner");
                        ui.label(&info.owner);
                    });
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        egui::Grid::new("acl_grid").striped(true).num_columns(4).show(ui, |ui| {
                            for h in ["", "Account", "Access", ""] {
                                ui.strong(h);
                            }
                            ui.end_row();
                            for ace in &info.aces {
                                if ace.allow {
                                    ui.label("Allow");
                                } else {
                                    ui.colored_label(Color32::from_rgb(230, 80, 80), "Deny");
                                }
                                ui.label(&ace.account);
                                ui.label(&ace.rights);
                                let mut note = Vec::new();
                                if ace.inherited {
                                    note.push("inherited");
                                }
                                if ace.inherit_only {
                                    note.push("subfolders/files only");
                                }
                                ui.weak(note.join(", "));
                                ui.end_row();
                            }
                        });
                    });
                }
            }
        });
        if resp.fully_open() && self.items.len() == 1 && ui.button("Edit permissions…").clicked() && !fileop::show_security_dialog(&path) {
            self.message = Some("Could not open the Security dialog".into());
        }
    }
}

fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return fsys::format_size(bytes);
    }
    format!("{} ({} bytes)", fsys::format_size(bytes), group(bytes))
}

/// 1234567 → "1,234,567"
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
