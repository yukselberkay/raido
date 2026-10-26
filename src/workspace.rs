use anyhow::Result;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub directory: bool,
    pub symlink: bool,
}

pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub expanded: BTreeSet<PathBuf>,
    pub selected: usize,
    pub offset: usize,
}

/// Tab completion for a path typed in Ctrl-p (relative to `root`, or absolute): the
/// entries of the folder typed so far whose names start with the rest, folders
/// first and ending in `/` so the next Tab goes inside. Hidden ones only when the
/// name typed starts with `.`; case is ignored when nothing matches exactly.
pub fn complete_path(root: &Path, typed: &str) -> Vec<String> {
    let (folder, prefix) = match typed.rfind('/') {
        Some(at) => typed.split_at(at + 1),
        None => ("", typed),
    };
    let Ok(read) = fs::read_dir(root.join(folder)) else {
        return Vec::new();
    };
    let mut found: Vec<(bool, String)> = read
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            (prefix.starts_with('.') || !name.starts_with('.'))
                .then(|| (entry.path().is_dir(), name))
        })
        .collect();
    let exact = found.iter().any(|(_, n)| n.starts_with(prefix));
    let lower = prefix.to_lowercase();
    found.retain(|(_, name)| {
        if exact {
            name.starts_with(prefix)
        } else {
            name.to_lowercase().starts_with(&lower)
        }
    });
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .map(|(dir, name)| format!("{folder}{name}{}", if dir { "/" } else { "" }))
        .collect()
}

pub fn printable(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

impl FileTree {
    pub fn new(root: PathBuf) -> Result<Self> {
        let mut tree = Self {
            root,
            entries: Vec::new(),
            expanded: BTreeSet::new(),
            selected: 0,
            offset: 0,
        };
        tree.refresh()?;
        Ok(tree)
    }
    pub fn refresh(&mut self) -> Result<()> {
        let selected = self.entries.get(self.selected).map(|e| e.path.clone());
        let mut entries = Vec::new();
        self.walk(&self.root, 0, &mut entries)?;
        self.entries = entries;
        if let Some(path) = selected {
            if let Some(i) = self.entries.iter().position(|e| e.path == path) {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        Ok(())
    }
    fn walk(&self, directory: &Path, depth: usize, out: &mut Vec<Entry>) -> Result<()> {
        if depth > 32 || out.len() >= 10_000 {
            return Ok(());
        }
        let mut children = fs::read_dir(directory)?
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Dotfiles are always shown; only build output stays out of the way.
                if name == "target" || name == "node_modules" {
                    return None;
                }
                let kind = entry.file_type().ok()?;
                Some(Entry {
                    path: entry.path(),
                    depth,
                    directory: kind.is_dir(),
                    symlink: kind.is_symlink(),
                })
            })
            .collect::<Vec<_>>();
        children.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
        });
        for entry in children {
            if out.len() >= 10_000 {
                break;
            }
            let recurse = entry.directory && self.expanded.contains(&entry.path);
            let path = entry.path.clone();
            out.push(entry);
            if recurse {
                let _ = self.walk(&path, depth + 1, out);
            }
        }
        Ok(())
    }
    pub fn down(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.entries.len().saturating_sub(1));
    }
    pub fn activate(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(None);
        };
        if entry.directory {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.refresh()?;
            Ok(None)
        } else {
            Ok(Some(entry.path))
        }
    }
    pub fn right(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(None);
        };
        if entry.directory {
            if self.expanded.insert(entry.path) {
                self.refresh()?;
            } else {
                self.down(1);
            }
            Ok(None)
        } else {
            Ok(Some(entry.path))
        }
    }
    pub fn left(&mut self) -> Result<()> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(());
        };
        if self.expanded.remove(&entry.path) {
            self.refresh()?;
        } else if let Some(parent) = entry.path.parent() {
            if let Some(i) = self.entries.iter().position(|e| e.path == parent) {
                self.selected = i;
            }
        }
        Ok(())
    }
    /// Fold every folder, keeping the selection on the top-level entry it was under.
    pub fn collapse_all(&mut self) -> Result<()> {
        let selected = self.entries.get(self.selected).map(|e| e.path.clone());
        self.expanded.clear();
        self.refresh()?;
        let top = selected.as_deref().and_then(|path| {
            path.ancestors()
                .find(|a| a.parent() == Some(self.root.as_path()))
                .map(Path::to_path_buf)
        });
        if let Some(i) = top.and_then(|top| self.entries.iter().position(|e| e.path == top)) {
            self.selected = i;
        }
        Ok(())
    }
    /// Unfold the selected folder and every folder inside it (for a file, the folder
    /// holding it). `.git` and build output stay folded.
    pub fn expand_all(&mut self) -> Result<()> {
        let folder = match self.entries.get(self.selected) {
            Some(entry) if entry.directory => entry.path.clone(),
            Some(entry) => entry
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        };
        let mut pending = vec![(folder, 0)];
        let mut opened = 0;
        while let Some((dir, depth)) = pending.pop() {
            if dir != self.root {
                self.expanded.insert(dir.clone());
            }
            opened += 1;
            if depth >= 32 || opened >= 2_000 {
                continue;
            }
            let Ok(children) = fs::read_dir(&dir) else {
                continue;
            };
            for child in children.filter_map(|e| e.ok()) {
                let name = child.file_name();
                let skip = matches!(name.to_str(), Some(".git" | "target" | "node_modules"));
                if !skip && child.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push((child.path(), depth + 1));
                }
            }
        }
        self.refresh()
    }
    /// Space: fold or unfold the selected folder, or the folder holding a file.
    pub fn toggle(&mut self) -> Result<()> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(());
        };
        if entry.directory {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            return self.refresh();
        }
        if let Some(parent) = entry.path.parent() {
            if let Some(i) = self.entries.iter().position(|e| e.path == parent) {
                self.selected = i;
                self.expanded.remove(parent);
                self.refresh()?;
            }
        }
        Ok(())
    }
    pub fn reveal(&mut self, path: &Path) {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(&self.root) || dir == self.root {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            parent = dir.parent();
        }
        let _ = self.refresh();
        if let Some(i) = self.entries.iter().position(|e| e.path == path) {
            self.selected = i;
        }
    }
}

#[derive(Clone, Default)]
pub struct GitInfo {
    pub branch: String,
    pub changes: Vec<(String, String)>,
    pub graph: Vec<String>,
    pub error: Option<String>,
    /// Explorer decorations: changed files, and folders holding changes.
    marks: HashMap<String, Mark>,
    /// Whole untracked or ignored folders; everything inside shares the mark.
    folders: HashMap<String, Mark>,
}

/// How Git sees a path, as the explorer shows it (VS Code's letters). Ordered so a
/// folder takes the most urgent mark among the files it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Ignored,
    Untracked,
    Added,
    Renamed,
    Modified,
    Deleted,
    Conflict,
}

impl Mark {
    /// Map a `git status --porcelain` XY code.
    fn parse(code: &str) -> Option<Self> {
        let mut chars = code.chars();
        let (x, y) = (chars.next()?, chars.next()?);
        Some(match (x, y) {
            ('?', '?') => Self::Untracked,
            ('!', '!') => Self::Ignored,
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => Self::Conflict,
            (_, 'D') | ('D', _) => Self::Deleted,
            ('A', _) => Self::Added,
            ('R', _) | ('C', _) => Self::Renamed,
            ('M' | 'T', _) | (_, 'M' | 'T') => Self::Modified,
            _ => return None,
        })
    }
    pub fn letter(self) -> &'static str {
        match self {
            Self::Ignored => "",
            Self::Untracked => "U",
            Self::Added => "A",
            Self::Renamed => "R",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Conflict => "!",
        }
    }
}

impl GitInfo {
    fn record(&mut self, code: &str, path: &str) {
        let Some(mark) = Mark::parse(code) else {
            return;
        };
        let folder = path.ends_with('/');
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return;
        }
        // Ignored paths only dim themselves; `?? dir/` covers everything inside.
        if mark == Mark::Ignored || folder {
            self.folders.insert(path.to_owned(), mark);
        } else {
            self.raise(path, mark);
        }
        if mark == Mark::Ignored {
            return;
        }
        let mut parent = path;
        while let Some((up, _)) = parent.rsplit_once('/') {
            self.raise(up, mark);
            parent = up;
        }
    }
    fn raise(&mut self, path: &str, mark: Mark) {
        let slot = self.marks.entry(path.to_owned()).or_insert(mark);
        *slot = (*slot).max(mark);
    }
    /// The explorer mark for a path relative to the project root (`/`-separated).
    pub fn mark(&self, relative: &str) -> Option<Mark> {
        if let Some(mark) = self.marks.get(relative) {
            return Some(*mark);
        }
        if relative == ".git" || relative.starts_with(".git/") {
            return Some(Mark::Ignored);
        }
        let mut path = relative;
        loop {
            if let Some(mark) = self.folders.get(path) {
                return Some(*mark);
            }
            path = path.rsplit_once('/')?.0;
        }
    }
}

// Git runs on a worker, with no shell and no repository-provided fsmonitor.
pub(crate) fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    git_limited(root, args, 512 * 1024)
}

/// `git` keeping up to `limit` bytes of output.
pub(crate) fn git_limited(root: &Path, args: &[&str], limit: usize) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false", "-c", "core.pager=cat", "-C"])
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = stdout.take(limit as u64).read_to_end(&mut bytes);
        bytes
    });
    let start = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader.join().ok()?;
    success.then_some(bytes)
}

pub fn refresh_git(root: PathBuf) -> Receiver<GitInfo> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let Some(branch) = git(&root, &["symbolic-ref", "--short", "HEAD"])
            .or_else(|| git(&root, &["rev-parse", "--short", "HEAD"]))
        else {
            let _ = sender.send(GitInfo {
                error: Some("Not a Git repository".into()),
                ..GitInfo::default()
            });
            return;
        };
        let mut info = GitInfo {
            branch: printable(String::from_utf8_lossy(&branch).trim()),
            ..GitInfo::default()
        };
        let prefix = git(&root, &["rev-parse", "--show-prefix"])
            .map(|s| {
                String::from_utf8_lossy(&s)
                    .trim_end_matches('\n')
                    .to_owned()
            })
            .unwrap_or_default();
        if let Some(status) = git(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignored=matching",
                "--",
                ".",
            ],
        ) {
            let mut entries = status.split(|b| *b == 0).filter(|s| !s.is_empty());
            while let Some(entry) = entries.next() {
                if entry.len() < 3 {
                    continue;
                }
                let code = String::from_utf8_lossy(&entry[..2]).into_owned();
                let original = String::from_utf8_lossy(&entry[3..]);
                let path = original
                    .strip_prefix(&prefix)
                    .unwrap_or(&original)
                    .to_owned();
                if code.contains('R') || code.contains('C') {
                    entries.next();
                }
                info.record(&code, &path);
                if code != "!!" {
                    info.changes.push((code, path));
                }
            }
        } else {
            info.error = Some("Git status timed out or failed".into());
        }
        if let Some(graph) = git(
            &root,
            &[
                "log",
                "--graph",
                "--oneline",
                "--decorate=short",
                "--no-color",
                "-8",
            ],
        ) {
            info.graph = String::from_utf8_lossy(&graph)
                .lines()
                .map(printable)
                .collect();
        }
        let _ = sender.send(info);
    });
    receiver
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn tree_is_sorted_lazy_and_does_not_follow_symlink_loops() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        fs::write(dir.path().join(".env"), "").unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        let mut tree = FileTree::new(dir.path().into()).unwrap();
        // Dotfiles and dot-directories always show; build output does not.
        let names: Vec<_> = tree
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&".env".into()) && names.contains(&".git".into()));
        assert!(!names.contains(&"target".into()));
        fs::remove_file(dir.path().join(".env")).unwrap();
        fs::remove_dir(dir.path().join(".git")).unwrap();
        tree.refresh().unwrap();
        assert_eq!(tree.entries.len(), 3);
        assert!(tree.entries[0].directory);
        tree.activate().unwrap();
        assert_eq!(tree.entries.len(), 4);
        assert!(tree.entries.iter().any(|e| e.symlink && !e.directory));
    }

    #[test]
    fn explorer_marks_follow_git_status_like_vs_code() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args([
                    "-c",
                    "user.name=Raido",
                    "-c",
                    "user.email=raido@example.com",
                ])
                .arg("-C")
                .arg(root)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/deep/kept.rs"), "a").unwrap();
        fs::write(root.join("src/clean.rs"), "a").unwrap();
        fs::write(root.join("gone.txt"), "a").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        fs::write(root.join("src/deep/kept.rs"), "b").unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::create_dir_all(root.join("fresh/inner")).unwrap();
        fs::write(root.join("fresh/inner/new.rs"), "").unwrap();
        fs::write(root.join("staged.rs"), "").unwrap();
        run(&["add", "staged.rs"]);
        fs::write(root.join("debug.log"), "").unwrap();
        let info = refresh_git(root.to_path_buf())
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(info.mark("src/deep/kept.rs"), Some(Mark::Modified));
        assert_eq!(info.mark("src/deep"), Some(Mark::Modified));
        assert_eq!(info.mark("src"), Some(Mark::Modified));
        assert_eq!(info.mark("src/clean.rs"), None);
        assert_eq!(info.mark("staged.rs"), Some(Mark::Added));
        assert_eq!(info.mark("fresh"), Some(Mark::Untracked));
        assert_eq!(info.mark("fresh/inner/new.rs"), Some(Mark::Untracked));
        assert_eq!(info.mark("debug.log"), Some(Mark::Ignored));
        assert_eq!(info.mark(".git/HEAD"), Some(Mark::Ignored));
        assert_eq!(info.mark(".gitignore"), None);
        // Ignored files stay out of the change list; the deleted file stays in it.
        assert!(info.changes.iter().all(|(_, p)| p != "debug.log"));
        assert!(info
            .changes
            .iter()
            .any(|(c, p)| p == "gone.txt" && c.contains('D')));
    }

    #[test]
    fn folders_collapse_and_expand_all_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/a/b")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/a/b/deep.rs"), "").unwrap();
        fs::write(root.join("docs/guide.md"), "").unwrap();
        let mut tree = FileTree::new(root.into()).unwrap();
        let at = |tree: &FileTree, name: &str| {
            tree.entries
                .iter()
                .position(|e| e.path.ends_with(name))
                .unwrap()
        };
        // E on a folder opens everything inside it, and nothing else.
        tree.selected = at(&tree, "src");
        tree.expand_all().unwrap();
        assert!(tree.entries.iter().any(|e| e.path.ends_with("deep.rs")));
        assert!(!tree.expanded.contains(&root.join("docs")));
        // E on a top-level file opens the whole project, except .git.
        tree.selected = at(&tree, "docs");
        tree.right().unwrap();
        tree.selected = at(&tree, "guide.md");
        tree.toggle().unwrap();
        assert_eq!(
            tree.selected,
            at(&tree, "docs"),
            "space folds the file's folder"
        );
        assert!(!tree.expanded.contains(&root.join("docs")));
        tree.toggle().unwrap();
        assert!(tree.expanded.contains(&root.join("docs")));
        // c folds everything and keeps you on the top-level folder you were in.
        tree.selected = at(&tree, "deep.rs");
        tree.collapse_all().unwrap();
        assert!(tree.expanded.is_empty());
        assert_eq!(tree.entries.len(), 3);
        assert!(tree.entries[tree.selected].path.ends_with("src"));
        fs::write(root.join("top.txt"), "").unwrap();
        tree.refresh().unwrap();
        tree.selected = at(&tree, "top.txt");
        tree.expand_all().unwrap();
        assert!(tree.expanded.contains(&root.join("src/a/b")));
        assert!(tree.expanded.contains(&root.join("docs")));
        assert!(!tree.expanded.contains(&root.join(".git")));
    }

    #[test]
    fn folders_take_their_most_urgent_change() {
        let mut info = GitInfo::default();
        info.record("??", "a/b/new.rs");
        info.record(" M", "a/edit.rs");
        info.record("UU", "a/b/c/both.rs");
        info.record(" D", "z/old.rs");
        info.record("XY", "unknown.rs");
        assert_eq!(info.mark("a/b/new.rs"), Some(Mark::Untracked));
        assert_eq!(info.mark("a/b"), Some(Mark::Conflict));
        assert_eq!(info.mark("a"), Some(Mark::Conflict));
        assert_eq!(info.mark("z"), Some(Mark::Deleted));
        assert_eq!(info.mark("unknown.rs"), None);
        assert_eq!(Mark::parse("AM"), Some(Mark::Added));
        assert_eq!(Mark::parse("R "), Some(Mark::Renamed));
        assert_eq!(Mark::parse("MM"), Some(Mark::Modified));
        assert_eq!(Mark::Conflict.letter(), "!");
    }
}
