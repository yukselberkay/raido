//! Find in files (Ctrl-f in the explorer): files whose names contain the text,
//! then every line that does, searched in the background so a big project never
//! blocks typing. Git's file list keeps ignored files out when there is one.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
};

/// Results stop here, so a one-letter search stays quick.
const MAX_HITS: usize = 2_000;
/// Bigger files are skipped (logs, data, bundles).
const MAX_FILE: u64 = 2 * 1024 * 1024;

/// One result: a file whose name matches (`line` is None) or a line in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Relative to the project root.
    pub path: PathBuf,
    pub line: Option<usize>,
    /// Character column of the match in the line.
    pub col: usize,
    /// The line, trimmed, for the list.
    pub text: String,
}

pub struct Search {
    pub query: String,
    pub hits: Vec<Hit>,
    pub selected: usize,
    /// First row shown in the list.
    pub offset: usize,
    pub running: bool,
    /// More matched than [`MAX_HITS`].
    pub truncated: bool,
    receiver: Option<Receiver<(Vec<Hit>, bool)>>,
    cancel: Arc<AtomicBool>,
}

impl Search {
    pub fn new(query: String) -> Self {
        Self {
            query,
            hits: Vec::new(),
            selected: 0,
            offset: 0,
            running: false,
            truncated: false,
            receiver: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Search for the current query from scratch, dropping a search still running.
    pub fn start(&mut self, root: &Path) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.hits.clear();
        self.selected = 0;
        self.offset = 0;
        self.truncated = false;
        if self.query.is_empty() {
            self.running = false;
            self.receiver = None;
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let (root, query, cancel) = (root.to_path_buf(), self.query.clone(), self.cancel.clone());
        thread::spawn(move || run(&root, &query, &cancel, &sender));
        self.receiver = Some(receiver);
        self.running = true;
    }
    /// Take in results found since the last call; true when something changed.
    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        let mut changed = false;
        loop {
            match receiver.try_recv() {
                Ok((hits, done)) => {
                    self.hits.extend(hits);
                    changed = true;
                    if done {
                        self.truncated = self.hits.len() >= MAX_HITS;
                        self.running = false;
                        self.receiver = None;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.running = false;
                    self.receiver = None;
                    changed = true;
                    break;
                }
            }
        }
        changed
    }
    /// How many files the results are in.
    pub fn files(&self) -> usize {
        let paths: std::collections::BTreeSet<&PathBuf> =
            self.hits.iter().map(|h| &h.path).collect();
        paths.len()
    }
}

/// The project's files: Git's list (tracked and untracked, not ignored), else a walk
/// that skips `.git` and build output.
fn files(root: &Path) -> Vec<PathBuf> {
    let listed = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success());
    if let Some(output) = listed {
        let mut paths: Vec<PathBuf> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
            .collect();
        paths.sort();
        paths.dedup();
        return paths;
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() > 100_000 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | "target" | "node_modules" | ".raido")
        ) {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(root, &path, out),
            Ok(kind) if kind.is_file() => {
                if let Ok(relative) = path.strip_prefix(root) {
                    out.push(relative.to_path_buf());
                }
            }
            _ => {}
        }
    }
}

fn run(root: &Path, query: &str, cancel: &AtomicBool, sender: &Sender<(Vec<Hit>, bool)>) {
    let ignore_case = !query.chars().any(char::is_uppercase);
    let found = |text: &str| crate::editor::find_all(text, query, ignore_case, false);
    let files = files(root);
    let mut total = 0;
    // Names first.
    let names: Vec<Hit> = files
        .iter()
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            !found(name).is_empty()
        })
        .take(MAX_HITS)
        .map(|path| Hit {
            path: path.clone(),
            line: None,
            col: 0,
            text: String::new(),
        })
        .collect();
    total += names.len();
    if sender.send((names, false)).is_err() {
        return;
    }
    let mut batch = Vec::new();
    for path in &files {
        if cancel.load(Ordering::Relaxed) || total >= MAX_HITS {
            break;
        }
        let full = root.join(path);
        if fs::metadata(&full).map_or(true, |m| !m.is_file() || m.len() > MAX_FILE) {
            continue;
        }
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        // Binary files have NUL bytes early on.
        if bytes.iter().take(8_000).any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (number, line) in text.lines().enumerate() {
            let Some(&(at, _)) = found(line).first() else {
                continue;
            };
            let shown: String = line.trim().chars().take(200).collect();
            batch.push(Hit {
                path: path.clone(),
                line: Some(number),
                col: line[..at].chars().count(),
                text: shown,
            });
            total += 1;
            if total >= MAX_HITS {
                break;
            }
        }
        if batch.len() >= 200 && sender.send((std::mem::take(&mut batch), false)).is_err() {
            return;
        }
    }
    if !cancel.load(Ordering::Relaxed) {
        let _ = sender.send((batch, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn finish(search: &mut Search) {
        let start = Instant::now();
        while search.running && start.elapsed() < Duration::from_secs(10) {
            search.poll();
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn names_then_lines_with_smart_case_skipping_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/x")).unwrap();
        fs::write(
            root.join("src/counter.rs"),
            "fn main() {\n    let Count = 1;\n}\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "count the ducks\nnothing here\n").unwrap();
        fs::write(root.join("blob.bin"), b"count\0\0\0").unwrap();
        fs::write(root.join("node_modules/x/count.js"), "count").unwrap();
        let mut search = Search::new("count".into());
        search.start(root);
        finish(&mut search);
        let shown: Vec<(String, Option<usize>, usize)> = search
            .hits
            .iter()
            .map(|h| (h.path.to_string_lossy().replace('\\', "/"), h.line, h.col))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("src/counter.rs".into(), None, 0),
                ("notes.txt".into(), Some(0), 0),
                ("src/counter.rs".into(), Some(1), 8),
            ]
        );
        assert_eq!(search.hits[2].text, "let Count = 1;");
        assert_eq!(search.files(), 2);
        // A capital letter matches case.
        search.query = "Count".into();
        search.start(root);
        finish(&mut search);
        assert_eq!(search.hits.len(), 1);
        assert_eq!(search.hits[0].line, Some(1));
        // Nothing typed, nothing searched.
        search.query.clear();
        search.start(root);
        assert!(!search.running && search.hits.is_empty());
    }
}
