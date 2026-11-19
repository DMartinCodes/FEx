//! Files recently opened from FEx (the Linux side of `recent.rs`), newest
//! first, in `$XDG_DATA_HOME/fex/recent` (NUL-separated paths, since any
//! other byte can appear in a Linux file name).

use std::path::PathBuf;
use std::sync::Arc;

use super::fileop;
use super::fsys::{self, Row};
use super::session;

const MAX_RECENT: usize = 100;

fn file() -> PathBuf {
    session::data_dir().join("recent")
}

fn load() -> Vec<Vec<u8>> {
    std::fs::read(file()).unwrap_or_default().split(|&b| b == 0).filter(|p| !p.is_empty()).map(<[u8]>::to_vec).collect()
}

fn save(paths: &[Vec<u8>]) {
    let f = file();
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = f.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, paths.join(&0u8)).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
}

fn insert(paths: &mut Vec<Vec<u8>>, path: &[u8]) {
    paths.retain(|p| p != path);
    paths.insert(0, path.to_vec());
    paths.truncate(MAX_RECENT);
}

pub fn record(path: &[u8]) {
    let mut paths = load();
    insert(&mut paths, path);
    save(&paths);
}

/// Follow a rename (or move) made in FEx, folders included.
pub fn renamed(old: &[u8], new: &[u8]) {
    let mut paths = load();
    let mut changed = false;
    for p in paths.iter_mut() {
        if fsys::in_scope(p, old) {
            *p = [new, &p[old.len()..]].concat();
            changed = true;
        }
    }
    if changed {
        let mut seen = std::collections::HashSet::new();
        paths.retain(|p| seen.insert(p.clone()));
        save(&paths);
    }
}

/// Rows for the Recent page: files that still exist, newest first.
pub fn rows() -> Vec<Row> {
    load()
        .into_iter()
        .filter_map(|path| {
            let st = fileop::stat(&path).filter(|s| !s.is_dir())?;
            let dir = fsys::parent(&path)?;
            Some(Row { name: fsys::base_name(&path).into(), dir: Some(Arc::from(dir)), size: st.size, mtime: st.mtime, flags: st.flags, sel: false, id: 0 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_duplicates() {
        let mut v = vec![b"/a".to_vec(), b"/b".to_vec()];
        insert(&mut v, b"/b");
        assert_eq!(v, vec![b"/b".to_vec(), b"/a".to_vec()]);
    }
}
