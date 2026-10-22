//! What FEx restores at the next start (`%APPDATA%\FEx\session.txt`): open
//! tabs, view settings and window placement. Plain `key<TAB>value` lines, so
//! a damaged file loses only the lines it damages. When several windows are
//! open, the last one closed is the one remembered (as in browsers).

use std::path::PathBuf;

use crate::fsys::SortKey;

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub tabs: Vec<String>,
    pub active: usize,
    pub sort: SortKey,
    pub descending: bool,
    pub show_hidden: bool,
    pub recursive: bool,
    pub regex: bool,
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
            pos: None,
            size: None,
            maximized: false,
        }
    }
}

fn file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|root| PathBuf::from(root).join("FEx").join("session.txt"))
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
                "tab" if !value.is_empty() => s.tabs.push(value.to_string()),
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
            out.push_str(&format!("tab\t{tab}\r\n"));
        }
        out.push_str(&format!("active\t{}\r\n", self.active));
        out.push_str(&format!("sort\t{} {}\r\n", sort_name(self.sort), if self.descending { "desc" } else { "asc" }));
        let flag = |b: bool| if b { "1" } else { "0" };
        out.push_str(&format!("hidden\t{}\r\nsubfolders\t{}\r\nregex\t{}\r\n", flag(self.show_hidden), flag(self.recursive), flag(self.regex)));
        if let Some([x, y]) = self.pos {
            out.push_str(&format!("pos\t{x:.0},{y:.0}\r\n"));
        }
        if let Some([w, h]) = self.size {
            out.push_str(&format!("size\t{w:.0},{h:.0}\r\n"));
        }
        out.push_str(&format!("maximized\t{}\r\n", flag(self.maximized)));
        out
    }

    pub fn load() -> Option<Session> {
        std::fs::read_to_string(file()?).ok().map(|t| Session::parse(&t))
    }

    pub fn save(&self) {
        let Some(path) = file() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write then rename, so a crash mid-write never leaves half a session.
        let tmp = path.with_extension("tmp");
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
            tabs: vec![r"C:\Users".into(), "Recent".into(), r"zip:C:\a.zip|x/y".into()],
            active: 2,
            sort: SortKey::Size,
            descending: true,
            show_hidden: true,
            recursive: false,
            regex: true,
            pos: Some([10.0, 20.0]),
            size: Some([900.0, 600.0]),
            maximized: true,
        };
        assert_eq!(Session::parse(&s.to_text()), s);

        let damaged = Session::parse("tab\tC:\\\nactive\t9\ngarbage\nsize\tnan,3\npos\t1,");
        assert_eq!(damaged.tabs, vec![r"C:\".to_string()]);
        assert_eq!(damaged.active, 0);
        assert_eq!((damaged.size, damaged.pos), (None, None));
        assert!(damaged.recursive, "missing settings keep their defaults");
    }
}
