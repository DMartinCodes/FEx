//! Undo/redo for file operations done in FEx. Every entry records something
//! that happened; undoing runs its inverse, redoing runs it again.

use crate::ops::Kind;

/// Entries kept per stack; the oldest are forgotten first.
const LIMIT: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Rename { from: String, to: String },
    /// Top-level (source, destination) pairs a transfer created.
    Transfer { kind: Kind, pairs: Vec<(String, String)> },
    /// Items sent to the Recycle Bin.
    Recycle(Vec<String>),
    /// Items that didn't exist before (new folder, new file, extraction, ...).
    Create(Vec<String>),
}

/// The filesystem work that carries out an undo or redo.
#[derive(Debug, PartialEq)]
pub enum Step {
    Rename { from: String, to: String },
    Transfer { kind: Kind, pairs: Vec<(String, String)> },
    Recycle(Vec<String>),
    Restore(Vec<String>),
}

impl Entry {
    pub fn undo_step(&self) -> Step {
        match self {
            Entry::Rename { from, to } => Step::Rename { from: to.clone(), to: from.clone() },
            Entry::Transfer { kind: Kind::Move, pairs } => {
                Step::Transfer { kind: Kind::Move, pairs: pairs.iter().map(|(s, d)| (d.clone(), s.clone())).collect() }
            }
            Entry::Transfer { kind: Kind::Copy, pairs } => Step::Recycle(pairs.iter().map(|(_, d)| d.clone()).collect()),
            Entry::Recycle(paths) => Step::Restore(paths.clone()),
            Entry::Create(paths) => Step::Recycle(paths.clone()),
        }
    }

    pub fn redo_step(&self) -> Step {
        match self {
            Entry::Rename { from, to } => Step::Rename { from: from.clone(), to: to.clone() },
            Entry::Transfer { kind, pairs } => Step::Transfer { kind: *kind, pairs: pairs.clone() },
            Entry::Recycle(paths) => Step::Recycle(paths.clone()),
            // Undoing a creation recycled it, so redo brings it back.
            Entry::Create(paths) => Step::Restore(paths.clone()),
        }
    }

    pub fn describe(&self) -> String {
        let name = |p: &str| p.trim_end_matches('\\').rsplit('\\').next().unwrap_or(p).to_string();
        let items = |n: usize, first: &str| if n == 1 { format!("'{}'", name(first)) } else { format!("{n} items") };
        match self {
            Entry::Rename { from, to } => format!("rename '{}' → '{}'", name(from), name(to)),
            Entry::Transfer { kind, pairs } => {
                let verb = if *kind == Kind::Move { "move" } else { "copy" };
                format!("{verb} of {}", items(pairs.len(), pairs.first().map_or("", |p| p.0.as_str())))
            }
            Entry::Recycle(paths) => format!("delete of {}", items(paths.len(), paths.first().map_or("", |p| p.as_str()))),
            Entry::Create(paths) => format!("creation of {}", items(paths.len(), paths.first().map_or("", |p| p.as_str()))),
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

impl Entry {
    fn is_empty(&self) -> bool {
        match self {
            Entry::Rename { .. } => false,
            Entry::Transfer { pairs, .. } => pairs.is_empty(),
            Entry::Recycle(paths) | Entry::Create(paths) => paths.is_empty(),
        }
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

    #[test]
    fn steps_invert_entries() {
        let rename = Entry::Rename { from: r"C:\a.txt".into(), to: r"C:\b.txt".into() };
        assert_eq!(rename.undo_step(), Step::Rename { from: r"C:\b.txt".into(), to: r"C:\a.txt".into() });
        let moved = Entry::Transfer { kind: Kind::Move, pairs: vec![(r"C:\x\f".into(), r"D:\f (2)".into())] };
        assert_eq!(moved.undo_step(), Step::Transfer { kind: Kind::Move, pairs: vec![(r"D:\f (2)".into(), r"C:\x\f".into())] });
        let copied = Entry::Transfer { kind: Kind::Copy, pairs: vec![(r"C:\x\f".into(), r"D:\f".into())] };
        assert_eq!(copied.undo_step(), Step::Recycle(vec![r"D:\f".into()]));
        assert_eq!(Entry::Create(vec![r"C:\n".into()]).redo_step(), Step::Restore(vec![r"C:\n".into()]));
    }

    #[test]
    fn new_actions_void_redo_but_redone_ones_do_not() {
        let mut h = History::default();
        let a = Entry::Create(vec!["a".into()]);
        let b = Entry::Create(vec!["b".into()]);
        h.record(a.clone());
        h.record(b.clone());
        for _ in 0..2 {
            let e = h.take_undo().unwrap();
            h.undone(e);
        }
        let e = h.take_redo().unwrap(); // redo `a`
        h.redone(e);
        assert_eq!(h.next_redo(), Some(&b));
        h.record(Entry::Recycle(vec!["c".into()]));
        assert_eq!(h.next_redo(), None);
        h.record(Entry::Transfer { kind: Kind::Copy, pairs: vec![] }); // nothing happened: not recorded
        assert_eq!(h.next_undo(), Some(&Entry::Recycle(vec!["c".into()])));
    }
}
