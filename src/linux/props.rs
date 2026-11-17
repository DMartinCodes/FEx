//! Properties window (the Linux side of `props.rs`), built to the same
//! rules: nothing on the UI thread touches the disk.
//! * names, types and sizes of the selection come from the listing;
//! * a single item's full metadata (statx, link target, owner names, the
//!   user's own access, ACLs) is read once on a worker, so a stalled network
//!   mount or a slow LDAP lookup can't hang the window;
//! * folder sizes come from a parallel walk, cancelled when the window closes;
//! * permission changes are applied on a worker too.
//!
//! A symbolic link has no permissions of its own (they are always 0777), so
//! for a link the window shows its target and the target's permissions.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use eframe::egui::{self, Color32, RichText, Ui};

use super::fsys::{self, Row};
use super::sys::{self, AclEntry, AclTag, Stat, DIR, LINK};
use super::walk::{self, Visit};

const RED: Color32 = Color32::from_rgb(230, 80, 80);

pub struct Item {
    pub path: Vec<u8>,
    pub name: String,
    pub flags: u8,
    pub size: u64,
}

impl Item {
    fn is_dir(&self) -> bool {
        self.flags & DIR != 0
    }

    /// A real folder to count (not a link to one: links aren't followed).
    fn is_real_dir(&self) -> bool {
        self.flags & (DIR | LINK) == DIR
    }
}

/// What a single item's worker reads.
struct Details {
    /// The item itself (a link is not followed).
    own: Stat,
    /// For a link: what it points to as stored, and the target's metadata
    /// (an error for a broken link).
    link: Option<(Vec<u8>, Result<Stat, i32>)>,
    /// The file whose permissions are shown: the item, or a link's target.
    perms: Option<Perms>,
    /// Filesystem type, source and mount point of the item's volume.
    volume: Option<(String, String, String)>,
}

#[derive(Clone)]
struct Perms {
    path: Vec<u8>,
    mode: u32,
    uid: u32,
    gid: u32,
    owner: String,
    group: String,
    /// The current user's effective access: read, write, execute.
    access: (bool, bool, bool),
    /// Only the owner (or root) may change permissions.
    can_chmod: bool,
    acl: Vec<(String, u8)>,
    default_acl: Vec<(String, u8)>,
}

#[derive(Default)]
struct Usage {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    on_disk: AtomicU64,
    errors: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

fn take<T>(slot: &Slot<T>) -> Option<T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

fn put<T>(slot: &Slot<T>, value: T, ctx: &egui::Context) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
    ctx.request_repaint();
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown".into())
}

pub struct Props {
    items: Vec<Item>,
    details: Option<Result<Details, String>>,
    pending: Option<Slot<Result<Details, String>>>,
    usage: Option<Arc<Usage>>,
    /// A permission change in flight: the mode asked for, and its result.
    chmod: Option<(u32, Slot<Result<(), String>>)>,
    message: Option<String>,
    ctx: egui::Context,
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
        let mut p = Props { items, details: None, pending: None, usage: None, chmod: None, message: None, ctx: ctx.clone() };
        if let [one] = p.items.as_slice() {
            let slot: Slot<Result<Details, String>> = Arc::default();
            let (s, path, ctx) = (slot.clone(), one.path.clone(), ctx.clone());
            thread::spawn(move || {
                let result = catch_unwind(|| read_details(&path)).unwrap_or_else(|e| Err(format!("Internal error: {}", panic_text(&*e))));
                put(&s, result, &ctx);
            });
            p.pending = Some(slot);
        }
        let dirs: Vec<Arc<[u8]>> = p.items.iter().filter(|i| i.is_real_dir()).map(|i| Arc::from(i.path.as_slice())).collect();
        if !dirs.is_empty() {
            let usage = Arc::new(Usage::default());
            let u = usage.clone();
            thread::spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| count(&dirs, &u)));
                if r.is_err() {
                    u.errors.fetch_add(1, Relaxed);
                }
                u.done.store(true, Relaxed);
                ctx.request_repaint();
            });
            p.usage = Some(usage);
        }
        p
    }

    /// Still counting (the caller repaints periodically to show progress).
    pub fn busy(&self) -> bool {
        self.usage.as_ref().is_some_and(|u| !u.done.load(Relaxed)) || self.chmod.is_some()
    }

    fn collect(&mut self) {
        if let Some(d) = self.pending.as_ref().and_then(take) {
            self.details = Some(d);
            self.pending = None;
        }
        if let Some((mode, slot)) = &self.chmod
            && let Some(result) = take(slot)
        {
            match result {
                Ok(()) => {
                    if let Some(Ok(Details { perms: Some(p), .. })) = &mut self.details {
                        p.mode = (p.mode & !0o7777) | (mode & 0o7777);
                    }
                    self.message = None;
                }
                Err(e) => self.message = Some(e),
            }
            self.chmod = None;
        }
    }

    /// Draw the window; returns false once closed.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.collect();
        let mut open = true;
        let title = match self.items.as_slice() {
            [one] => format!("{} — Properties", one.name),
            many => format!("{} items — Properties", many.len()),
        };
        egui::Window::new(title)
            .id(egui::Id::unique("properties"))
            .open(&mut open)
            .collapsible(false)
            // Height follows the contents (scrolling only past the screen).
            .resizable([true, false])
            .default_width(440.0)
            .show(ctx, |ui| {
                let max = ui.ctx().content_rect().height() - 120.0;
                egui::ScrollArea::vertical().auto_shrink([false, true]).min_scrolled_height(max).max_height(max).show(ui, |ui| self.contents(ui));
            });
        open
    }

    fn contents(&mut self, ui: &mut Ui) {
        self.general(ui);
        ui.separator();
        self.permissions(ui);
        if let Some(m) = &self.message {
            ui.colored_label(RED, m);
        }
    }

    fn general(&self, ui: &mut Ui) {
        let single = self.items.len() == 1;
        let row = |ui: &mut Ui, label: &str, value: String| {
            ui.weak(label);
            ui.add(egui::Label::new(value).selectable(true).wrap());
            ui.end_row();
        };
        egui::Grid::new("props_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            let details = match &self.details {
                Some(Ok(d)) => Some(d),
                _ => None,
            };
            if let [one] = self.items.as_slice() {
                row(ui, "Name", one.name.clone());
                row(ui, "Location", lossy(&fsys::parent(&one.path).unwrap_or_default()));
                let r = Row { name: one.name.as_bytes().into(), dir: None, size: 0, mtime: 0, flags: one.flags, sel: false, id: 0 };
                row(ui, "Type", r.type_name());
                if let Some((target, resolved)) = details.and_then(|d| d.link.as_ref()) {
                    row(ui, "Link target", lossy(target));
                    let state = match resolved {
                        Ok(t) if t.flags() & DIR != 0 => "Folder".to_string(),
                        Ok(t) => format!("File, {}", size_text(t.size)),
                        Err(e) if *e == libc::ENOENT => "Missing (broken link)".to_string(),
                        Err(e) => sys::error_text(*e),
                    };
                    row(ui, "Target", state);
                }
                if !one.is_dir() {
                    let size = details.map_or(one.size, |d| d.own.size);
                    row(ui, "Size", size_text(size));
                    if let Some(d) = details {
                        row(ui, "Size on disk", size_text(d.own.on_disk));
                    }
                }
            } else {
                let dirs = self.items.iter().filter(|i| i.is_dir()).count();
                row(ui, "Selected", format!("{} files, {dirs} folders", self.items.len() - dirs));
                row(ui, "Location", lossy(&fsys::parent(&self.items[0].path).unwrap_or_default()));
            }

            let files: Vec<&Item> = self.items.iter().filter(|i| !i.is_dir()).collect();
            let files_bytes: u64 = files.iter().map(|i| i.size).sum();
            if let Some(u) = &self.usage {
                let done = u.done.load(Relaxed);
                let counting = if done { "" } else { "  counting…" };
                row(ui, if single { "Size" } else { "Total size" }, format!("{}{counting}", size_text(files_bytes + u.bytes.load(Relaxed))));
                row(ui, "Size on disk", format!("{}{counting}", size_text(u.on_disk.load(Relaxed))));
                let errs = u.errors.load(Relaxed);
                let unreadable = if errs > 0 { format!(" ({errs} folders unreadable)") } else { String::new() };
                row(ui, "Contains", format!("{} files, {} folders{unreadable}", files.len() as u64 + u.files.load(Relaxed), u.dirs.load(Relaxed)));
            } else if !single {
                row(ui, "Total size", size_text(files_bytes));
            }

            match &self.details {
                Some(Ok(d)) => {
                    let s = &d.own;
                    if s.btime != 0 {
                        row(ui, "Created", sys::format_time_full(s.btime));
                    }
                    row(ui, "Modified", sys::format_time_full(s.mtime));
                    row(ui, "Accessed", sys::format_time_full(s.atime));
                    row(ui, "Changed", sys::format_time_full(s.ctime));
                    if let Some((fstype, source, point)) = &d.volume {
                        row(ui, "Volume", format!("{point}  ({fstype} on {source})"));
                    }
                    row(ui, "Inode", format!("{} on device {}:{}, {} hard link{}", s.ino, s.dev_major, s.dev_minor, s.nlink, if s.nlink == 1 { "" } else { "s" }));
                    let attrs = attribute_names(s.attributes);
                    if !attrs.is_empty() {
                        row(ui, "Attributes", attrs.join(", "));
                    }
                }
                Some(Err(e)) => {
                    ui.weak("Details");
                    ui.colored_label(RED, e);
                    ui.end_row();
                }
                None if self.pending.is_some() => {
                    ui.weak("Details");
                    ui.weak("reading…");
                    ui.end_row();
                }
                None => {}
            }
        });
    }

    fn permissions(&mut self, ui: &mut Ui) {
        let is_link = self.items.first().is_some_and(|i| i.flags & LINK != 0);
        let heading = if is_link { "Permissions (of the link's target)" } else { "Permissions" };
        egui::CollapsingHeader::new(RichText::new(heading).strong()).default_open(true).show(ui, |ui| {
            let perms = match &self.details {
                None if self.items.len() > 1 => {
                    ui.weak("Select a single item to see its permissions.");
                    return;
                }
                None => {
                    ui.weak("reading…");
                    return;
                }
                Some(Err(_)) => return,
                Some(Ok(Details { perms: None, .. })) => {
                    ui.weak("The link's target can't be reached, so there are no permissions to show.");
                    return;
                }
                Some(Ok(Details { perms: Some(p), .. })) => p.clone(),
            };
            egui::Grid::new("owner_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.weak("Owner");
                ui.label(format!("{} ({})", perms.owner, perms.uid));
                ui.end_row();
                ui.weak("Group");
                ui.label(format!("{} ({})", perms.group, perms.gid));
                ui.end_row();
                ui.weak("Mode");
                ui.add(egui::Label::new(RichText::new(format!("{}   {:04o}", sys::mode_text(perms.mode), perms.mode & 0o7777)).monospace()).selectable(true));
                ui.end_row();
                ui.weak("Your access");
                let (r, w, x) = perms.access;
                let mut can: Vec<&str> = Vec::new();
                if r {
                    can.push(if perms.mode & libc::S_IFMT == libc::S_IFDIR { "list" } else { "read" });
                }
                if w {
                    can.push("write");
                }
                if x {
                    can.push(if perms.mode & libc::S_IFMT == libc::S_IFDIR { "enter" } else { "execute" });
                }
                ui.label(if can.is_empty() { "none".to_string() } else { can.join(", ") });
                ui.end_row();
            });
            ui.add_space(6.0);

            let editable = perms.can_chmod && self.items.len() == 1 && self.chmod.is_none();
            let mut mode = perms.mode;
            let is_dir = perms.mode & libc::S_IFMT == libc::S_IFDIR;
            ui.add_enabled_ui(editable, |ui| {
                egui::Grid::new("mode_grid").num_columns(4).spacing([18.0, 4.0]).striped(true).show(ui, |ui| {
                    ui.label("");
                    for h in ["Read", "Write", if is_dir { "Enter" } else { "Execute" }] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for (who, shift) in [("Owner", 6), ("Group", 3), ("Others", 0)] {
                        ui.label(who);
                        for bit in [4u32, 2, 1] {
                            let mask = bit << shift;
                            let mut on = mode & mask != 0;
                            if ui.checkbox(&mut on, "").changed() {
                                mode = if on { mode | mask } else { mode & !mask };
                            }
                        }
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    let tips = [
                        ("Set user ID", libc::S_ISUID, "Runs as the file's owner"),
                        ("Set group ID", libc::S_ISGID, if is_dir { "New files get this folder's group" } else { "Runs with the file's group" }),
                        ("Sticky", libc::S_ISVTX, "Only owners may delete or rename entries"),
                    ];
                    for (label, mask, tip) in tips {
                        if mask == libc::S_ISVTX && !is_dir {
                            continue;
                        }
                        let mut on = mode & mask != 0;
                        if ui.checkbox(&mut on, label).on_hover_text(tip).changed() {
                            mode = if on { mode | mask } else { mode & !mask };
                        }
                    }
                });
            });
            if self.chmod.is_some() {
                ui.weak("applying…");
            } else if !perms.can_chmod {
                ui.weak("Only the owner can change these permissions.");
            }
            if mode != perms.mode {
                self.apply_mode(perms.path.clone(), mode);
            }

            acl_list(ui, "Access control list", &perms.acl, "Extra entries beyond the mode bits. The mask caps every entry except the owner and others.");
            if is_dir {
                acl_list(ui, "Defaults for new entries", &perms.default_acl, "What files and folders created here inherit.");
            }
        });
    }

    fn apply_mode(&mut self, path: Vec<u8>, mode: u32) {
        let slot: Slot<Result<(), String>> = Arc::default();
        let (s, ctx) = (slot.clone(), self.ctx.clone());
        thread::spawn(move || {
            let result = sys::chmod(&path, mode).map_err(|e| format!("Could not change permissions: {}", sys::error_text(e)));
            put(&s, result, &ctx);
        });
        self.chmod = Some((mode, slot));
    }
}

fn acl_list(ui: &mut Ui, title: &str, entries: &[(String, u8)], tip: &str) {
    if entries.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong()).on_hover_text(tip);
    egui::Grid::new(("acl", title)).num_columns(2).spacing([16.0, 2.0]).striped(true).show(ui, |ui| {
        for (who, perm) in entries {
            ui.label(who);
            ui.monospace(rwx(*perm));
            ui.end_row();
        }
    });
}

fn rwx(p: u8) -> String {
    [(4, 'r'), (2, 'w'), (1, 'x')].iter().map(|&(b, c)| if p & b != 0 { c } else { '-' }).collect()
}

fn read_details(path: &[u8]) -> Result<Details, String> {
    let own = sys::stat_full(path, false).map_err(sys::error_text)?;
    let is_link = own.mode & libc::S_IFMT == libc::S_IFLNK;
    let link = is_link.then(|| (sys::read_link(path).unwrap_or_default(), sys::stat_full(path, true)));
    let shown = match &link {
        None => Some(own),
        Some((_, Ok(target))) => Some(*target),
        Some((_, Err(_))) => None,
    };
    let perms = shown.map(|s| {
        let me = sys::uid();
        let name = |e: &AclEntry| match (e.tag, e.id) {
            (AclTag::User, Some(id)) => format!("user {}", sys::user_name(id).unwrap_or_else(|| id.to_string())),
            (AclTag::Group, Some(id)) => format!("group {}", sys::group_name(id).unwrap_or_else(|| id.to_string())),
            (AclTag::UserObj, _) => "owner".into(),
            (AclTag::GroupObj, _) => "owning group".into(),
            (AclTag::Mask, _) => "mask".into(),
            _ => "others".into(),
        };
        let named = |default: bool| -> Vec<(String, u8)> {
            let acl = sys::acl(path, default);
            // An access ACL of only owner/group/others is just the mode bits.
            if !default && acl.iter().all(|e| matches!(e.tag, AclTag::UserObj | AclTag::GroupObj | AclTag::Other)) {
                return Vec::new();
            }
            acl.iter().map(|e| (name(e), e.perm)).collect()
        };
        Perms {
            path: path.to_vec(),
            mode: s.mode,
            uid: s.uid,
            gid: s.gid,
            owner: sys::user_name(s.uid).unwrap_or_else(|| "unknown".into()),
            group: sys::group_name(s.gid).unwrap_or_else(|| "unknown".into()),
            access: sys::access(path),
            can_chmod: me == 0 || me == s.uid,
            acl: named(false),
            default_acl: if s.mode & libc::S_IFMT == libc::S_IFDIR { named(true) } else { Vec::new() },
        }
    });
    Ok(Details { own, link, perms, volume: volume_of(path) })
}

/// The mount holding `path`: the longest mount point that is a prefix of
/// its resolved location.
fn volume_of(path: &[u8]) -> Option<(String, String, String)> {
    let real = sys::real_path(path).or_else(|| fsys::parent(path).and_then(|p| sys::real_path(&p)))?;
    sys::mounts()
        .into_iter()
        .filter(|m| fsys::in_scope(&real, &m.point))
        .max_by_key(|m| m.point.len())
        .map(|m| (m.fstype, lossy(&m.source), lossy(&m.point)))
}

/// Count files, folders and bytes under `dirs`. Hard-linked files count
/// once (as `du` does); links aren't followed.
fn count(dirs: &[Arc<[u8]>], u: &Usage) {
    let seen: Mutex<HashSet<(u32, u32, u64)>> = Mutex::default();
    let workers = walk::cores().min(8);
    let (_, failed) = walk::walk(dirs, workers, &u.cancel, &HashSet::new(), &|| {}, || (), |_, _, visit| match visit {
        Visit::Begin(_) => {}
        Visit::Entry(_, e) if e.flags & (DIR | LINK) == DIR => {
            u.dirs.fetch_add(1, Relaxed);
        }
        Visit::Entry(dir, e) => {
            u.files.fetch_add(1, Relaxed);
            if let Some(s) = dir.stat_full(e.name) {
                if s.nlink > 1 && s.flags() & DIR == 0 && !seen.lock().unwrap_or_else(PoisonError::into_inner).insert((s.dev_major, s.dev_minor, s.ino)) {
                    return;
                }
                u.bytes.fetch_add(s.size, Relaxed);
                u.on_disk.fetch_add(s.on_disk, Relaxed);
            }
        }
        Visit::Unreadable => {
            u.errors.fetch_add(1, Relaxed);
        }
    });
    u.errors.fetch_add(failed as u64, Relaxed);
}

fn attribute_names(a: u64) -> Vec<&'static str> {
    [
        (libc::STATX_ATTR_IMMUTABLE as u64, "Immutable"),
        (libc::STATX_ATTR_APPEND as u64, "Append only"),
        (libc::STATX_ATTR_COMPRESSED as u64, "Compressed"),
        (libc::STATX_ATTR_ENCRYPTED as u64, "Encrypted"),
        (libc::STATX_ATTR_NODUMP as u64, "No dump"),
        (libc::STATX_ATTR_VERITY as u64, "Verity protected"),
        (libc::STATX_ATTR_DAX as u64, "DAX"),
    ]
    .into_iter()
    .filter(|(bit, _)| a & bit != 0)
    .map(|(_, name)| name)
    .collect()
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return fsys::format_size(bytes);
    }
    format!("{} ({} bytes)", fsys::format_size(bytes), group(bytes))
}

/// 1234567 -> "1,234,567"
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn formats() {
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(12), "12");
        assert_eq!(rwx(5), "r-x");
    }

    #[test]
    fn reads_links_and_counts_folders() {
        let root = std::env::temp_dir().join(format!("fex-props-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("d/a"), b"12345").unwrap();
        std::fs::write(root.join("d/sub/b"), b"123").unwrap();
        std::fs::hard_link(root.join("d/a"), root.join("d/sub/a2")).unwrap();
        std::os::unix::fs::symlink("d", root.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("broken")).unwrap();
        let b = |p: &std::path::Path| p.as_os_str().as_bytes().to_vec();

        let link = read_details(&b(&root.join("link"))).unwrap();
        let (target, resolved) = link.link.unwrap();
        assert_eq!(target, b"d");
        assert!(resolved.unwrap().flags() & DIR != 0);
        assert_eq!(link.perms.unwrap().mode & libc::S_IFMT, libc::S_IFDIR);

        let broken = read_details(&b(&root.join("broken"))).unwrap();
        assert!(broken.perms.is_none());
        assert_eq!(broken.link.unwrap().1.unwrap_err(), libc::ENOENT);

        let u = Usage::default();
        count(&[Arc::from(b(&root.join("d")).as_slice())], &u);
        assert_eq!(u.files.load(Relaxed), 3); // a, b, a2 (entries)
        assert_eq!(u.dirs.load(Relaxed), 1);
        assert_eq!(u.bytes.load(Relaxed), 8); // a2 is a hard link to a: counted once

        let _ = std::fs::remove_dir_all(root);
    }
}
