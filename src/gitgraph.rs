//! The Git pane: every branch with its upstream state, a commit graph with one color
//! per lane, and the selected commit's full message and changed files. Enter on a
//! commit shows its whole diff. Git runs on worker threads, like the sidebar's
//! status, so a slow repository never blocks keys.
use crate::workspace::{git, git_limited, printable};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Commits loaded into the graph; older history is a `git log` away.
const LIMIT: usize = 400;
const REFRESH: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// The checked-out branch (or a detached `HEAD`).
    Head,
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub parents: Vec<String>,
    pub refs: Vec<Ref>,
    pub author: String,
    pub time: i64,
    pub subject: String,
}

#[derive(Clone, Debug, Default)]
pub struct Branch {
    pub name: String,
    pub remote: bool,
    pub current: bool,
    pub hash: String,
    pub upstream: String,
    /// `ahead 2, behind 1`, `gone`, or empty when in sync / without upstream.
    pub track: String,
    pub time: i64,
}

/// One graph cell: a box-drawing glyph and the index of its lane's color.
pub type Cell = (char, usize);

#[derive(Default)]
pub struct Snapshot {
    pub branches: Vec<Branch>,
    pub commits: Vec<Commit>,
    pub graph: Vec<Vec<Cell>>,
    pub head: String,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Branches,
    Commits,
}

pub struct GitView {
    root: PathBuf,
    pub data: Snapshot,
    pub focus: Focus,
    pub branch: usize,
    pub commit: usize,
    /// First visible commit / branch row, kept in view of the selection at draw time.
    pub scroll: usize,
    pub branch_scroll: usize,
    /// Visible commit rows, recorded at draw time for paging.
    pub height: usize,
    /// Show only this branch's history instead of every branch.
    pub only: Option<String>,
    pub details: bool,
    /// Full message and changed files of the selected commit: (hash, lines).
    pub detail: Option<(String, Vec<String>)>,
    /// The diff of a commit, shown over the graph (Enter on a commit; Esc closes it).
    pub diff: Option<Diff>,
    receiver: Option<Receiver<Snapshot>>,
    detail_receiver: Option<Receiver<(String, Vec<String>)>>,
    last: Instant,
}

impl GitView {
    pub fn new(root: PathBuf) -> Self {
        let mut view = Self {
            root,
            data: Snapshot::default(),
            focus: Focus::Commits,
            branch: 0,
            commit: 0,
            scroll: 0,
            branch_scroll: 0,
            height: 20,
            only: None,
            details: true,
            detail: None,
            diff: None,
            receiver: None,
            detail_receiver: None,
            last: Instant::now(),
        };
        view.refresh();
        view
    }

    pub fn refresh(&mut self) {
        self.receiver = Some(load(self.root.clone(), self.only.clone()));
        self.last = Instant::now();
    }

    /// Take finished loads and start due ones; true when the view changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(snapshot) => {
                    self.receiver = None;
                    self.replace(snapshot);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.receiver = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if self.last.elapsed() >= REFRESH {
            self.refresh();
        }
        if let Some(diff) = &mut self.diff {
            changed |= diff.poll();
        }
        if let Some(receiver) = &self.detail_receiver {
            match receiver.try_recv() {
                Ok(detail) => {
                    self.detail_receiver = None;
                    self.detail = Some(detail);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.detail_receiver = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.details && self.detail_receiver.is_none() {
            if let Some(commit) = self.selected() {
                if self
                    .detail
                    .as_ref()
                    .is_none_or(|(hash, _)| hash != &commit.hash)
                {
                    self.detail_receiver =
                        Some(load_detail(self.root.clone(), commit.hash.clone()));
                }
            }
        }
        changed
    }

    /// Swap in fresh data, keeping the same commit and branch selected.
    fn replace(&mut self, snapshot: Snapshot) {
        let commit = self.selected().map(|c| c.hash.clone());
        let branch = self.data.branches.get(self.branch).map(|b| b.name.clone());
        self.data = snapshot;
        if let Some(i) = commit.and_then(|h| self.data.commits.iter().position(|c| c.hash == h)) {
            self.commit = i;
        }
        if let Some(i) = branch.and_then(|n| self.data.branches.iter().position(|b| b.name == n)) {
            self.branch = i;
        }
        self.commit = self.commit.min(self.data.commits.len().saturating_sub(1));
        self.branch = self.branch.min(self.data.branches.len().saturating_sub(1));
    }

    pub fn selected(&self) -> Option<&Commit> {
        self.data.commits.get(self.commit)
    }

    pub fn current_branch(&self) -> Option<&Branch> {
        self.data.branches.iter().find(|b| b.current)
    }

    /// Move the focused list by `delta` rows; a branch selection jumps the graph to
    /// that branch's newest commit.
    pub fn step(&mut self, delta: isize) {
        match self.focus {
            Focus::Commits => {
                let last = self.data.commits.len().saturating_sub(1);
                self.commit = self.commit.saturating_add_signed(delta).min(last);
            }
            Focus::Branches => {
                let last = self.data.branches.len().saturating_sub(1);
                self.branch = self.branch.saturating_add_signed(delta).min(last);
                self.jump_to_branch();
            }
        }
    }

    fn jump_to_branch(&mut self) {
        if let Some(branch) = self.data.branches.get(self.branch) {
            if let Some(i) = self.data.commits.iter().position(|c| c.hash == branch.hash) {
                self.commit = i;
            }
        }
    }

    /// Show only the selected branch's history, or every branch again.
    pub fn toggle_only(&mut self) {
        let name = self.data.branches.get(self.branch).map(|b| b.name.clone());
        self.only = if self.only == name { None } else { name };
        self.refresh();
    }

    pub fn show_all(&mut self) {
        if self.only.take().is_some() {
            self.refresh();
        }
    }

    /// Show the selected commit's changes.
    pub fn open_diff(&mut self) {
        if let Some(commit) = self.selected() {
            self.diff = Some(Diff::load(self.root.clone(), commit.hash.clone()));
        }
    }
}

/// How a diff line is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// The commit's author, date and message.
    Message,
    /// A file's line in the summary (`path | 12 ++--`).
    Stat,
    /// A changed file's heading.
    File,
    /// `new file`, `deleted file`, `rename from/to`, mode changes.
    Meta,
    /// `@@ -10,6 +10,8 @@`: where the following lines are.
    Hunk,
    Added,
    Removed,
    Context,
}

/// One line of a commit's diff, with its line numbers in the old and new file.
pub struct DiffLine {
    pub kind: Line,
    pub text: String,
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// A commit's message, changed-file summary and patch, scrolled in the Git pane.
pub struct Diff {
    pub hash: String,
    pub lines: Vec<DiffLine>,
    /// Index of each file heading, for jumping between files.
    pub files: Vec<usize>,
    pub added: usize,
    pub removed: usize,
    pub scroll: usize,
    pub hscroll: usize,
    /// Rows on screen, recorded at draw time for paging.
    pub height: usize,
    pub loading: bool,
    receiver: Option<Receiver<Vec<DiffLine>>>,
    /// Each hunk: its `@@` line, the line after its last, and its file's path.
    pub hunks: Vec<(usize, usize, String)>,
    /// Each hunk's lines in its language's colors (`None`: plain text), by hunk.
    #[allow(clippy::type_complexity)]
    pub colors:
        std::collections::HashMap<usize, Option<Vec<Vec<(syntect::highlighting::Style, String)>>>>,
}

/// Larger diffs (lockfiles, generated code) are cut off rather than slow the pane.
const DIFF_LINES: usize = 20_000;

impl Diff {
    fn load(root: PathBuf, hash: String) -> Self {
        let (sender, receiver) = mpsc::channel();
        let target = hash.clone();
        thread::spawn(move || {
            let text = git_limited(
                &root,
                &[
                    "show",
                    "--no-color",
                    "--no-ext-diff",
                    "-M",
                    "--stat=200",
                    "--patch",
                    "--date=format:%Y-%m-%d %H:%M",
                    "--format=%an <%ae>  %ad%n%n%B",
                    &target,
                ],
                8 * 1024 * 1024,
            );
            let lines = match text {
                Some(bytes) => parse_diff(&String::from_utf8_lossy(&bytes)),
                None => vec![DiffLine {
                    kind: Line::Meta,
                    text: "Could not read this commit".into(),
                    old: None,
                    new: None,
                }],
            };
            let _ = sender.send(lines);
        });
        Self {
            hash,
            lines: Vec::new(),
            files: Vec::new(),
            added: 0,
            removed: 0,
            scroll: 0,
            hscroll: 0,
            height: 20,
            loading: true,
            receiver: Some(receiver),
            hunks: Vec::new(),
            colors: Default::default(),
        }
    }

    fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        match receiver.try_recv() {
            Ok(lines) => {
                self.set(lines);
                true
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                self.loading = false;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn set(&mut self, lines: Vec<DiffLine>) {
        self.receiver = None;
        self.loading = false;
        self.files = (0..lines.len())
            .filter(|&i| lines[i].kind == Line::File)
            .collect();
        self.added = lines.iter().filter(|l| l.kind == Line::Added).count();
        self.removed = lines.iter().filter(|l| l.kind == Line::Removed).count();
        self.hunks = Self::find_hunks(&lines);
        self.colors.clear();
        self.lines = lines;
    }

    /// The hunk line `index` is in, if it's code.
    pub fn hunk_of(&self, index: usize) -> Option<usize> {
        let kind = self.lines.get(index)?.kind;
        if !matches!(kind, Line::Added | Line::Removed | Line::Context) {
            return None;
        }
        let at = self.hunks.partition_point(|(start, ..)| *start < index);
        let h = at.checked_sub(1)?;
        (index < self.hunks[h].1).then_some(h)
    }

    /// Hunk `h`'s lines as the review has them (without the `+`/`-` sign), to color.
    pub fn hunk_lines(&self, h: usize) -> Vec<crate::review::Line> {
        let (start, end, _) = &self.hunks[h];
        self.lines[start + 1..*end]
            .iter()
            .map(|line| crate::review::Line {
                kind: match line.kind {
                    Line::Added => crate::review::Kind::Added,
                    Line::Removed => crate::review::Kind::Removed,
                    _ => crate::review::Kind::Context,
                },
                text: line.text.chars().skip(1).collect(),
                old: line.old,
                new: line.new,
                unterminated: false,
            })
            .collect()
    }

    /// Where each hunk's code lines are, and the file it's in (the new path of a
    /// rename).
    fn find_hunks(lines: &[DiffLine]) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        let mut file = String::new();
        let mut open: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            let code = matches!(line.kind, Line::Added | Line::Removed | Line::Context);
            if let Some(start) = open.filter(|_| !code) {
                out.push((start, i, file.clone()));
                open = None;
            }
            match line.kind {
                Line::File => {
                    file = line
                        .text
                        .rsplit(" → ")
                        .next()
                        .unwrap_or(&line.text)
                        .to_string();
                }
                Line::Hunk => open = Some(i),
                _ => {}
            }
        }
        if let Some(start) = open {
            out.push((start, lines.len(), file));
        }
        out
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let last = self.lines.len().saturating_sub(self.height.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(last);
    }

    /// `n` / `N`: the next / previous file's heading at the top; back from the first
    /// file returns to the message.
    pub fn next_file(&mut self, back: bool) {
        let target = if back {
            Some(
                self.files
                    .iter()
                    .rev()
                    .find(|&&i| i < self.scroll)
                    .copied()
                    .unwrap_or(0),
            )
        } else {
            self.files.iter().find(|&&i| i > self.scroll).copied()
        };
        if let Some(line) = target {
            self.scroll = line;
            self.scroll_by(0);
        }
    }

    /// Which file the top visible line belongs to, and how many there are.
    pub fn position(&self) -> (usize, usize) {
        let current = self.files.iter().filter(|&&i| i <= self.scroll).count();
        (current, self.files.len())
    }
}

/// Split `git show --stat --patch` output into styled lines with line numbers.
fn parse_diff(text: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut in_patch = false;
    let (mut old, mut new) = (0, 0);
    for raw in text.lines() {
        if lines.len() >= DIFF_LINES {
            lines.push(DiffLine {
                kind: Line::Meta,
                text: format!("… diff cut off after {DIFF_LINES} lines"),
                old: None,
                new: None,
            });
            break;
        }
        // Tabs become spaces (Go, Makefiles); other control characters are shown safely.
        let text = printable(&raw.replace('\t', "    "));
        let line = |kind, text: String, old, new| DiffLine {
            kind,
            text,
            old,
            new,
        };
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            in_patch = true;
            // "a/old b/new": show the new path, or "old → new" for a rename.
            let (from, to) = rest
                .split_once(" b/")
                .map(|(a, b)| (a.trim_start_matches("a/"), b))
                .unwrap_or((rest, rest));
            let name = if from == to {
                to.to_string()
            } else {
                format!("{from} → {to}")
            };
            lines.push(line(Line::File, printable(&name), None, None));
            continue;
        }
        if !in_patch {
            // git puts "---" between the message and the file summary.
            if raw == "---" {
                continue;
            }
            let kind = if raw.contains(" | ") && raw.starts_with(' ') {
                Line::Stat
            } else {
                Line::Message
            };
            lines.push(line(kind, text, None, None));
            continue;
        }
        if raw.starts_with("index ") || raw.starts_with("--- ") || raw.starts_with("+++ ") {
            continue;
        }
        if let Some(header) = raw.strip_prefix("@@") {
            // @@ -old,count +new,count @@ context
            let mut numbers = header.split_whitespace();
            let start = |field: Option<&str>| {
                field
                    .and_then(|f| f[1..].split(',').next())
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(1)
            };
            old = start(numbers.next());
            new = start(numbers.next());
            lines.push(line(Line::Hunk, text, None, None));
            continue;
        }
        match raw.chars().next() {
            Some('+') => {
                lines.push(line(Line::Added, text, None, Some(new)));
                new += 1;
            }
            Some('-') => {
                lines.push(line(Line::Removed, text, Some(old), None));
                old += 1;
            }
            Some(' ') => {
                lines.push(line(Line::Context, text, Some(old), Some(new)));
                old += 1;
                new += 1;
            }
            _ => lines.push(line(Line::Meta, text, None, None)),
        }
    }
    lines
}

fn load(root: PathBuf, only: Option<String>) -> Receiver<Snapshot> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(snapshot(&root, only.as_deref()));
    });
    receiver
}

fn snapshot(root: &std::path::Path, only: Option<&str>) -> Snapshot {
    let text = |args: &[&str]| git(root, args).map(|b| String::from_utf8_lossy(&b).into_owned());
    let Some(head) = text(&["rev-parse", "--verify", "--quiet", "HEAD"]) else {
        let error = if text(&["rev-parse", "--git-dir"]).is_some() {
            "No commits yet"
        } else {
            "Not a Git repository"
        };
        return Snapshot {
            error: Some(error.into()),
            ..Snapshot::default()
        };
    };
    let branches = text(&[
        "for-each-ref",
        "--format=%(HEAD)%1f%(refname)%1f%(refname:short)%1f%(objectname)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(committerdate:unix)",
        "refs/heads",
        "refs/remotes",
    ])
    .map(|t| parse_branches(&t))
    .unwrap_or_default();
    let remotes: HashSet<&str> = branches
        .iter()
        .filter(|b| b.remote)
        .map(|b| b.name.as_str())
        .collect();
    let limit = format!("-{LIMIT}");
    let mut args = vec![
        "log",
        "--topo-order",
        "--no-color",
        "--format=%H%x1f%h%x1f%P%x1f%D%x1f%an%x1f%at%x1f%s",
        limit.as_str(),
    ];
    match only {
        Some(name) => args.extend(["--end-of-options", name]),
        None => args.extend(["--branches", "--remotes", "--tags", "HEAD"]),
    }
    let Some(log) = text(&args) else {
        return Snapshot {
            branches,
            head: head.trim().into(),
            error: Some("git log failed or timed out".into()),
            ..Snapshot::default()
        };
    };
    let commits = parse_log(&log, &remotes);
    let graph = layout(&commits);
    Snapshot {
        branches,
        commits,
        graph,
        head: head.trim().into(),
        error: None,
    }
}

fn load_detail(root: PathBuf, hash: String) -> Receiver<(String, Vec<String>)> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let lines = git(
            &root,
            &[
                "show",
                "--no-color",
                "--stat=200",
                "--date=format:%Y-%m-%d %H:%M",
                "--format=%an <%ae>%x1f%ad%n%n%B",
                &hash,
            ],
        )
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .lines()
                // Keep the author/date separator; sanitize each side.
                .map(|l| {
                    l.trim_end()
                        .split('\u{1f}')
                        .map(printable)
                        .collect::<Vec<_>>()
                        .join("\u{1f}")
                })
                .collect()
        })
        .unwrap_or_else(|| vec!["Could not read this commit".into()]);
        let _ = sender.send((hash, lines));
    });
    receiver
}

fn parse_branches(text: &str) -> Vec<Branch> {
    let mut branches: Vec<Branch> = text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\u{1f}').collect();
            let [head, full, name, hash, upstream, track, time] = f[..] else {
                return None;
            };
            // `origin/HEAD` only points at another remote branch.
            if full.starts_with("refs/remotes/") && full.ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                name: printable(name),
                remote: full.starts_with("refs/remotes/"),
                current: head == "*",
                hash: hash.into(),
                upstream: printable(upstream),
                track: printable(track),
                time: time.parse().unwrap_or(0),
            })
        })
        .collect();
    // Yours first: the checked-out branch, then local and remote branches by recency.
    branches.sort_by_key(|b| (!b.current, b.remote, -b.time));
    branches
}

fn parse_log(text: &str, remotes: &HashSet<&str>) -> Vec<Commit> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.splitn(7, '\u{1f}').collect();
            let [hash, short, parents, refs, author, time, subject] = f[..] else {
                return None;
            };
            Some(Commit {
                hash: hash.into(),
                short: short.into(),
                parents: parents.split_whitespace().map(String::from).collect(),
                refs: parse_refs(refs, remotes),
                author: printable(author),
                time: time.parse().unwrap_or(0),
                subject: printable(subject),
            })
        })
        .collect()
}

/// `%D` decorations: "HEAD -> main, origin/main, tag: v1.0".
fn parse_refs(text: &str, remotes: &HashSet<&str>) -> Vec<Ref> {
    text.split(", ")
        .filter(|r| !r.is_empty())
        .map(|r| {
            let (name, kind) = if let Some(branch) = r.strip_prefix("HEAD -> ") {
                (branch, RefKind::Head)
            } else if let Some(tag) = r.strip_prefix("tag: ") {
                (tag, RefKind::Tag)
            } else if r == "HEAD" {
                (r, RefKind::Head)
            } else if remotes.contains(r) {
                (r, RefKind::Remote)
            } else {
                (r, RefKind::Local)
            };
            Ref {
                name: printable(name),
                kind,
            }
        })
        .collect()
}

/// Lay out the commit graph, newest first, two columns per lane: the lane's glyph
/// and a connector column. Each lane keeps one color from where it starts until it
/// ends, so a branch reads as one colored line through the history.
pub fn layout(commits: &[Commit]) -> Vec<Vec<Cell>> {
    // What each lane waits for: (the commit it continues to, its color).
    let mut lanes: Vec<Option<(&str, usize)>> = Vec::new();
    let mut colors = 0..;
    let mut rows = Vec::with_capacity(commits.len());
    for commit in commits {
        let before: Vec<bool> = lanes.iter().map(Option::is_some).collect();
        let waiting: Vec<usize> = (0..lanes.len())
            .filter(|&i| lanes[i].is_some_and(|(h, _)| h == commit.hash))
            .collect();
        let col = match waiting.first() {
            Some(&i) => i,
            None => {
                let color = colors.next().unwrap_or(0);
                place(&mut lanes, (&commit.hash, color), None)
            }
        };
        let color = lanes[col].map_or(0, |(_, c)| c);
        // Other children's lanes end here: they branched off from this commit.
        let joins: Vec<(usize, usize)> = waiting[waiting.len().min(1)..]
            .iter()
            .map(|&i| (i, lanes[i].map_or(0, |(_, c)| c)))
            .collect();
        for &(i, _) in &joins {
            lanes[i] = None;
        }
        lanes[col] = commit.parents.first().map(|p| (p.as_str(), color));
        // Merge parents: join a lane already waiting for them, or start a new one.
        let mut forks = Vec::new();
        for parent in commit.parents.iter().skip(1) {
            match lanes
                .iter()
                .position(|l| l.is_some_and(|(h, _)| h == parent))
            {
                Some(i) => forks.push((i, lanes[i].map_or(0, |(_, c)| c), true)),
                None => {
                    let color = colors.next().unwrap_or(0);
                    let i = place(&mut lanes, (parent, color), Some(col));
                    forks.push((i, color, false));
                }
            }
        }
        let width = lanes.len().max(before.len());
        let mut row = vec![(' ', 0); width * 2];
        for (i, lane) in lanes.iter().enumerate() {
            if i != col && before.get(i).copied().unwrap_or(false) {
                if let Some((_, c)) = lane {
                    row[i * 2] = ('│', *c);
                }
            }
        }
        for &(i, c) in &joins {
            connect(&mut row, col, i, c);
            row[i * 2] = ('╯', c);
        }
        for &(i, c, existing) in &forks {
            connect(&mut row, col, i, c);
            row[i * 2] = match (existing, i > col) {
                (true, true) => ('┤', c),
                (true, false) => ('├', c),
                (false, true) => ('╮', c),
                (false, false) => ('╭', c),
            };
        }
        row[col * 2] = ('●', color);
        while row.len() > 2 && row[row.len() - 2..].iter().all(|(g, _)| *g == ' ') {
            row.truncate(row.len() - 2);
        }
        rows.push(row);
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
    }
    rows
}

/// Put `value` in the first free lane (never `skip`), or a new lane on the right.
fn place<'a>(
    lanes: &mut Vec<Option<(&'a str, usize)>>,
    value: (&'a str, usize),
    skip: Option<usize>,
) -> usize {
    match (0..lanes.len()).find(|&i| lanes[i].is_none() && Some(i) != skip) {
        Some(i) => {
            lanes[i] = Some(value);
            i
        }
        None => {
            lanes.push(Some(value));
            lanes.len() - 1
        }
    }
}

/// Horizontal line between lanes `a` and `b`, crossing any lane in between.
fn connect(row: &mut [Cell], a: usize, b: usize, color: usize) {
    let (from, to) = (a.min(b) * 2 + 1, a.max(b) * 2);
    for cell in &mut row[from..to] {
        cell.0 = if cell.0 == '│' { '┼' } else { '─' };
        if cell.0 == '─' {
            cell.1 = color;
        }
    }
}

/// Compact age of a Unix timestamp: "now", "5m", "3h", "2d", "6w", "4mo", "2y".
pub fn age(time: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    age_at(now, time)
}

fn age_at(now: i64, time: i64) -> String {
    let seconds = (now - time).max(0);
    match seconds {
        s if s < 60 => "now".into(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 14 * 86_400 => format!("{}d", s / 86_400),
        s if s < 60 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_are_split_into_files_hunks_and_numbered_lines() {
        let text = "Ada <ada@example.com>  2026-10-21 10:00\n\n    Tidy imports\n\n src/main.go | 3 ++-\n 1 file changed\n\ndiff --git a/src/main.go b/src/main.go\nindex 1..2 100644\n--- a/src/main.go\n+++ b/src/main.go\n@@ -3,3 +3,4 @@ import (\n \t\"fmt\"\n-\t\"os\"\n+\t\"strings\"\n+\t\"time\"\n )\ndiff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n";
        let lines = parse_diff(text);
        let kinds: Vec<Line> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds[0], Line::Message);
        assert!(kinds.contains(&Line::Stat));
        let file = lines.iter().position(|l| l.kind == Line::File).unwrap();
        assert_eq!(lines[file].text, "src/main.go");
        assert!(
            !lines
                .iter()
                .any(|l| l.text.starts_with("index ") || l.text.starts_with("+++")),
            "plumbing lines are left out"
        );
        let numbered: Vec<(Line, Option<usize>, Option<usize>)> = lines
            .iter()
            .filter(|l| matches!(l.kind, Line::Added | Line::Removed | Line::Context))
            .map(|l| (l.kind, l.old, l.new))
            .collect();
        assert_eq!(
            numbered,
            [
                (Line::Context, Some(3), Some(3)),
                (Line::Removed, Some(4), None),
                (Line::Added, None, Some(4)),
                (Line::Added, None, Some(5)),
                (Line::Context, Some(5), Some(6)),
            ]
        );
        // Tabs are expanded so Go code lines up.
        assert!(lines.iter().any(|l| l.text == "+    \"strings\""));
        let renamed = lines
            .iter()
            .filter(|l| l.kind == Line::File)
            .nth(1)
            .unwrap();
        assert_eq!(renamed.text, "old.txt → new.txt");
        // Jumping between files and counting changes.
        let mut diff = Diff {
            hash: "abc".into(),
            lines: Vec::new(),
            files: Vec::new(),
            added: 0,
            removed: 0,
            scroll: 0,
            hscroll: 0,
            height: 3,
            loading: true,
            receiver: None,
            hunks: Vec::new(),
            colors: Default::default(),
        };
        diff.set(lines);
        assert_eq!((diff.added, diff.removed, diff.files.len()), (2, 1, 2));
        // The hunk's code lines, in Go, for its colors: without their signs.
        assert_eq!(diff.hunks.len(), 1);
        let (start, end, file) = diff.hunks[0].clone();
        assert_eq!((end - start - 1, file.as_str()), (5, "src/main.go"));
        assert_eq!(diff.hunk_of(start), None, "the @@ line isn't code");
        assert_eq!(diff.hunk_of(start + 2), Some(0));
        assert_eq!(diff.hunk_of(end), None);
        let code = diff.hunk_lines(0);
        assert_eq!(code[1].text, "    \"os\"");
        assert_eq!(code[1].kind, crate::review::Kind::Removed);
        diff.next_file(false);
        assert_eq!(diff.scroll, diff.files[0]);
        assert_eq!(diff.position(), (1, 2));
        diff.next_file(true);
        assert_eq!(diff.scroll, 0);
    }

    fn commit(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            ..Commit::default()
        }
    }

    fn draw(commits: &[Commit]) -> Vec<String> {
        layout(commits)
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(g, _)| g)
                    .collect::<String>()
                    .trim_end()
                    .into()
            })
            .collect()
    }

    #[test]
    fn linear_history_is_one_lane() {
        let commits = [commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        assert_eq!(draw(&commits), ["●", "●", "●"]);
        let colors: HashSet<usize> = layout(&commits).iter().map(|r| r[0].1).collect();
        assert_eq!(colors.len(), 1);
    }

    #[test]
    fn a_merged_branch_forks_and_joins_in_its_own_color() {
        // m merges feature (f2, f1) into main (b); both started from a.
        let commits = [
            commit("m", &["b", "f2"]),
            commit("f2", &["f1"]),
            commit("b", &["a"]),
            commit("f1", &["a"]),
            commit("a", &[]),
        ];
        assert_eq!(draw(&commits), ["●─╮", "│ ●", "● │", "│ ●", "●─╯"]);
        let graph = layout(&commits);
        assert_ne!(
            graph[0][0].1, graph[0][2].1,
            "branch lane has its own color"
        );
        assert_eq!(graph[1][2].1, graph[0][2].1, "lane keeps its color");
    }

    #[test]
    fn unmerged_branch_tips_get_their_own_lane() {
        let commits = [
            commit("main2", &["base"]),
            commit("topic", &["base"]),
            commit("base", &[]),
        ];
        assert_eq!(draw(&commits), ["●", "│ ●", "●─╯"]);
    }

    #[test]
    fn crossing_a_lane_draws_a_crossing() {
        let commits = [
            commit("x", &["a"]),
            commit("y", &["b"]),
            commit("m", &["c", "a"]),
        ];
        // m sits in lane 2 and merges toward lane 0, crossing lane 1.
        assert_eq!(draw(&commits)[2], "├─┼─●");
    }

    #[test]
    fn decorations_and_branches_parse() {
        let remotes: HashSet<&str> = ["origin/main"].into();
        let refs = parse_refs("HEAD -> main, origin/main, tag: v1.0, topic", &remotes);
        let kinds: Vec<_> = refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("main", RefKind::Head),
                ("origin/main", RefKind::Remote),
                ("v1.0", RefKind::Tag),
                ("topic", RefKind::Local),
            ]
        );
        let text = [
            " \u{1f}refs/heads/old\u{1f}old\u{1f}1\u{1f}\u{1f}\u{1f}100",
            "*\u{1f}refs/heads/main\u{1f}main\u{1f}2\u{1f}origin/main\u{1f}ahead 2\u{1f}50",
            " \u{1f}refs/remotes/origin/HEAD\u{1f}origin\u{1f}2\u{1f}\u{1f}\u{1f}50",
            " \u{1f}refs/remotes/origin/main\u{1f}origin/main\u{1f}3\u{1f}\u{1f}\u{1f}300",
            " \u{1f}refs/heads/new\u{1f}new\u{1f}4\u{1f}\u{1f}\u{1f}200",
        ]
        .join("\n");
        let names: Vec<_> = parse_branches(&text).into_iter().map(|b| b.name).collect();
        assert_eq!(names, ["main", "new", "old", "origin/main"]);
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(age_at(1_000, 990), "now");
        assert_eq!(age_at(10_000, 10_000 - 7_200), "2h");
        assert_eq!(age_at(100 * 86_400, 97 * 86_400), "3d");
        assert_eq!(age_at(800 * 86_400, 0), "2y");
    }
}
