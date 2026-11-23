//! Quick diff, like VS Code's gutter: each editor line is marked when it differs
//! from the file's version in the Git index (what `git diff` compares against), so
//! staging or committing a change clears its marks. Unsaved edits count too.
use similar::{capture_diff_slices_deadline, Algorithm, DiffOp};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    /// Lines were removed just below this one.
    Deleted,
    /// Lines were removed above this one, the first line.
    DeletedAbove,
}

/// What one buffer is compared against, and its marks for the current text.
#[derive(Default)]
pub struct State {
    /// The file this state belongs to; a buffer slot reused for another file resets.
    pub path: Option<PathBuf>,
    /// The index version's lines; `None` when untracked or outside Git.
    base: Option<Vec<String>>,
    pending: Option<Receiver<Option<String>>>,
    /// Bumped whenever a new base arrives, so marks are recomputed.
    base_version: u64,
    /// (buffer revision, base version) the marks were computed for.
    computed: Option<(u64, u64)>,
    pub marks: Vec<Option<Change>>,
}

impl State {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            ..Self::default()
        }
    }
    /// Whether marks were computed against a fetched index version.
    #[cfg(test)]
    pub fn computed_base(&self) -> bool {
        self.pending.is_none() && self.base.is_some() && self.computed.is_some()
    }
    /// Ask Git for the index version again (after commits, staging, checkouts).
    pub fn refetch(&mut self) {
        if let (Some(path), None) = (&self.path, &self.pending) {
            self.pending = Some(fetch(path.clone()));
        }
    }
    /// Take a fetched base and recompute the marks if anything changed. Returns
    /// whether the marks did.
    pub fn update(&mut self, revision: u64, text: impl FnOnce() -> String) -> bool {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(base) => {
                    self.pending = None;
                    let base: Option<Vec<String>> =
                        base.map(|b| b.lines().map(str::to_owned).collect());
                    if base != self.base {
                        self.base = base;
                        self.base_version += 1;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.computed == Some((revision, self.base_version)) {
            return false;
        }
        self.computed = Some((revision, self.base_version));
        let marks = match &self.base {
            Some(base) => {
                let text = text();
                let lines: Vec<&str> = text.lines().collect();
                let base: Vec<&str> = base.iter().map(String::as_str).collect();
                marks(&base, &lines)
            }
            None => Vec::new(),
        };
        let changed = marks != self.marks;
        self.marks = marks;
        changed
    }
}

/// Mark each line of `current` that differs from `base`.
pub fn marks(base: &[&str], current: &[&str]) -> Vec<Option<Change>> {
    let mut marks = vec![None; current.len()];
    // A huge rewrite gives up on the minimal diff rather than stall typing.
    let deadline = Instant::now() + Duration::from_millis(50);
    for op in capture_diff_slices_deadline(Algorithm::Myers, base, current, Some(deadline)) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            } => mark(&mut marks, new_index, new_len, Change::Added),
            DiffOp::Replace {
                new_index, new_len, ..
            } => mark(&mut marks, new_index, new_len, Change::Modified),
            DiffOp::Delete { new_index, .. } => {
                let (line, change) = match new_index.checked_sub(1) {
                    Some(above) => (above, Change::Deleted),
                    None => (0, Change::DeletedAbove),
                };
                if let Some(slot) = marks.get_mut(line) {
                    slot.get_or_insert(change);
                }
            }
        }
    }
    marks
}

fn mark(marks: &mut [Option<Change>], start: usize, len: usize, change: Change) {
    for slot in marks.iter_mut().skip(start).take(len) {
        *slot = Some(change);
    }
}

/// The file's content in the Git index, read on a worker; `None` when the file is
/// not tracked (or not in a repository), so it gets no marks.
fn fetch(path: PathBuf) -> Receiver<Option<String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(index_version(&path));
    });
    receiver
}

fn index_version(path: &Path) -> Option<String> {
    let directory = path.parent()?;
    let name = path.file_name()?.to_str()?;
    let bytes = crate::workspace::git_limited(
        directory,
        &["show", &format!(":./{name}")],
        crate::editor::MAX_FILE_SIZE as usize,
    )?;
    crate::encoding::decode(&bytes).map(|decoded| decoded.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_marked_like_vs_code() {
        let base = ["a", "b", "c", "d", "e"];
        // Unchanged: no marks.
        assert!(marks(&base, &base).iter().all(Option::is_none));
        // A new line after "b".
        let added = marks(&base, &["a", "b", "new", "c", "d", "e"]);
        assert_eq!(added[2], Some(Change::Added));
        assert_eq!(added.iter().flatten().count(), 1);
        // "c" edited in place.
        let modified = marks(&base, &["a", "b", "C!", "d", "e"]);
        assert_eq!(modified[2], Some(Change::Modified));
        assert_eq!(modified.iter().flatten().count(), 1);
        // "c" and "d" removed: the line above the gap carries the mark.
        let deleted = marks(&base, &["a", "b", "e"]);
        assert_eq!(deleted, [None, Some(Change::Deleted), None]);
        assert_eq!(
            marks(&base, &["a", "c", "d", "e"])[0],
            Some(Change::Deleted)
        );
        // Removing the first line marks the top of the new first line.
        assert_eq!(
            marks(&base, &["b", "c", "d", "e"])[0],
            Some(Change::DeletedAbove)
        );
        // Everything removed: nothing left to mark, nothing to crash on.
        assert!(marks(&base, &[]).is_empty());
    }

    #[test]
    fn marks_follow_edits_and_the_index() {
        let mut state = State {
            base: Some(vec!["one".into(), "two".into()]),
            ..State::default()
        };
        assert!(state.update(1, || "one\ntwo\nthree\n".into()));
        assert_eq!(state.marks, [None, None, Some(Change::Added)]);
        assert!(!state.update(1, || unreachable!("unchanged revision is cached")));
        // The change was staged: the index now matches and the mark goes away.
        state.base = Some(vec!["one".into(), "two".into(), "three".into()]);
        state.base_version += 1;
        assert!(state.update(1, || "one\ntwo\nthree\n".into()));
        assert!(state.marks.iter().all(Option::is_none));
    }
}
