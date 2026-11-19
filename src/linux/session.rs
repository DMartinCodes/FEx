//! What FEx restores at the next start (the Linux side of `session.rs`):
//! open tabs, view settings and window placement, in
//! `$XDG_CONFIG_HOME/fex/session.txt`. Plain `key<TAB>value` lines, so a
//! damaged file loses only the lines it damages; paths are stored with `%`
//! escapes for the bytes that would break a line. When several windows are
//! open, the last one closed is the one remembered (as in browsers).

use std::path::PathBuf;

use super::fsys::SortKey;
use super::sys;

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub tabs: Vec<Vec<u8>>,
    pub active: usize,
    pub sort: SortKey,
    pub descending: bool,
    pub show_hidden: bool,
    pub recursive: bool,
    pub regex: bool,
    pub network_space: bool,
    /// Window placement in points: outer top-left and inner size.
    pub pos: Option<[f32; 2]>,
    pub size: Option<[f32; 2]>,
    pub maximized: bool,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            tabs: Vec::new(),
            active: 0,
            sort: SortKey::Name,
            descending: false,
            show_hidden: false,
            recursive: true,
            regex: false,
            network_space: false,
            pos: None,
            size: None,
            maximized: false,
        }
    }
}

/// `$XDG_CONFIG_HOME/fex` (default `~/.config/fex`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(sys::os(&sys::home())).join(".config"))
        .join("fex")
}

/// `$XDG_DATA_HOME/fex` (default `~/.local/share/fex`).
pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(sys::os(&sys::home())).join(".local/share"))
        .join("fex")
}

pub fn escape(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c == b'%' || c < 0x20 || c == 0x7f || c >= 0x80 {
            s.push_str(&format!("%{c:02X}"));
        } else {
            s.push(c as char);
        }
    }
    s
}

pub fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn sort_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Modified => "modified",
        SortKey::Type => "type",
        SortKey::Size => "size",
    }
}

fn pair(v: &str) -> Option<[f32; 2]> {
    let (a, b) = v.split_once(',')?;
    let (a, b) = (a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?);
    (a.is_finite() && b.is_finite()).then_some([a, b])
}

impl Session {
    pub fn parse(text: &str) -> Session {
        let mut s = Session::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('\t') else { continue };
            match key {
                "tab" if !value.is_empty() => s.tabs.push(unescape(value)),
                "active" => s.active = value.parse().unwrap_or(0),
                "sort" => {
                    let (name, dir) = value.split_once(' ').unwrap_or((value, "asc"));
                    s.sort = match name {
                        "modified" => SortKey::Modified,
                        "type" => SortKey::Type,
                        "size" => SortKey::Size,
                        _ => SortKey::Name,
                    };
                    s.descending = dir == "desc";
                }
                "hidden" => s.show_hidden = value == "1",
                "subfolders" => s.recursive = value != "0",
                "regex" => s.regex = value == "1",
                "network_space" => s.network_space = value == "1",
                "pos" => s.pos = pair(value),
                "size" => s.size = pair(value).filter(|[w, h]| *w >= 200.0 && *h >= 150.0),
                "maximized" => s.maximized = value == "1",
                _ => {}
            }
        }
        s.active = s.active.min(s.tabs.len().saturating_sub(1));
        s
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for tab in &self.tabs {
            out.push_str(&format!("tab\t{}\n", escape(tab)));
        }
        out.push_str(&format!("active\t{}\n", self.active));
        out.push_str(&format!("sort\t{} {}\n", sort_name(self.sort), if self.descending { "desc" } else { "asc" }));
        let flag = |b: bool| if b { "1" } else { "0" };
        out.push_str(&format!(
            "hidden\t{}\nsubfolders\t{}\nregex\t{}\nnetwork_space\t{}\n",
            flag(self.show_hidden),
            flag(self.recursive),
            flag(self.regex),
            flag(self.network_space)
        ));
        if let Some([x, y]) = self.pos {
            out.push_str(&format!("pos\t{x:.0},{y:.0}\n"));
        }
        if let Some([w, h]) = self.size {
            out.push_str(&format!("size\t{w:.0},{h:.0}\n"));
        }
        out.push_str(&format!("maximized\t{}\n", flag(self.maximized)));
        out
    }

    pub fn load() -> Option<Session> {
        std::fs::read_to_string(config_dir().join("session.txt")).ok().map(|t| Session::parse(&t))
    }

    pub fn save(&self) {
        let dir = config_dir();
        let _ = std::fs::create_dir_all(&dir);
        // Write then rename, so a crash mid-write never leaves half a session.
        let (path, tmp) = (dir.join("session.txt"), dir.join(format!("session.{}.tmp", std::process::id())));
        if std::fs::write(&tmp, self.to_text()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_damage() {
        let s = Session {
            tabs: vec![b"/home/me".to_vec(), b"Recent".to_vec(), b"/odd\n\tname\xff%".to_vec()],
            active: 2,
            sort: SortKey::Size,
            descending: true,
            show_hidden: true,
            recursive: false,
            regex: true,
            network_space: true,
            pos: Some([10.0, 20.0]),
            size: Some([900.0, 600.0]),
            maximized: true,
        };
        assert_eq!(Session::parse(&s.to_text()), s);

        let damaged = Session::parse("tab\t/\nactive\t9\ngarbage\nsize\tnan,3\npos\t1,");
        assert_eq!(damaged.tabs, vec![b"/".to_vec()]);
        assert_eq!(damaged.active, 0);
        assert_eq!((damaged.size, damaged.pos), (None, None));
        assert!(damaged.recursive, "missing settings keep their defaults");
    }
}
