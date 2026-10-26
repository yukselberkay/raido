use anyhow::{bail, Context, Result};
use ropey::Rope;
use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
const UNDO_LIMIT: usize = 200;

#[derive(Clone)]
struct Snapshot {
    text: Rope,
    row: usize,
    col: usize,
}

pub struct Buffer {
    pub path: Option<PathBuf>,
    pub text: Rope,
    pub row: usize,
    pub col: usize,
    pub scroll: usize,
    pub hscroll: usize,
    pub revision: u64,
    pub dirty: bool,
    pub crlf: bool,
    pub search: String,
    /// Search options: `/` matches case exactly; the find bar ignores case unless the
    /// query has a capital letter, and can match whole words only.
    pub search_case: bool,
    pub search_word: bool,
    /// Selection start (char index) while selecting; the cursor is the other end.
    pub anchor: Option<usize>,
    saved: Rope,
    disk_hash: Option<u64>,
    pub disk_stamp: Option<(SystemTime, u64)>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    grouping: bool,
}

fn hash(text: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            path: None,
            text: Rope::new(),
            row: 0,
            col: 0,
            scroll: 0,
            hscroll: 0,
            revision: 0,
            dirty: false,
            crlf: false,
            search: String::new(),
            search_case: true,
            search_word: false,
            anchor: None,
            saved: Rope::new(),
            disk_hash: None,
            disk_stamp: None,
            undo: Vec::new(),
            redo: Vec::new(),
            grouping: false,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut buffer = Self::empty();
        if path.exists() {
            let canonical = path.canonicalize()?;
            let metadata = fs::metadata(&canonical)?;
            if !metadata.is_file() {
                bail!("Not a regular file: {}", path.display());
            }
            if metadata.len() > MAX_FILE_SIZE {
                bail!("File exceeds the 16 MiB editing limit");
            }
            let bytes = fs::read(&canonical)?;
            if bytes.len() as u64 > MAX_FILE_SIZE {
                bail!("File grew beyond the 16 MiB limit");
            }
            if bytes.contains(&0) {
                bail!("Binary files are not editable");
            }
            let source =
                std::str::from_utf8(&bytes).context("Raido currently edits UTF-8 files only")?;
            buffer.crlf = source.contains("\r\n");
            buffer.text = Rope::from_str(source);
            buffer.saved = buffer.text.clone();
            buffer.disk_hash = Some(hash(&bytes));
            buffer.disk_stamp = metadata.modified().ok().map(|time| (time, metadata.len()));
            buffer.path = Some(canonical);
        } else {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let canonical_parent = parent
                .canonicalize()
                .context("Parent directory must exist")?;
            let name = path.file_name().context("Missing file name")?;
            buffer.path = Some(canonical_parent.join(name));
        }
        Ok(buffer)
    }

    pub fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[scratch]".into())
    }

    pub fn lines(&self) -> usize {
        self.text.len_lines()
    }
    pub fn line(&self, row: usize) -> String {
        if row >= self.lines() {
            return String::new();
        }
        self.text
            .line(row)
            .to_string()
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    }
    pub fn line_len(&self, row: usize) -> usize {
        self.line(row).chars().count()
    }
    pub fn index(&self) -> usize {
        self.text.line_to_char(self.row) + self.col.min(self.line_len(self.row))
    }
    pub fn clamp(&mut self) {
        self.row = self.row.min(self.lines().saturating_sub(1));
        self.col = self.col.min(self.line_len(self.row));
    }
    pub fn move_by(&mut self, dx: isize, dy: isize) {
        self.row = self.row.saturating_add_signed(dy).min(self.lines() - 1);
        self.col = self
            .col
            .saturating_add_signed(dx)
            .min(self.line_len(self.row));
    }
    pub fn goto(&mut self, line: usize) {
        self.row = line.saturating_sub(1).min(self.lines() - 1);
        self.clamp();
    }
    pub fn home(&mut self) {
        self.col = 0;
    }
    pub fn end(&mut self) {
        self.col = self.line_len(self.row);
    }
    pub fn end_group(&mut self) {
        self.grouping = false;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            row: self.row,
            col: self.col,
        }
    }
    fn checkpoint(&mut self, grouped: bool) {
        if !grouped || !self.grouping {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.grouping = grouped;
        self.revision += 1;
        self.dirty = true;
    }
    pub fn set_index(&mut self, index: usize) {
        let i = index.min(self.text.len_chars());
        self.row = self.text.char_to_line(i);
        self.col = i - self.text.line_to_char(self.row);
        self.clamp();
    }
    pub fn insert(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.checkpoint(true);
        let index = self.index();
        self.text.insert(index, value);
        self.set_index(index + value.chars().count());
    }
    pub fn newline(&mut self) {
        let indent: String = self
            .line(self.row)
            .chars()
            .take(self.col)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let newline = if self.crlf { "\r\n" } else { "\n" };
        self.insert(&format!("{newline}{indent}"));
    }
    pub fn backspace(&mut self) {
        let i = self.index();
        if i == 0 {
            return;
        }
        self.checkpoint(true);
        let start = if i >= 2 && self.text.char(i - 1) == '\n' && self.text.char(i - 2) == '\r' {
            i - 2
        } else {
            i - 1
        };
        self.text.remove(start..i);
        self.set_index(start);
    }
    pub fn delete_char(&mut self) {
        let i = self.index();
        if i >= self.text.len_chars() {
            return;
        }
        self.checkpoint(false);
        let end = if self.text.char(i) == '\r'
            && i + 1 < self.text.len_chars()
            && self.text.char(i + 1) == '\n'
        {
            i + 2
        } else {
            i + 1
        };
        self.text.remove(i..end);
        self.clamp();
    }
    pub fn delete_line(&mut self) -> String {
        self.checkpoint(false);
        let start = self.text.line_to_char(self.row);
        let end = if self.row + 1 < self.lines() {
            self.text.line_to_char(self.row + 1)
        } else {
            self.text.len_chars()
        };
        let removed = self.text.slice(start..end).to_string();
        self.text.remove(start..end);
        self.clamp();
        removed
    }
    pub fn delete_to_end(&mut self) {
        let start = self.index();
        let end = self.text.line_to_char(self.row) + self.line_len(self.row);
        if start < end {
            self.checkpoint(false);
            self.text.remove(start..end);
            self.clamp();
        }
    }
    /// Selected char range `[start, end)`. `inclusive` covers the character under the
    /// cursor (Vim-style visual mode); `linewise` expands to whole lines.
    pub fn selection(&self, inclusive: bool, linewise: bool) -> Option<(usize, usize)> {
        let anchor = self.anchor?.min(self.text.len_chars());
        let cursor = self.index();
        let (start, end) = (anchor.min(cursor), anchor.max(cursor));
        if linewise {
            let first = self.text.char_to_line(start);
            let last = self.text.char_to_line(end);
            let stop = if last + 1 < self.lines() {
                self.text.line_to_char(last + 1)
            } else {
                self.text.len_chars()
            };
            return Some((self.text.line_to_char(first), stop));
        }
        let end = if inclusive { end + 1 } else { end };
        Some((start, end.min(self.text.len_chars())))
    }
    pub fn slice(&self, start: usize, end: usize) -> String {
        let len = self.text.len_chars();
        self.text.slice(start.min(len)..end.min(len)).to_string()
    }
    pub fn char_index(&self, row: usize, col: usize) -> usize {
        let row = row.min(self.lines().saturating_sub(1));
        self.text.line_to_char(row) + col.min(self.line_len(row))
    }
    /// Replace `[start, end)` with `value` as one undo step; the cursor lands after it.
    pub fn replace(&mut self, start: usize, end: usize, value: &str) {
        let len = self.text.len_chars();
        let (start, end) = (start.min(len), end.min(len));
        if start == end && value.is_empty() {
            return;
        }
        self.checkpoint(false);
        self.text.remove(start..end);
        self.text.insert(start, value);
        self.set_index(start + value.chars().count());
    }
    /// Apply non-overlapping `(start, end, text)` char-range edits as one undo step,
    /// keeping the cursor on its line.
    pub fn apply_edits(&mut self, mut edits: Vec<(usize, usize, String)>) {
        if edits.is_empty() {
            return;
        }
        self.checkpoint(false);
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let len = self.text.len_chars();
        for (start, end, value) in edits {
            let (start, end) = (start.min(len), end.min(len).max(start.min(len)));
            if end <= self.text.len_chars() {
                self.text.remove(start..end);
                self.text.insert(start, &value);
            }
        }
        self.clamp();
    }
    /// Insert whole lines below (or above) the cursor line and move onto them.
    pub fn paste_lines(&mut self, value: &str, above: bool) {
        if value.is_empty() {
            return;
        }
        self.checkpoint(false);
        let row = self.row;
        let eol = if self.crlf { "\r\n" } else { "\n" };
        if above {
            let index = self.text.line_to_char(row);
            self.text.insert(
                index,
                &format!("{}{eol}", value.trim_end_matches(['\r', '\n'])),
            );
            self.col = 0;
            return;
        }
        if row + 1 < self.lines() {
            let index = self.text.line_to_char(row + 1);
            self.text.insert(
                index,
                &format!("{}{eol}", value.trim_end_matches(['\r', '\n'])),
            );
        } else {
            self.text.insert(
                self.text.len_chars(),
                &format!("{eol}{}", value.trim_end_matches(['\r', '\n'])),
            );
        }
        self.row = row + 1;
        self.col = 0;
    }
    pub fn open_line(&mut self, above: bool) {
        self.end_group();
        self.checkpoint(true);
        let indent: String = self
            .line(self.row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let eol = if self.crlf { "\r\n" } else { "\n" };
        if above {
            self.text
                .insert(self.text.line_to_char(self.row), &format!("{indent}{eol}"));
        } else if self.row + 1 < self.lines() {
            self.text.insert(
                self.text.line_to_char(self.row + 1),
                &format!("{indent}{eol}"),
            );
            self.row += 1;
        } else {
            self.text
                .insert(self.text.len_chars(), &format!("{eol}{indent}"));
            self.row += 1;
        }
        self.col = indent.chars().count();
    }
    pub fn undo(&mut self, redo: bool) {
        self.end_group();
        let state = if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        };
        if let Some(state) = state {
            let current = self.snapshot();
            if redo {
                self.undo.push(current);
            } else {
                self.redo.push(current);
            }
            self.text = state.text;
            self.row = state.row;
            self.col = state.col;
            self.revision += 1;
            self.dirty = self.text != self.saved;
            self.clamp();
        }
    }
    pub fn word(&mut self, backwards: bool) {
        let len = self.text.len_chars();
        let mut i = self.index();
        if backwards {
            i = i.saturating_sub(1);
            while i > 0 && kind(self.text.char(i)) == 0 {
                i -= 1;
            }
            if i < len {
                let k = kind(self.text.char(i));
                while i > 0 && kind(self.text.char(i - 1)) == k {
                    i -= 1;
                }
            }
        } else if i < len {
            let k = kind(self.text.char(i));
            while i < len && kind(self.text.char(i)) == k {
                i += 1;
            }
            while i < len && kind(self.text.char(i)) == 0 {
                i += 1;
            }
        }
        self.set_index(i);
    }
    /// To the end of this or the next word, like Option+→ in a Mac text field.
    pub fn word_end(&mut self) {
        let len = self.text.len_chars();
        let mut i = self.index();
        while i < len && kind(self.text.char(i)) == 0 {
            i += 1;
        }
        if i < len {
            let k = kind(self.text.char(i));
            while i < len && kind(self.text.char(i)) == k {
                i += 1;
            }
        }
        self.set_index(i);
    }
    /// Delete back to the start of the word, like Option+Backspace.
    pub fn delete_word_back(&mut self) {
        let end = self.index();
        self.word(true);
        let start = self.index();
        if start < end {
            self.replace(start, end, "");
        }
    }
    /// Char ranges of every match of the search, in order.
    pub fn search_ranges(&self) -> Vec<(usize, usize)> {
        if self.search.is_empty() {
            return Vec::new();
        }
        let all = self.text.to_string();
        find_all(&all, &self.search, !self.search_case, self.search_word)
            .into_iter()
            .map(|(start, end)| (self.text.byte_to_char(start), self.text.byte_to_char(end)))
            .collect()
    }
    /// Move to the next match after the cursor (or the one before), wrapping around.
    pub fn find(&mut self, backwards: bool) -> bool {
        let ranges = self.search_ranges();
        let i = self.index();
        let found = if backwards {
            ranges.iter().rev().find(|r| r.0 < i).or(ranges.last())
        } else {
            ranges.iter().find(|r| r.0 > i).or(ranges.first())
        };
        match found {
            Some(&(start, _)) => {
                self.set_index(start);
                true
            }
            None => false,
        }
    }
    /// Move to the first match starting at `index` or later, wrapping around.
    pub fn find_from(&mut self, index: usize) -> bool {
        let ranges = self.search_ranges();
        match ranges.iter().find(|r| r.0 >= index).or(ranges.first()) {
            Some(&(start, _)) => {
                self.set_index(start);
                true
            }
            None => false,
        }
    }
    /// (which match the cursor is on, counting from 1, or 0; how many there are).
    pub fn match_position(&self) -> (usize, usize) {
        let ranges = self.search_ranges();
        let i = self.index();
        let at = ranges.iter().position(|r| r.0 == i).map_or(0, |n| n + 1);
        (at, ranges.len())
    }
    /// Replace the match under the cursor and move to the next one. False when the
    /// cursor isn't on a match (it moves to the next one instead).
    pub fn replace_match(&mut self, with: &str) -> bool {
        let i = self.index();
        let Some((start, end)) = self.search_ranges().into_iter().find(|r| r.0 == i) else {
            self.find(false);
            return false;
        };
        self.replace(start, end, with);
        let after = self.index();
        self.find_from(after);
        true
    }
    /// Replace every match as one undo step; returns how many.
    pub fn replace_all(&mut self, with: &str) -> usize {
        let ranges = self.search_ranges();
        let count = ranges.len();
        self.apply_edits(
            ranges
                .into_iter()
                .map(|(start, end)| (start, end, with.to_string()))
                .collect(),
        );
        count
    }
    pub fn save(&mut self, new_path: Option<PathBuf>) -> Result<()> {
        let proposed = new_path
            .or_else(|| self.path.clone())
            .context("Use :w filename to name this buffer")?;
        let path = if proposed.exists() {
            proposed.canonicalize()?
        } else {
            proposed
        };
        let existing = if path.exists() {
            let metadata = fs::metadata(&path)?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_SIZE {
                bail!("Save destination is not a regular file within the 16 MiB limit");
            }
            Some(fs::read(&path)?)
        } else {
            None
        };
        let same_path = self.path.as_ref() == Some(&path);
        if let Some(bytes) = &existing {
            if !same_path || Some(hash(bytes)) != self.disk_hash {
                bail!("File changed on disk or destination exists; refusing to overwrite. Reload or save to a new name.");
            }
            // Atomic rename must not silently replace a read-only file.
            if fs::metadata(&path)?.permissions().readonly() {
                bail!("File is read-only");
            }
        } else if same_path && self.disk_hash.is_some() {
            bail!("File was deleted on disk; save to a new name");
        }
        let parent = path.parent().context("Missing parent directory")?;
        let serial = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = parent.join(format!(".raido-{}-{serial}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            if path.exists() {
                file.set_permissions(fs::metadata(&path)?.permissions())?;
            }
            for chunk in self.text.chunks() {
                file.write_all(chunk.as_bytes())?;
            }
            file.sync_all()?;
            // Recheck after writing the temporary file to narrow the race window.
            let now = if path.exists() {
                Some(hash(&fs::read(&path)?))
            } else {
                None
            };
            if now != existing.as_ref().map(|b| hash(b)) {
                bail!("File changed during save");
            }
            fs::rename(&temp, &path)?;
            if let Ok(directory) = fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.disk_hash = Some(hash(self.text.to_string().as_bytes()));
        self.saved = self.text.clone();
        self.path = Some(path);
        self.disk_stamp = self
            .path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok().map(|time| (time, m.len())));
        self.dirty = false;
        self.end_group();
        Ok(())
    }
}

/// Word motions see three kinds of text: spaces, word characters, punctuation.
fn kind(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Byte ranges of `needle` in `haystack`, left to right and not overlapping;
/// `ignore_case` compares letters case-insensitively, `whole_word` keeps only matches
/// with no letter, digit or `_` right before or after them.
pub fn find_all(
    haystack: &str,
    needle: &str,
    ignore_case: bool,
    whole_word: bool,
) -> Vec<(usize, usize)> {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut found = Vec::new();
    if needle.is_empty() {
        return found;
    }
    let mut from = 0;
    while from < haystack.len() {
        let next = if ignore_case {
            haystack[from..].char_indices().find_map(|(at, _)| {
                let start = from + at;
                let mut rest = haystack[start..].char_indices();
                let mut end = start;
                for wanted in needle.chars() {
                    let (offset, c) = rest.next()?;
                    if !c.to_lowercase().eq(wanted.to_lowercase()) {
                        return None;
                    }
                    end = start + offset + c.len_utf8();
                }
                Some((start, end))
            })
        } else {
            haystack[from..]
                .find(needle)
                .map(|at| (from + at, from + at + needle.len()))
        };
        let Some((start, end)) = next else {
            break;
        };
        let alone = !whole_word
            || (!word(haystack[..start].chars().next_back())
                && !word(haystack[end..].chars().next()));
        if alone {
            found.push((start, end));
            from = end;
        } else {
            from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn find_all_ignores_case_and_keeps_whole_words() {
        let text = "Ünïcode ünïcode_x x ünïcode";
        assert_eq!(find_all(text, "ünïcode", false, false).len(), 2);
        assert_eq!(find_all(text, "ünïcode", true, false).len(), 3);
        let words = find_all(text, "ünïcode", true, true);
        assert_eq!(words.len(), 2, "not inside ünïcode_x");
        assert_eq!(&text[words[1].0..words[1].1], "ünïcode");
        assert_eq!(find_all("aaaa", "aa", false, false), vec![(0, 2), (2, 4)]);
        assert_eq!(find_all("x", "", true, true), vec![]);
        assert_eq!(find_all("ab_ab ab", "ab", false, true), vec![(6, 8)]);
    }
    #[test]
    fn unicode_edit_undo_redo() {
        let mut b = Buffer::empty();
        b.insert("λ你好\nRust 🦀");
        b.backspace();
        assert_eq!(b.text.to_string(), "λ你好\nRust ");
        b.end_group();
        b.undo(false);
        assert_eq!(b.text.len_chars(), 0);
        b.undo(true);
        assert_eq!(b.text.to_string(), "λ你好\nRust ");
    }
    #[test]
    fn save_preserves_crlf_and_rejects_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.end();
        b.newline();
        b.insert("c");
        b.save(None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\r\nc\r\nb\r\n");
        fs::write(&path, "external").unwrap();
        assert!(b.save(None).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "external");
    }
    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_permissions_and_new_destination() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.insert("# hello\n");
        b.save(None).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let other = dir.path().join("other");
        fs::write(&other, "keep").unwrap();
        assert!(b.save(Some(other.clone())).is_err());
        assert_eq!(fs::read_to_string(other).unwrap(), "keep");
    }
    #[test]
    fn rejects_binary_and_wraps_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary");
        fs::write(&path, [1, 0, 2]).unwrap();
        assert!(Buffer::open(&path).is_err());
        let mut b = Buffer::empty();
        b.insert("one two one");
        b.search = "one".into();
        assert!(b.find(false));
        assert_eq!(b.col, 0);
        assert!(b.find(false));
        assert_eq!(b.col, 8);
    }
}
