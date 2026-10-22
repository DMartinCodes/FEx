//! Recently opened files, persisted under `%APPDATA%\FEx`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::fileop;
use crate::fsys::{self, Row};
use crate::win;

const MAX_RECENT: usize = 100;

fn history_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|root| PathBuf::from(root).join("FEx").join("recent.txt"))
}

fn insert_path(paths: &mut Vec<String>, path: &str) {
    paths.retain(|old| !old.eq_ignore_ascii_case(path));
    paths.insert(0, path.to_string());
    paths.truncate(MAX_RECENT);
}

fn replace_path(paths: &mut Vec<String>, old: &str, new: &str) -> bool {
    let mut changed = false;
    for path in paths.iter_mut() {
        if path.eq_ignore_ascii_case(old) {
            *path = new.to_string();
            changed = true;
        }
    }
    if changed {
        let mut seen = HashSet::new();
        paths.retain(|path| seen.insert(path.to_lowercase()));
    }
    changed
}

fn load_paths(file: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

fn save_paths(file: PathBuf, paths: &[String]) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, paths.join("\0"));
}

pub fn record(path: &str) {
    let Some(file) = history_file() else { return };
    let mut paths = load_paths(&file);
    insert_path(&mut paths, path);
    save_paths(file, &paths);
}

pub fn renamed(old: &str, new: &str) {
    let Some(file) = history_file() else { return };
    let mut paths = load_paths(&file);
    if replace_path(&mut paths, old, new) {
        save_paths(file, &paths);
    }
}

pub fn rows() -> Vec<Row> {
    let Some(file) = history_file() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    contents
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| {
            let stat = fileop::stat(path)?;
            if stat.attrs & win::ATTR_DIR != 0 {
                return None;
            }
            let dir = fsys::parent(path)?;
            let name = path.rsplit('\\').next()?;
            Some(Row {
                name: name.into(),
                dir: Some(Arc::from(dir)),
                size: stat.size,
                mtime: stat.mtime,
                attrs: stat.attrs,
                sel: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_without_case_duplicates() {
        let mut paths = vec![
            r"C:\Docs\report.txt".to_string(),
            r"C:\Docs\other.txt".to_string(),
        ];
        insert_path(&mut paths, r"c:\docs\REPORT.txt");
        assert_eq!(paths, [r"c:\docs\REPORT.txt", r"C:\Docs\other.txt"]);
    }

    #[test]
    fn history_is_bounded() {
        let mut paths: Vec<String> = (0..MAX_RECENT).map(|i| format!(r"C:\{i}.txt")).collect();
        insert_path(&mut paths, r"C:\new.txt");
        assert_eq!(paths.len(), MAX_RECENT);
        assert_eq!(paths[0], r"C:\new.txt");
        assert!(!paths.iter().any(|path| path == r"C:\99.txt"));
    }

    #[test]
    fn rename_updates_history_without_duplicates() {
        let mut paths = vec![r"C:\old.txt".to_string(), r"C:\new.txt".to_string()];
        assert!(replace_path(&mut paths, r"c:\old.txt", r"C:\new.txt"));
        assert_eq!(paths, [r"C:\new.txt"]);
    }
}
