//! Registered ShellNew templates, without executing registry commands.
//!
//! Like Explorer, a template is found under `HKCR\.ext\ShellNew` or, for most
//! Office types, under the extension's ProgID: `HKCR\.ext\<ProgID>\ShellNew`.
//! `Command` templates (which run a program) are deliberately not offered.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_BINARY, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumKeyExW,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_CommonTemplates, FOLDERID_Templates, SHLoadIndirectString};

use crate::{fsys, ops, win};

#[derive(Clone, Debug)]
pub enum Template {
    Empty,
    Data(Vec<u8>),
    /// A template file, already located on disk.
    File(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub extension: String,
    pub label: String,
    template: Template,
}

struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: &str) -> Option<Self> {
        let path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut key = null_mut();
        (unsafe { RegOpenKeyExW(parent, path.as_ptr(), 0, KEY_READ, &mut key) } == 0).then_some(Key(key))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn value(key: HKEY, name: &str) -> Option<(u32, Vec<u8>)> {
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let (mut kind, mut size) = (0u32, 0u32);
    let rc = unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut kind, null_mut(), &mut size) };
    if rc != 0 && rc != ERROR_MORE_DATA {
        return None;
    }
    let mut bytes = vec![0; size as usize];
    let rc = unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut size) };
    if rc != 0 {
        return None;
    }
    bytes.truncate(size as usize);
    Some((kind, bytes))
}

/// UTF-16 registry string data up to its terminator.
fn utf16_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    let mut text = String::new();
    win::push_utf16(&mut text, &units[..len]);
    text
}

/// A string value, with `%VARS%` expanded for REG_EXPAND_SZ.
fn text_value(key: HKEY, name: &str) -> Option<String> {
    match value(key, name)? {
        (REG_SZ, bytes) => Some(utf16_text(&bytes)),
        (REG_EXPAND_SZ, bytes) => Some(fsys::expand_env(&utf16_text(&bytes))),
        _ => None,
    }
}

/// Resolve `@file.dll,-123` resource references; other text passes through.
fn display_text(text: String) -> Option<String> {
    if !text.starts_with('@') {
        return Some(text).filter(|t| !t.is_empty());
    }
    let source = win::wide(&text, "");
    let mut out = [0u16; 512];
    if unsafe { SHLoadIndirectString(source.as_ptr(), out.as_mut_ptr(), out.len() as u32, null_mut()) } < 0 {
        return None;
    }
    let len = out.iter().position(|&c| c == 0).unwrap_or(0);
    let mut s = String::new();
    win::push_utf16(&mut s, &out[..len]);
    Some(s).filter(|s| !s.is_empty())
}

/// Explorer's label: the template's ItemName, else the type's friendly name.
fn label(extension: &str, prog_id: Option<&str>, shell_key: HKEY) -> String {
    let from_template = text_value(shell_key, "ItemName").and_then(display_text);
    let from_type = || {
        let key = Key::open(HKEY_CLASSES_ROOT, prog_id?)?;
        text_value(key.0, "FriendlyTypeName").and_then(display_text).or_else(|| text_value(key.0, "").and_then(display_text))
    };
    from_template.or_else(from_type).unwrap_or_else(|| format!("{} File", extension.trim_start_matches('.').to_uppercase()))
}

/// Locate a `FileName` template: absolute, or in the user's, the shared,
/// or the Windows ShellNew folder (Explorer's search order).
fn find_template(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(name);
    if path.is_absolute() {
        return path.is_file().then_some(path);
    }
    let windir = std::env::var("WINDIR").ok().map(|w| format!(r"{w}\ShellNew"));
    [win::known_folder(&FOLDERID_Templates), win::known_folder(&FOLDERID_CommonTemplates), windir]
        .into_iter()
        .flatten()
        .map(|dir| Path::new(&dir).join(&path))
        .find(|p| p.is_file())
}

fn template(shell_key: HKEY) -> Option<Template> {
    if value(shell_key, "NullFile").is_some() {
        return Some(Template::Empty);
    }
    match value(shell_key, "Data") {
        Some((REG_BINARY, data)) => return Some(Template::Data(data)),
        // String data is the file's text, not including the registry terminator.
        Some((REG_SZ | REG_EXPAND_SZ, data)) => return Some(Template::Data(utf16_text(&data).into_bytes())),
        _ => {}
    }
    text_value(shell_key, "FileName").and_then(|name| find_template(&name)).map(Template::File)
}

/// The ShellNew key for an extension: directly under it, or under its ProgID.
fn shell_new_key(ext_key: &Key, prog_id: Option<&str>) -> Option<Key> {
    Key::open(ext_key.0, "ShellNew").or_else(|| Key::open(ext_key.0, &format!(r"{}\ShellNew", prog_id?)))
}

pub fn load() -> Vec<Item> {
    let mut items = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name = vec![0u16; 260];
        let mut len = name.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                HKEY_CLASSES_ROOT,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        index = index.saturating_add(1);
        if rc != 0 || len < 2 || name[0] != b'.' as u16 {
            continue;
        }
        name.truncate(len as usize);
        let mut extension = String::new();
        win::push_utf16(&mut extension, &name);
        let Some(ext_key) = Key::open(HKEY_CLASSES_ROOT, &extension) else { continue };
        let prog_id = text_value(ext_key.0, "").filter(|p| !p.is_empty());
        let Some(shell_key) = shell_new_key(&ext_key, prog_id.as_deref()) else { continue };
        let Some(template) = template(shell_key.0) else { continue };
        items.push(Item { label: label(&extension, prog_id.as_deref(), shell_key.0), extension, template });
    }
    items.sort_by_key(|item| item.label.to_lowercase());
    items
}

/// `New <label>.ext`, with characters a filename can't hold removed.
fn file_stem(label: &str) -> String {
    let clean: String = label.chars().filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')).collect();
    format!("New {}", clean.trim())
}

/// Create a new file from the template in `directory`; returns its name.
pub fn create(directory: &str, item: &Item) -> Result<String, String> {
    let wanted = format!("{}{}", file_stem(&item.label), item.extension);
    for _ in 0..128 {
        let name = ops::unique_name(directory, &wanted, false);
        let path = Path::new(directory).join(&name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut output) => {
                let result = match &item.template {
                    Template::Empty => Ok(()),
                    Template::Data(data) => output.write_all(data),
                    Template::File(source) => {
                        drop(output);
                        fs::copy(source, &path).map(|_| ())
                    }
                };
                if let Err(error) = result {
                    let _ = fs::remove_file(&path);
                    return Err(format!("Could not create {}: {error}", path.display()));
                }
                return Ok(name);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not create {}: {error}", path.display())),
        }
    }
    Err("Could not choose a unique filename".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_data_template_with_unique_name() {
        let dir = std::env::temp_dir().join(format!("fex-shellnew-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let item = Item { extension: ".txt".into(), label: "Text Document".into(), template: Template::Data(b"template".to_vec()) };

        let first = create(&dir.to_string_lossy(), &item).unwrap();
        let second = create(&dir.to_string_lossy(), &item).unwrap();
        assert_eq!(first, "New Text Document.txt");
        assert_ne!(first, second);
        assert_eq!(fs::read(dir.join(first)).unwrap(), b"template");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn string_data_drops_the_registry_terminator() {
        let utf16: Vec<u8> = "hi\0".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(utf16_text(&utf16).into_bytes(), b"hi");
        assert_eq!(file_stem("Rich Text: Format?"), "New Rich Text Format");
    }

    #[test]
    fn finds_the_standard_text_template() {
        // Every Windows install registers `.txt` with a NullFile template.
        let items = load();
        assert!(items.iter().any(|i| i.extension.eq_ignore_ascii_case(".txt")), "{items:?}");
    }
}
