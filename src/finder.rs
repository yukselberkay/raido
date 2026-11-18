//! Ctrl-p: open a file by a few of its letters, as VS Code's quick open and fzf do.
//! `mnrs` finds `src/main.rs`: the letters in order, anywhere in the path, ranked by
//! how well they fall (the start of a name or word, one after another, in the file's
//! own name), shorter paths first among equals. With nothing typed, the files you
//! opened last come first. The project's files are Git's list (tracked and new, not
//! ignored), else a walk that skips build output, read in the background.
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

/// The most files listed, best first.
const SHOWN: usize = 200;

pub struct Finder {
    pub query: String,
    /// Every file in the project, relative to its root, with `/` between folders.
    pub files: Vec<String>,
    /// Files opened last, most recent first (relative to the root).
    pub recent: Vec<String>,
    /// What's listed: indexes into `files`, with the characters that matched.
    pub shown: Vec<(usize, Vec<usize>)>,
    pub selected: usize,
    /// The file list, while it's being read.
    loading: Option<Receiver<Vec<String>>>,
}

impl Finder {
    /// Open on `root`'s files (the list from last time, if any, until the new one is
    /// read), the `recent` ones first.
    pub fn new(root: &Path, known: Vec<String>, recent: Vec<String>) -> Self {
        let (send, loading) = mpsc::channel();
        let root = root.to_path_buf();
        thread::spawn(move || {
            let _ = send.send(project_files(&root));
        });
        let mut finder = Self {
            query: String::new(),
            files: known,
            recent,
            shown: Vec::new(),
            selected: 0,
            loading: Some(loading),
        };
        finder.refilter();
        finder
    }

    /// Whether the file list is still being read.
    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Take the file list once it's read; true when the list changed.
    pub fn poll(&mut self) -> bool {
        let Some(files) = self.loading.as_ref().and_then(|l| l.try_recv().ok()) else {
            return false;
        };
        self.loading = None;
        self.files = files;
        self.refilter();
        true
    }

    /// The file under the selection.
    pub fn chosen(&self) -> Option<&str> {
        let (index, _) = self.shown.get(self.selected)?;
        Some(&self.files[*index])
    }

    /// The query without a `:line` at its end, and that line.
    pub fn target(&self) -> (&str, Option<usize>) {
        if let Some((path, line)) = self.query.rsplit_once(':') {
            if let Ok(line) = line.parse::<usize>() {
                return (path, Some(line));
            }
        }
        (&self.query, None)
    }

    /// List what matches the query, best first; with none, the recent files first.
    pub fn refilter(&mut self) {
        self.selected = 0;
        let (query, _) = self.target();
        let query = query.trim().to_string();
        if query.is_empty() {
            let rank = |file: &String| {
                self.recent
                    .iter()
                    .position(|r| r == file)
                    .unwrap_or(usize::MAX)
            };
            let mut order: Vec<usize> = (0..self.files.len()).collect();
            order.sort_by_key(|&i| (rank(&self.files[i]), i));
            self.shown = order
                .into_iter()
                .take(SHOWN)
                .map(|i| (i, Vec::new()))
                .collect();
            return;
        }
        let mut scored: Vec<(i64, usize, Vec<usize>)> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(i, file)| {
                let (score, at) = fuzzy(&query, file)?;
                Some((score, i, at))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| self.files[a.1].len().cmp(&self.files[b.1].len()))
                .then_with(|| self.files[a.1].cmp(&self.files[b.1]))
        });
        self.shown = scored
            .into_iter()
            .take(SHOWN)
            .map(|(_, i, at)| (i, at))
            .collect();
    }

    pub fn step(&mut self, by: isize) {
        let len = self.shown.len();
        if len > 0 {
            self.selected = (self.selected as isize + by).rem_euclid(len as isize) as usize;
        }
    }
}

/// How well `query`'s characters fall in `path`, in order, and which characters of
/// `path` they are; `None` when they aren't all there. Case is ignored unless the
/// query has a capital.
pub fn fuzzy(query: &str, path: &str) -> Option<(i64, Vec<usize>)> {
    let wanted: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if wanted.is_empty() {
        return Some((0, Vec::new()));
    }
    let exact = wanted.iter().any(|c| c.is_uppercase());
    let chars: Vec<char> = path.chars().collect();
    let same = |a: char, b: char| {
        if exact {
            a == b
        } else {
            a.to_lowercase().eq(b.to_lowercase())
        }
    };
    // Where they first all fall, then the narrowest stretch ending there: the
    // letters as close together as they go (fzf's way).
    let mut end = None;
    let mut next = 0;
    for (i, &c) in chars.iter().enumerate() {
        if same(c, wanted[next]) {
            next += 1;
            if next == wanted.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    let mut start = end;
    let mut left = wanted.len();
    for i in (0..=end).rev() {
        if same(chars[i], wanted[left - 1]) {
            left -= 1;
            if left == 0 {
                start = i;
                break;
            }
        }
    }
    // Score that stretch, matching forward from its start.
    let name_start = chars.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let mut at = Vec::with_capacity(wanted.len());
    let mut score: i64 = 0;
    let mut next = 0;
    let mut previous: Option<usize> = None;
    for (i, &c) in chars.iter().enumerate().take(end + 1).skip(start) {
        if next < wanted.len() && same(c, wanted[next]) {
            let before = i.checked_sub(1).map(|b| chars[b]);
            let boundary = match before {
                None => 10,
                Some('/') => 9,
                Some('_' | '-' | '.' | ' ') => 7,
                Some(b) if b.is_lowercase() && c.is_uppercase() => 7,
                _ => 0,
            };
            score += 16 + boundary;
            match previous {
                Some(p) if p + 1 == i => score += 8,
                Some(p) => score -= 3 + (i - p - 1).min(10) as i64,
                None => {}
            }
            if i >= name_start {
                score += 4;
            }
            at.push(i);
            previous = Some(i);
            next += 1;
        }
    }
    // All of it in the file's own name, from its first letter: the best there is.
    if start >= name_start {
        score += 12;
        if start == name_start {
            score += 8;
        }
    }
    Some((score, at))
}

/// The project's files, relative to `root` with `/` between folders.
pub fn project_files(root: &Path) -> Vec<String> {
    crate::search::project_files(root)
        .into_iter()
        .map(|p: PathBuf| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn best<'a>(query: &str, files: &[&'a str]) -> Vec<&'a str> {
        let mut scored: Vec<(i64, &str)> = files
            .iter()
            .filter_map(|f| fuzzy(query, f).map(|(s, _)| (s, *f)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
        scored.into_iter().map(|(_, f)| f).collect()
    }

    #[test]
    fn letters_in_order_find_a_file_and_rank_it_by_how_they_fall() {
        let files = [
            "src/main.rs",
            "src/markdown.rs",
            "src/model_server.rs",
            "docs/manual.md",
            "README.md",
            "src/app.rs",
            "tests/main_test.rs",
        ];
        assert_eq!(best("mnrs", &files)[0], "src/main.rs");
        assert_eq!(
            best("main", &files)[..2],
            ["src/main.rs", "tests/main_test.rs"]
        );
        assert_eq!(best("md", &files)[0], "README.md");
        assert_eq!(best("ms", &files)[0], "src/model_server.rs");
        assert_eq!(best("app", &files)[0], "src/app.rs");
        assert!(best("zz", &files).is_empty());
        // Folders count: the letters may span the path.
        assert_eq!(best("srcapp", &files)[0], "src/app.rs");
        // Case is ignored, unless the query has a capital.
        assert_eq!(best("readme", &files)[0], "README.md");
        assert!(best("Main", &files).is_empty());
        // The characters that matched, for highlighting.
        assert_eq!(fuzzy("mnrs", "src/main.rs").unwrap().1, vec![4, 7, 9, 10]);
        // Word starts beat letters inside words: model_server's m-s.
        let (start, _) = fuzzy("ms", "src/model_server.rs").unwrap();
        let (inside, _) = fuzzy("ms", "src/items.rs").unwrap();
        assert!(start > inside);
    }

    #[test]
    fn recent_files_come_first_and_a_line_can_follow_the_name() {
        let mut finder = Finder {
            query: String::new(),
            files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            recent: vec!["c.rs".into(), "a.rs".into()],
            shown: Vec::new(),
            selected: 0,
            loading: None,
        };
        finder.refilter();
        let listed: Vec<&str> = finder
            .shown
            .iter()
            .map(|(i, _)| finder.files[*i].as_str())
            .collect();
        assert_eq!(listed, ["c.rs", "a.rs", "b.rs"]);
        finder.query = "brs:42".into();
        finder.refilter();
        assert_eq!(finder.chosen(), Some("b.rs"));
        assert_eq!(finder.target(), ("brs", Some(42)));
        finder.step(-1);
        assert_eq!(finder.selected, 0);
    }
}
