//! Undo/redo for file operations done in FEx (the Linux side of
//! `history.rs`). Every entry records something that happened; undoing runs
//! its inverse, redoing runs it again.

use super::fsys;
use super::ops::{Kind, Path};

/// Entries kept per stack; the oldest are forgotten first.
const LIMIT: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Rename { from: Path, to: Path },
    /// Top-level (source, destination) pairs a transfer created.
    Transfer { kind: Kind, pairs: Vec<(Path, Path)> },
    /// Items moved to the trash (by their original paths).
    Trash(Vec<Path>),
    /// Items that didn't exist before (new folder, new file, extraction, ...).
    Create(Vec<Path>),
}

/// The filesystem work that carries out an undo or redo.
#[derive(Debug, PartialEq)]
pub enum Step {
    Rename { from: Path, to: Path },
    Transfer { kind: Kind, pairs: Vec<(Path, Path)> },
    Trash(Vec<Path>),
    Restore(Vec<Path>),
}

impl Entry {
    pub fn undo_step(&self) -> Step {
        match self {
            Entry::Rename { from, to } => Step::Rename { from: to.clone(), to: from.clone() },
            Entry::Transfer { kind: Kind::Move, pairs } => {
                Step::Transfer { kind: Kind::Move, pairs: pairs.iter().map(|(s, d)| (d.clone(), s.clone())).collect() }
            }
            Entry::Transfer { kind: Kind::Copy, pairs } => Step::Trash(pairs.iter().map(|(_, d)| d.clone()).collect()),
            Entry::Trash(paths) => Step::Restore(paths.clone()),
            Entry::Create(paths) => Step::Trash(paths.clone()),
        }
    }

    pub fn redo_step(&self) -> Step {
        match self {
            Entry::Rename { from, to } => Step::Rename { from: from.clone(), to: to.clone() },
            Entry::Transfer { kind, pairs } => Step::Transfer { kind: *kind, pairs: pairs.clone() },
            Entry::Trash(paths) => Step::Trash(paths.clone()),
            // Undoing a creation trashed it, so redo brings it back.
            Entry::Create(paths) => Step::Restore(paths.clone()),
        }
    }

    pub fn describe(&self) -> String {
        let name = |p: &[u8]| String::from_utf8_lossy(fsys::base_name(p)).into_owned();
        let items = |n: usize, first: &[u8]| if n == 1 { format!("'{}'", name(first)) } else { format!("{n} items") };
        match self {
            Entry::Rename { from, to } => format!("rename '{}' → '{}'", name(from), name(to)),
            Entry::Transfer { kind, pairs } => {
                let verb = if *kind == Kind::Move { "move" } else { "copy" };
                format!("{verb} of {}", items(pairs.len(), pairs.first().map_or(&b""[..], |p| &p.0)))
            }
            Entry::Trash(paths) => format!("delete of {}", items(paths.len(), paths.first().map_or(&b""[..], |p| p))),
            Entry::Create(paths) => format!("creation of {}", items(paths.len(), paths.first().map_or(&b""[..], |p| p))),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Entry::Rename { .. } => false,
            Entry::Transfer { pairs, .. } => pairs.is_empty(),
            Entry::Trash(paths) | Entry::Create(paths) => paths.is_empty(),
        }
    }
}

#[derive(Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// An undo or redo is running; another waits until it finishes.
    pub busy: bool,
}

impl History {
    /// Something new happened: it becomes undoable and the redo stack is void.
    pub fn record(&mut self, entry: Entry) {
        if entry.is_empty() {
            return;
        }
        push(&mut self.undo, entry);
        self.redo.clear();
    }

    pub fn take_undo(&mut self) -> Option<Entry> {
        if self.busy { None } else { self.undo.pop() }
    }

    pub fn take_redo(&mut self) -> Option<Entry> {
        if self.busy { None } else { self.redo.pop() }
    }

    /// An undo finished: the entry can now be redone.
    pub fn undone(&mut self, entry: Entry) {
        push(&mut self.redo, entry);
    }

    /// A redo finished: the entry can be undone again (without voiding redo).
    pub fn redone(&mut self, entry: Entry) {
        if !entry.is_empty() {
            push(&mut self.undo, entry);
        }
    }

    /// An undo or redo failed before changing anything: keep it for a retry.
    pub fn put_back(&mut self, entry: Entry, undo: bool) {
        push(if undo { &mut self.undo } else { &mut self.redo }, entry);
    }

    pub fn next_undo(&self) -> Option<&Entry> {
        self.undo.last()
    }

    pub fn next_redo(&self) -> Option<&Entry> {
        self.redo.last()
    }
}

fn push(stack: &mut Vec<Entry>, entry: Entry) {
    if stack.len() == LIMIT {
        stack.remove(0);
    }
    stack.push(entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Path {
        s.as_bytes().to_vec()
    }

    #[test]
    fn steps_invert_entries() {
        let rename = Entry::Rename { from: v("/a.txt"), to: v("/b.txt") };
        assert_eq!(rename.undo_step(), Step::Rename { from: v("/b.txt"), to: v("/a.txt") });
        let moved = Entry::Transfer { kind: Kind::Move, pairs: vec![(v("/x/f"), v("/y/f (2)"))] };
        assert_eq!(moved.undo_step(), Step::Transfer { kind: Kind::Move, pairs: vec![(v("/y/f (2)"), v("/x/f"))] });
        let copied = Entry::Transfer { kind: Kind::Copy, pairs: vec![(v("/x/f"), v("/y/f"))] };
        assert_eq!(copied.undo_step(), Step::Trash(vec![v("/y/f")]));
        assert_eq!(Entry::Create(vec![v("/n")]).redo_step(), Step::Restore(vec![v("/n")]));
        assert_eq!(rename.describe(), "rename 'a.txt' → 'b.txt'");
    }

    #[test]
    fn new_actions_void_redo_but_redone_ones_do_not() {
        let mut h = History::default();
        let a = Entry::Create(vec![v("a")]);
        let b = Entry::Create(vec![v("b")]);
        h.record(a.clone());
        h.record(b.clone());
        for _ in 0..2 {
            let e = h.take_undo().unwrap();
            h.undone(e);
        }
        let e = h.take_redo().unwrap();
        h.redone(e);
        assert_eq!(h.next_redo(), Some(&b));
        h.record(Entry::Trash(vec![v("c")]));
        assert_eq!(h.next_redo(), None);
        h.record(Entry::Transfer { kind: Kind::Copy, pairs: vec![] });
        assert_eq!(h.next_undo(), Some(&Entry::Trash(vec![v("c")])));
    }
}
