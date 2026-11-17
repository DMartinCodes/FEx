//! The New menu's file types (the Linux side of `shellnew.rs`): the files
//! in the XDG Templates folder (`~/Templates`, as GNOME Files, Nemo, Caja
//! and Thunar use it), plus an empty document. Creating one copies the
//! template's bytes into a new file; nothing is executed.

use std::io::Write;

use super::fsys;
use super::ops;
use super::sys::{self, DIR};

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Shown in the menu: the template's name without its extension.
    pub label: String,
    /// Extension with its dot, or empty.
    pub extension: String,
    /// The template file; `None` for the empty document.
    pub source: Option<Vec<u8>>,
    /// The name a new file gets (made unique in the folder).
    pub name: Vec<u8>,
}

/// Templates, sorted by label, after the empty document.
pub fn load() -> Vec<Item> {
    let mut out = Vec::new();
    if let Some(dir) = sys::xdg_user_dir("TEMPLATES", "Templates")
        && let Ok(d) = sys::Dir::open(&dir)
    {
        let _ = d.scan(&mut Vec::new(), |e| {
            if e.flags & DIR != 0 || e.name.starts_with(b".") || e.name.ends_with(b"~") {
                return;
            }
            let ext = fsys::ext(e.name);
            let stem = if ext.is_empty() { e.name } else { &e.name[..e.name.len() - ext.len() - 1] };
            out.push(Item {
                label: String::from_utf8_lossy(stem).into_owned(),
                extension: if ext.is_empty() { String::new() } else { format!(".{}", String::from_utf8_lossy(ext)) },
                source: Some(fsys::join(&dir, e.name)),
                name: e.name.to_vec(),
            });
        });
    }
    out.sort_by(|a, b| fsys::cmp_names(a.label.as_bytes(), b.label.as_bytes()));
    out.insert(0, Item { label: "Empty document".into(), extension: String::new(), source: None, name: b"New document".to_vec() });
    out
}

/// Create a new file from `item` in `dir`; returns its name.
pub fn create(dir: &[u8], item: &Item) -> Result<Vec<u8>, String> {
    let data = match &item.source {
        Some(src) => std::fs::read(sys::os(src)).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(src)))?,
        None => Vec::new(),
    };
    // A name can be taken between choosing and creating it: choose again.
    for _ in 0..16 {
        let name = ops::unique_name(dir, &item.name, false);
        let path = fsys::join(dir, &name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(sys::os(&path)) {
            Ok(mut f) => {
                f.write_all(&data).map_err(|e| format!("{}: {e}", String::from_utf8_lossy(&path)))?;
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not create the file: {e}")),
        }
    }
    Err("Could not choose a name for the new file".into())
}
