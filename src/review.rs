//! Review what AI agents change, like VS Code's chat edits. While AI panes are open,
//! Raido snapshots the project with Git into its own index and object store (your
//! repository, index and stash are never written): every couple of seconds while
//! an agent works, every few seconds while they wait. A file that changes between
//! two snapshots while an agent worked is remembered as it was before, so the review
//! shows everything the agents changed since you last looked, and each change can be
//! kept or undone: one hunk, one file or all. Files that change while no agent
//! works, or that you save from Raido's editor, are yours and stay out of it.
use similar::{capture_diff_slices_deadline, group_diff_ops, Algorithm};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

/// How often to snapshot while an agent works.
const BUSY: Duration = Duration::from_millis(1500);
/// How often to look again while the review is on screen and nothing works, so your
/// own edits to a reviewed file show up there too.
const IDLE: Duration = Duration::from_secs(3);
/// How often to snapshot while AI panes wait, so what you change meanwhile isn't
/// taken for theirs when one starts (an approved edit lands at once).
const WAITING: Duration = Duration::from_secs(5);
/// Larger files are kept or undone whole, without a line diff.
const MAX_TEXT: usize = 2 * 1024 * 1024;
/// Unchanged lines shown around each change.
const CONTEXT: usize = 3;
/// A snapshot of a large project gets this long.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Stores left behind by a Raido that didn't quit cleanly are cleared after this.
const STALE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Git, pointed at the project but with Raido's own index and object store. Objects
/// the repository already has are read from it (an alternate), never written to it.
struct Store {
    /// The repository's top level: Git's paths are relative to it.
    top: PathBuf,
    /// Raido's project folder, `top` or inside it.
    root: PathBuf,
    dir: PathBuf,
    /// The repository's own object store.
    objects: PathBuf,
    /// The repository's index, copied as a head start (Git reuses its file stats).
    index: PathBuf,
}

impl Store {
    fn open(root: &Path, dir: &Path) -> Result<Self, String> {
        let output = run(
            Command::new("git").current_dir(root).args([
                "rev-parse",
                "--show-toplevel",
                "--git-path",
                "objects",
                "--git-path",
                "index",
            ]),
            None,
        )
        .map_err(|error| {
            if error.to_lowercase().contains("not a git repository") {
                "not a Git repository".to_string()
            } else {
                error
            }
        })?;
        let text = String::from_utf8_lossy(&output);
        let mut lines = text.lines();
        let mut next = || -> Result<PathBuf, String> {
            let line = lines.next().ok_or("unexpected answer from git")?;
            Ok(root.join(line))
        };
        let top = next()?;
        let objects = next()?;
        let index = next()?;
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        clear_stale(dir);
        fs::create_dir_all(dir.join("objects")).map_err(|e| e.to_string())?;
        let store = Self {
            top: canonical(&top),
            root: canonical(root),
            dir: dir.to_path_buf(),
            objects: canonical(&objects),
            index,
        };
        store.seed();
        Ok(store)
    }

    fn seed(&self) {
        let index = self.dir.join("index");
        if !index.exists() {
            let _ = fs::copy(&self.index, index);
        }
    }

    fn git(&self, cwd: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        run(
            Command::new("git")
                .current_dir(cwd)
                .args(["-c", "core.fsmonitor=false"])
                .args(args)
                .env("GIT_INDEX_FILE", self.dir.join("index"))
                .env("GIT_OBJECT_DIRECTORY", self.dir.join("objects"))
                .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.objects),
            input,
        )
    }

    /// Every file of the project as it is now (`.gitignore` applies), as a tree.
    fn snapshot(&self) -> Result<String, String> {
        let take = || {
            // Only Raido uses this index, one snapshot at a time: a lock is left over
            // from a git that took too long and was stopped.
            let _ = fs::remove_file(self.dir.join("index.lock"));
            // A file that can't be read is left as it was; the rest still counts.
            let _ = self.git(
                &self.root,
                &[
                    "add",
                    "-A",
                    "--ignore-errors",
                    "--",
                    ".",
                    ":(exclude).raido",
                ],
                None,
            );
            self.git(&self.top, &["write-tree"], None)
        };
        let tree = take().or_else(|_| {
            // The repository dropped objects our index still names (gc): start over.
            let _ = fs::remove_file(self.dir.join("index"));
            self.seed();
            take()
        })?;
        Ok(String::from_utf8_lossy(&tree).trim().to_string())
    }

    /// Files that differ between two snapshots: (path, blob before, executable).
    /// Links and submodules are left alone.
    fn changes(&self, from: &str, to: &str) -> Result<Vec<(String, Option<String>, bool)>, String> {
        let out = self.git(
            &self.top,
            &["diff-tree", "-r", "-z", "--no-renames", from, to],
            None,
        )?;
        let text = String::from_utf8_lossy(&out);
        let mut fields = text.split('\0');
        let mut changes = Vec::new();
        while let (Some(header), Some(path)) = (fields.next(), fields.next()) {
            // ":100644 100644 <old> <new> M"
            let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
            let [old_mode, new_mode, old, ..] = parts[..] else {
                continue;
            };
            let file = |mode: &str| mode == "000000" || mode.starts_with("100");
            if !file(old_mode) || !file(new_mode) {
                continue;
            }
            let blob = (old_mode != "000000").then(|| old.to_string());
            changes.push((path.to_string(), blob, old_mode == "100755"));
        }
        Ok(changes)
    }

    /// Blobs as they'd be checked out (line endings and filters applied).
    fn blobs(&self, wanted: &[(&str, &str)]) -> Result<Vec<Vec<u8>>, String> {
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = wanted
            .iter()
            .map(|(blob, path)| format!("{blob} {path}\n"))
            .collect();
        let out = self.git(
            &self.top,
            &["cat-file", "--batch", "--filters"],
            Some(input.as_bytes()),
        )?;
        split_batch(&out, wanted)
    }

    fn blob(&self, blob: &str, path: &str) -> Result<Vec<u8>, String> {
        self.blobs(&[(blob, path)])
            .map(|mut b| b.pop().unwrap_or_default())
    }

    /// Store `bytes` as the reviewed version of `path`.
    fn write(&self, bytes: &[u8], path: &str) -> Result<String, String> {
        let path = format!("--path={path}");
        let out = self.git(
            &self.top,
            &["hash-object", "-w", "--stdin", &path],
            Some(bytes),
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}

/// The blobs in `git cat-file --batch --filters` output, in `wanted` order. Git's
/// header gives a blob's size before the filters, which differs when they change it
/// (`core.autocrlf`, Git for Windows' default, adds a CR to every line), so sizes
/// can't be trusted: each blob ends where the next one's header starts, the last one
/// at the end.
fn split_batch(out: &[u8], wanted: &[(&str, &str)]) -> Result<Vec<Vec<u8>>, String> {
    let short = || "short answer from git cat-file".to_string();
    let mut blobs = Vec::new();
    let mut rest = out;
    for (i, (blob, _)) in wanted.iter().enumerate() {
        let end = rest.iter().position(|&b| b == b'\n').ok_or_else(short)?;
        let header = String::from_utf8_lossy(&rest[..end]);
        if !header.starts_with(&format!("{blob} blob ")) {
            return Err(format!("git cat-file: {header}"));
        }
        let body = &rest[end + 1..];
        // Every blob is followed by a newline of git's.
        let len = match wanted.get(i + 1) {
            Some((next, _)) => {
                let next = format!("\n{next} blob ");
                body.windows(next.len())
                    .position(|w| w == next.as_bytes())
                    .ok_or_else(short)?
            }
            None => body.len().checked_sub(1).ok_or_else(short)?,
        };
        blobs.push(body[..len].to_vec());
        rest = &body[len + 1..];
    }
    Ok(blobs)
}

/// Run `command`, feeding it `input`, and return what it printed.
fn run(command: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    run_for(command, input, TIMEOUT, "git")
}

/// Run `command` (`what`, in errors) with `input` and give its output, stopping it
/// after `timeout`.
pub(crate) fn run_for(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: {e}"))?;
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let mut stdout = child.stdout.take().ok_or(format!("{what}: no output"))?;
    let mut stderr = child.stderr.take().ok_or(format!("{what}: no output"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let errors = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} took too long"));
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    let errors = errors.join().unwrap_or_default();
    if status.success() {
        Ok(bytes)
    } else {
        let line = errors
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        Err(format!("{what}: {}", line.trim()))
    }
}

/// Remove stores of other Raidos that are long gone.
fn clear_stale(dir: &Path) {
    let Some(parent) = dir.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > STALE);
        if old && entry.path() != dir {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// What a file was before the agents changed it.
#[derive(Clone, Debug)]
struct Base {
    /// `None`: the file didn't exist.
    blob: Option<String>,
    executable: bool,
    /// The AI panes that were working when it changed.
    by: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Context,
    Removed,
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    /// Without its line ending.
    pub text: String,
    /// 1-based line numbers in the version before and the one now.
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The file ends on this line without a newline.
    pub unterminated: bool,
}

/// Where a hunk sits, as 0-based line ranges before and now; it names the hunk when
/// you keep or undo it, so a hunk that moved since it was drawn isn't touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub old: (usize, usize),
    pub new: (usize, usize),
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub span: Span,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug)]
pub enum Body {
    Text(Vec<Hunk>),
    /// Not text, or too large to compare line by line.
    Binary,
}

/// One changed file, ready to show.
#[derive(Clone, Debug)]
pub struct FileDiff {
    /// Git's path, relative to the repository's top level.
    pub path: String,
    /// The path shown: relative to Raido's project folder.
    pub name: String,
    pub absolute: PathBuf,
    pub status: Status,
    pub by: Vec<String>,
    pub body: Body,
    pub added: usize,
    pub removed: usize,
    /// What the change did, read from the code (see `outline`).
    pub outline: crate::outline::Outline,
}

impl FileDiff {
    pub fn hunks(&self) -> &[Hunk] {
        match &self.body {
            Body::Text(hunks) => hunks,
            Body::Binary => &[],
        }
    }
}

fn is_text(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_TEXT
        && !bytes[..bytes.len().min(8000)].contains(&0)
        && std::str::from_utf8(bytes).is_ok()
}

/// Lines with their endings, so joining them gives the file back byte for byte.
fn lines(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .unwrap_or_default()
        .split_inclusive('\n')
        .collect()
}

/// The hunks between two versions of a text, with `CONTEXT` lines around each.
pub fn hunks(old: &[&str], new: &[&str]) -> Vec<Hunk> {
    let deadline = Instant::now() + Duration::from_millis(300);
    let ops = capture_diff_slices_deadline(Algorithm::Myers, old, new, Some(deadline));
    let mut out = Vec::new();
    for group in group_diff_ops(ops, CONTEXT) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (_, old_first, new_first) = first.as_tag_tuple();
        let (_, old_last, new_last) = last.as_tag_tuple();
        let span = Span {
            old: (old_first.start, old_last.end),
            new: (new_first.start, new_last.end),
        };
        let mut lines = Vec::new();
        let line = |kind, text: &str, old: Option<usize>, new: Option<usize>| Line {
            kind,
            text: text
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string(),
            old: old.map(|i| i + 1),
            new: new.map(|i| i + 1),
            unterminated: !text.ends_with('\n'),
        };
        for op in &group {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            if tag == similar::DiffTag::Equal {
                for (o, n) in old_range.zip(new_range) {
                    lines.push(line(Kind::Context, old[o], Some(o), Some(n)));
                }
                continue;
            }
            for o in old_range {
                lines.push(line(Kind::Removed, old[o], Some(o), None));
            }
            for n in new_range {
                lines.push(line(Kind::Added, new[n], None, Some(n)));
            }
        }
        out.push(Hunk { span, lines });
    }
    out
}

/// Compare a file's reviewed version with what's on disk; `None` when they match.
fn compare(
    store: &Store,
    path: &str,
    base: &Base,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Option<FileDiff> {
    let status = match (old, new) {
        (None, None) => return None,
        (Some(a), Some(b)) if a == b => return None,
        (None, Some(_)) => Status::Added,
        (Some(_), None) => Status::Deleted,
        _ => Status::Modified,
    };
    let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
    let (body, text) = if is_text(old) && is_text(new) {
        (Body::Text(hunks(&lines(old), &lines(new))), true)
    } else {
        (Body::Binary, false)
    };
    let count = |kind| match &body {
        Body::Text(hunks) => hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .count(),
        Body::Binary => 0,
    };
    let absolute = store.top.join(path);
    let name = absolute
        .strip_prefix(&store.root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string());
    let outline = match &body {
        Body::Text(hunks) if text => crate::outline::outline(
            &name,
            std::str::from_utf8(old).unwrap_or_default(),
            std::str::from_utf8(new).unwrap_or_default(),
            hunks,
        ),
        _ => Default::default(),
    };
    Some(FileDiff {
        outline,
        added: count(Kind::Added),
        removed: count(Kind::Removed),
        path: path.to_string(),
        name,
        absolute,
        status,
        by: base.by.iter().cloned().collect(),
        body,
    })
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The diffs of every reviewed file, reading the versions before in one go.
fn diffs(store: &Store, pending: &BTreeMap<String, Base>) -> Result<Vec<FileDiff>, String> {
    let wanted: Vec<(&str, &str)> = pending
        .iter()
        .filter_map(|(path, base)| Some((base.blob.as_deref()?, path.as_str())))
        .collect();
    let mut blobs = store.blobs(&wanted)?.into_iter();
    let mut out = Vec::new();
    for (path, base) in pending {
        let old = base.blob.as_ref().map(|_| blobs.next().unwrap_or_default());
        let new = read(&store.top.join(path))?;
        out.extend(compare(store, path, base, old.as_deref(), new.as_deref()));
    }
    Ok(out)
}

/// What a snapshot found.
struct Outcome {
    store: Arc<Store>,
    tree: String,
    /// Files that changed since the last snapshot (not counting Raido's own writes),
    /// as they were before; whose they are is decided when it's back.
    changed: Vec<(String, Base)>,
    /// The diff of every file under review, the ones above included.
    diffs: Vec<FileDiff>,
}

struct Job {
    store: Option<Arc<Store>>,
    root: PathBuf,
    dir: PathBuf,
    from: Option<String>,
    pending: BTreeMap<String, Base>,
    /// Files Raido wrote itself (saved, undone), by their fingerprint.
    written: HashMap<String, Option<u64>>,
}

impl Job {
    fn run(self) -> Result<Outcome, String> {
        let store = match self.store {
            Some(store) => store,
            None => Arc::new(Store::open(&self.root, &self.dir)?),
        };
        let tree = store.snapshot()?;
        let mut changed = Vec::new();
        if let Some(from) = self.from.filter(|f| *f != tree) {
            for (path, blob, executable) in store.changes(&from, &tree)? {
                if let Some(written) = self.written.get(&path) {
                    let now = read(&store.top.join(&path))?.map(|b| fingerprint(&b));
                    if *written == now {
                        continue;
                    }
                }
                let base = Base {
                    blob,
                    executable,
                    by: BTreeSet::new(),
                };
                changed.push((path, base));
            }
        }
        let mut pending = self.pending;
        for (path, base) in &changed {
            pending.entry(path.clone()).or_insert_with(|| base.clone());
        }
        let diffs = diffs(&store, &pending)?;
        Ok(Outcome {
            store,
            tree,
            changed,
            diffs,
        })
    }
}

/// A file under review as it was before the agents and as it is now (empty when
/// it doesn't exist).
struct Versions {
    store: Arc<Store>,
    base: Base,
    old: Vec<u8>,
    new: Vec<u8>,
}

/// What to keep or undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    All,
    File(String),
    Hunk(String, Span),
}

/// The changes AI agents made that you haven't kept or undone yet.
pub struct Changes {
    root: PathBuf,
    dir: PathBuf,
    store: Option<Arc<Store>>,
    /// Why there's nothing to track here (not a Git repository).
    pub problem: Option<String>,
    /// The last snapshot failed for this reason.
    pub error: Option<String>,
    tree: Option<String>,
    pending: BTreeMap<String, Base>,
    /// What the review shows, in path order.
    pub files: Vec<FileDiff>,
    /// Bumped whenever `files` changes.
    pub version: u64,
    job: Option<Receiver<Result<Outcome, String>>>,
    /// AI panes seen working since the last snapshot began.
    since: BTreeSet<String>,
    /// Those from before the snapshot running now began.
    running: BTreeSet<String>,
    last: Instant,
    /// Look again as soon as possible.
    wanted: bool,
    written: HashMap<String, Option<u64>>,
    /// Files kept or undone while a snapshot ran; what it says about them is stale.
    acted: BTreeSet<String>,
    /// The AI's reading of the changes (`s` in the review), the one on its way and
    /// the agent asked, and why the last one failed.
    pub summary: Option<crate::summary::Summary>,
    summary_job: Option<(Receiver<Result<crate::summary::Summary, String>>, String)>,
    pub summary_error: Option<String>,
    /// What all the explanations asked for since Raido started cost.
    pub spent: crate::summary::Usage,
    /// How many there were.
    pub asked: usize,
}

impl Changes {
    pub fn new(root: PathBuf) -> Self {
        let base = directories::BaseDirs::new()
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base
            .join("raido")
            .join("changes")
            .join(std::process::id().to_string());
        Self::with_store(root, dir)
    }

    fn with_store(root: PathBuf, dir: PathBuf) -> Self {
        Self {
            root,
            dir,
            store: None,
            problem: None,
            error: None,
            tree: None,
            pending: BTreeMap::new(),
            files: Vec::new(),
            version: 0,
            job: None,
            since: BTreeSet::new(),
            running: BTreeSet::new(),
            last: Instant::now(),
            wanted: false,
            written: HashMap::new(),
            acted: BTreeSet::new(),
            summary: None,
            summary_job: None,
            summary_error: None,
            spent: Default::default(),
            asked: 0,
        }
    }

    /// Snapshot regularly while an agent works and once after it stops, now and then
    /// while AI panes wait (`agents`), and while the review is `watching`. `busy`
    /// names the AI panes working now. True when the review changed.
    pub fn tick(&mut self, busy: &[String], agents: bool, watching: bool) -> bool {
        let mut changed = self.summary_arrived();
        if let Some(receiver) = &self.job {
            match receiver.try_recv() {
                Ok(result) => {
                    self.job = None;
                    changed = self.finish(result);
                }
                Err(mpsc::TryRecvError::Disconnected) => self.job = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.problem.is_some() {
            return changed;
        }
        self.since.extend(busy.iter().cloned());
        let every = if !self.since.is_empty() {
            Some(BUSY)
        } else if watching && !self.pending.is_empty() {
            Some(IDLE)
        } else if agents {
            Some(WAITING)
        } else {
            None
        };
        // The first snapshot is the start line: take it as soon as there's a reason.
        let due = every.is_some_and(|every| self.tree.is_none() || self.last.elapsed() >= every);
        if self.job.is_none() && (due || self.wanted) {
            self.start();
        }
        changed
    }

    fn start(&mut self) {
        self.running = std::mem::take(&mut self.since);
        let job = Job {
            store: self.store.clone(),
            root: self.root.clone(),
            dir: self.dir.clone(),
            from: self.tree.clone(),
            pending: self.pending.clone(),
            written: std::mem::take(&mut self.written),
        };
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(job.run());
        });
        self.job = Some(receiver);
        self.last = Instant::now();
        self.wanted = false;
        self.acted.clear();
    }

    fn finish(&mut self, result: Result<Outcome, String>) -> bool {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                // The next snapshot covers this one's changes, by the same agents.
                self.since.extend(std::mem::take(&mut self.running));
                if self.store.is_none() {
                    self.problem = Some(error);
                } else {
                    self.error = Some(error);
                }
                return true;
            }
        };
        self.error = None;
        self.store = Some(outcome.store);
        self.tree = Some(outcome.tree);
        // The agents that worked before this snapshot began or while it ran (one that
        // starts mid-snapshot may already be writing) own what changed; with none,
        // the changes are yours.
        let mut by = std::mem::take(&mut self.running);
        by.extend(self.since.iter().cloned());
        for (path, mut base) in outcome.changed {
            if by.is_empty() || self.acted.contains(&path) {
                continue;
            }
            base.by = by.clone();
            self.pending
                .entry(path)
                .and_modify(|b| b.by.extend(by.iter().cloned()))
                .or_insert(base);
        }
        let mut diffs = outcome.diffs;
        if self.acted.is_empty() {
            // Files that match their version before again leave the review.
            self.pending
                .retain(|path, _| diffs.iter().any(|d| &d.path == path));
        } else {
            self.wanted = true;
            diffs.retain(|d| !self.acted.contains(&d.path));
        }
        for diff in &mut diffs {
            if let Some(base) = self.pending.get(&diff.path) {
                diff.by = base.by.iter().cloned().collect();
            }
        }
        diffs.retain(|d| self.pending.contains_key(&d.path));
        if diffs.len() == self.files.len() && diffs.iter().zip(&self.files).all(|(a, b)| same(a, b))
        {
            return false;
        }
        self.files = diffs;
        self.version += 1;
        true
    }

    /// Ask the AI named by config `review_ai` to explain the changes under review:
    /// only the files it hasn't explained as they are now, unless `afresh`.
    pub fn summarize(&mut self, setting: &[String], afresh: bool) -> Result<String, String> {
        if self.files.is_empty() {
            return Err("No agent changes to summarize".into());
        }
        if let Some((_, by)) = &self.summary_job {
            return Ok(format!("{by} is still reading the changes…"));
        }
        let Some(agent) = crate::summary::Agent::choose(setting)? else {
            return Err(
                "AI summaries are off (review_ai = \"off\" in config.toml); the outline needs no AI"
                    .into(),
            );
        };
        let name = agent.name();
        let before = self.summary.as_ref().filter(|_| !afresh);
        let Some(job) = crate::summary::start(agent, &self.files, before) else {
            return Ok("Nothing new since the last explanation · S explains it all again".into());
        };
        self.summary_error = None;
        self.summary_job = Some((job, name.clone()));
        self.version += 1;
        Ok(format!(
            "Asked {name} to explain the changes · it shows here when done"
        ))
    }

    /// The AI asked to summarize is still reading: its name.
    pub fn summarizing(&self) -> Option<&str> {
        self.summary_job.as_ref().map(|(_, by)| by.as_str())
    }

    /// Whether the summary describes the changes as they are now.
    pub fn summary_current(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|s| s.key == crate::summary::key(&self.files))
    }

    fn summary_arrived(&mut self) -> bool {
        let Some((receiver, by)) = &self.summary_job else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err(format!("{by} stopped")),
        };
        self.summary_job = None;
        match result {
            Ok(mut summary) => {
                self.asked += 1;
                if let Some(usage) = summary.usage {
                    self.spent.add(usage);
                }
                // What it said before about files it wasn't sent stays, for the files
                // still under review.
                if let Some(before) = self.summary.take() {
                    for (path, note) in before.files {
                        if self.files.iter().any(|f| f.path == path) {
                            summary.files.entry(path).or_insert(note);
                        }
                    }
                }
                self.summary = Some(summary);
            }
            Err(error) => self.summary_error = Some(error),
        }
        self.version += 1;
        true
    }

    /// Look for changes now (opening the review).
    pub fn request(&mut self) {
        self.problem = None;
        self.wanted = true;
    }

    /// Nothing known yet: the first look is still running.
    pub fn loading(&self) -> bool {
        self.tree.is_none() && (self.job.is_some() || self.wanted)
    }

    /// Files changed by the AI pane titled `title` and still under review.
    pub fn by(&self, title: &str) -> usize {
        self.files
            .iter()
            .filter(|f| f.by.iter().any(|b| b == title))
            .count()
    }

    pub fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    fn relative(&self, absolute: &Path) -> Option<String> {
        let store = self.store.as_ref()?;
        let canonical = absolute.canonicalize().ok()?;
        let path = canonical.strip_prefix(&store.top).ok()?;
        Some(path.to_string_lossy().replace('\\', "/"))
    }

    /// Raido saved `absolute`: that change is yours, even while an agent works.
    pub fn saved(&mut self, absolute: &Path) {
        if let Some(path) = self.relative(absolute) {
            if !self.pending.contains_key(&path) {
                let print = fs::read(absolute).ok().map(|b| fingerprint(&b));
                self.written.insert(path, print);
            }
        }
    }

    /// Keep the change: it becomes the version the review compares with.
    pub fn keep(&mut self, target: &Target) -> Result<String, String> {
        match target {
            Target::All => {
                let count = self.pending.len();
                self.acted.extend(self.pending.keys().cloned());
                self.pending.clear();
                self.files.clear();
                self.version += 1;
                Ok(format!("Kept the changes to {}", files(count)))
            }
            Target::File(path) => {
                self.forget(path);
                Ok(format!("Kept {}", self.name(path)))
            }
            Target::Hunk(path, span) => {
                let Versions {
                    store,
                    base,
                    old,
                    new,
                } = self.versions(path)?;
                let (old_lines, new_lines) = (lines(&old), lines(&new));
                Self::check(&old_lines, &new_lines, span)?;
                let mut kept: Vec<&str> = old_lines[..span.old.0].to_vec();
                kept.extend(&new_lines[span.new.0..span.new.1]);
                kept.extend(&old_lines[span.old.1..]);
                let kept = kept.concat();
                let mut base = base;
                base.blob = Some(store.write(kept.as_bytes(), path)?);
                self.pending.insert(path.clone(), base);
                self.acted.insert(path.clone());
                self.refresh(path)?;
                Ok("Kept that change".into())
            }
        }
    }

    /// Undo the change: the file goes back to how it was. Returns the files written.
    pub fn undo(&mut self, target: &Target) -> Result<Vec<PathBuf>, String> {
        match target {
            Target::All => {
                let mut written = Vec::new();
                let paths: Vec<String> = self.pending.keys().cloned().collect();
                for path in paths {
                    written.extend(self.undo(&Target::File(path))?);
                }
                Ok(written)
            }
            Target::File(path) => {
                let Versions { base, old, .. } = self.versions(path)?;
                let absolute =
                    self.write_file(path, base.blob.is_some().then_some(&old[..]), &base)?;
                self.forget(path);
                Ok(vec![absolute])
            }
            Target::Hunk(path, span) => {
                let Versions { base, old, new, .. } = self.versions(path)?;
                let (old_lines, new_lines) = (lines(&old), lines(&new));
                Self::check(&old_lines, &new_lines, span)?;
                let mut undone: Vec<&str> = new_lines[..span.new.0].to_vec();
                undone.extend(&old_lines[span.old.0..span.old.1]);
                undone.extend(&new_lines[span.new.1..]);
                let undone = undone.concat();
                // Undoing all of a new file removes it.
                let gone = base.blob.is_none() && undone.is_empty();
                let absolute =
                    self.write_file(path, (!gone).then_some(undone.as_bytes()), &base)?;
                self.acted.insert(path.clone());
                self.refresh(path)?;
                Ok(vec![absolute])
            }
        }
    }

    fn check(old: &[&str], new: &[&str], span: &Span) -> Result<(), String> {
        if hunks(old, new).iter().any(|h| h.span == *span) {
            Ok(())
        } else {
            Err("That change moved since it was shown; look again".into())
        }
    }

    /// The file's reviewed version and what's on disk.
    fn versions(&self, path: &str) -> Result<Versions, String> {
        let store = self.store.clone().ok_or("No snapshot yet")?;
        let base = self
            .pending
            .get(path)
            .cloned()
            .ok_or("That file isn't under review anymore")?;
        let old = match &base.blob {
            Some(blob) => store.blob(blob, path)?,
            None => Vec::new(),
        };
        let new = read(&store.top.join(path))?.unwrap_or_default();
        Ok(Versions {
            store,
            base,
            old,
            new,
        })
    }

    fn write_file(
        &mut self,
        path: &str,
        bytes: Option<&[u8]>,
        base: &Base,
    ) -> Result<PathBuf, String> {
        let store = self.store.clone().ok_or("No snapshot yet")?;
        let absolute = store.top.join(path);
        let fail = |e: std::io::Error| format!("{path}: {e}");
        match bytes {
            Some(bytes) => {
                let existed = absolute.exists();
                if let Some(parent) = absolute.parent() {
                    fs::create_dir_all(parent).map_err(fail)?;
                }
                fs::write(&absolute, bytes).map_err(fail)?;
                #[cfg(unix)]
                if !existed && base.executable {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&absolute, fs::Permissions::from_mode(0o755));
                }
                #[cfg(not(unix))]
                let _ = (existed, base.executable);
            }
            None => match fs::remove_file(&absolute) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(fail(e)),
                _ => {}
            },
        }
        self.written
            .insert(path.to_string(), bytes.map(fingerprint));
        Ok(absolute)
    }

    fn forget(&mut self, path: &str) {
        self.pending.remove(path);
        self.acted.insert(path.to_string());
        self.files.retain(|f| f.path != path);
        self.version += 1;
    }

    /// Recompute one file's diff after keeping or undoing part of it.
    fn refresh(&mut self, path: &str) -> Result<(), String> {
        let Versions {
            store,
            base,
            old,
            new,
        } = self.versions(path)?;
        let exists = store.top.join(path).exists();
        let diff = compare(
            &store,
            path,
            &base,
            base.blob.is_some().then_some(&old[..]),
            exists.then_some(&new[..]),
        );
        match diff {
            Some(diff) => match self.files.iter_mut().find(|f| f.path == path) {
                Some(slot) => *slot = diff,
                None => {
                    self.files.push(diff);
                    self.files.sort_by(|a, b| a.path.cmp(&b.path));
                }
            },
            None => self.forget(path),
        }
        self.version += 1;
        Ok(())
    }

    fn name(&self, path: &str) -> String {
        self.files
            .iter()
            .find(|f| f.path == path)
            .map_or_else(|| path.to_string(), |f| f.name.clone())
    }
}

impl Drop for Changes {
    fn drop(&mut self) {
        if self.store.is_some() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn same(a: &FileDiff, b: &FileDiff) -> bool {
    a.path == b.path
        && a.status == b.status
        && a.by == b.by
        && a.added == b.added
        && a.removed == b.removed
        && match (&a.body, &b.body) {
            (Body::Text(x), Body::Text(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y)
                        .all(|(p, q)| p.span == q.span && p.lines == q.lines)
            }
            (Body::Binary, Body::Binary) => true,
            _ => false,
        }
}

pub fn files(count: usize) -> String {
    format!("{count} file{}", if count == 1 { "" } else { "s" })
}

/// One row of the review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A file's heading.
    File(usize),
    /// `@@ -10,6 +10,8 @@`: (file, hunk).
    Hunk(usize, usize),
    /// (file, hunk, line).
    Line(usize, usize, usize),
    /// A file without lines to show (binary, empty).
    Note(usize),
    /// A line of the summary (`ChangesView::texts`): under a file's heading, or of
    /// the whole change at the end (no file).
    Text(Option<usize>, usize),
    /// Space between files.
    Gap,
}

/// How a summary line reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Written by the AI.
    Ai,
    New,
    Changed,
    Removed,
    Plain,
    /// The summary's heading.
    Title,
    /// Hints and states.
    Faint,
}

impl Row {
    pub fn file(self) -> Option<usize> {
        match self {
            Row::File(f) | Row::Hunk(f, _) | Row::Line(f, _, _) | Row::Note(f) => Some(f),
            Row::Text(f, _) => f,
            Row::Gap => None,
        }
    }
    pub fn hunk(self) -> Option<(usize, usize)> {
        match self {
            Row::Hunk(f, h) | Row::Line(f, h, _) => Some((f, h)),
            _ => None,
        }
    }
}

/// The review pane (Ctrl-b v): every changed file's hunks, one after another, with
/// a cursor row that says which hunk or file to keep or undo.
#[derive(Default)]
pub struct ChangesView {
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub scroll: usize,
    pub hscroll: usize,
    /// Rows on screen, recorded at draw time for paging.
    pub height: usize,
    /// Columns for the rows, recorded at draw time to wrap the summary.
    pub width: usize,
    /// Summaries shown: `notes` when you toggled them (`i`), else config's
    /// `review_summary`; `show` is what the rows were built with.
    pub notes: Option<bool>,
    pub show: bool,
    /// The summary's lines, for `Row::Text`.
    pub texts: Vec<(Tone, String)>,
    /// Each hunk's lines in its language's colors (`None`: plain text), by (file, hunk).
    #[allow(clippy::type_complexity)]
    pub colors: HashMap<(usize, usize), Option<Vec<Vec<(syntect::highlighting::Style, String)>>>>,
    /// What the rows were built from: changes version, width, summaries shown.
    built: Option<(u64, usize, bool)>,
    /// The files' paths when the rows were built, to find them again.
    paths: Vec<String>,
    /// An undo waiting for `y`: what, and the question asked.
    pub confirm: Option<(Target, String)>,
}

impl ChangesView {
    /// Rebuild the rows when the changes did, keeping the cursor on the same file
    /// and hunk (or the one that took its place).
    pub fn sync(&mut self, changes: &Changes) {
        let wanted = (changes.version, self.width, self.show);
        if self.built == Some(wanted) {
            return;
        }
        let anchor = self.rows.get(self.cursor).copied().map(|row| {
            let path = row.file().and_then(|f| self.paths.get(f).cloned());
            (row, path)
        });
        self.built = Some(wanted);
        self.paths = changes.files.iter().map(|f| f.path.clone()).collect();
        self.rows.clear();
        self.texts.clear();
        self.colors.clear();
        for (f, file) in changes.files.iter().enumerate() {
            if f > 0 {
                self.rows.push(Row::Gap);
            }
            self.rows.push(Row::File(f));
            if self.show {
                for line in self.file_notes(changes, file) {
                    self.texts.push(line);
                    self.rows.push(Row::Text(Some(f), self.texts.len() - 1));
                }
            }
            let hunks = file.hunks();
            if hunks.is_empty() {
                self.rows.push(Row::Note(f));
            }
            for (h, hunk) in hunks.iter().enumerate() {
                self.rows.push(Row::Hunk(f, h));
                self.rows
                    .extend((0..hunk.lines.len()).map(|l| Row::Line(f, h, l)));
            }
        }
        if self.show && !changes.files.is_empty() {
            self.rows.push(Row::Gap);
            for line in self.overview(changes) {
                self.texts.push(line);
                self.rows.push(Row::Text(None, self.texts.len() - 1));
            }
        }
        let Some((row, path)) = anchor else {
            self.cursor = 0;
            return;
        };
        let file = path
            .and_then(|p| changes.files.iter().position(|f| f.path == p))
            .or_else(|| {
                row.file()
                    .map(|f| f.min(changes.files.len().saturating_sub(1)))
            });
        let target = file.and_then(|f| {
            let at = |wanted: Row| self.rows.iter().position(|r| *r == wanted);
            let hunks = changes.files.get(f)?.hunks().len();
            match row {
                Row::Text(None, _) => None,
                Row::Hunk(_, h) | Row::Line(_, h, _) if hunks > 0 => {
                    let h = h.min(hunks - 1);
                    let offset = match row {
                        Row::Line(_, _, l) => l + 1,
                        _ => 0,
                    };
                    let length = changes.files[f].hunks()[h].lines.len();
                    at(Row::Hunk(f, h)).map(|i| i + offset.min(length))
                }
                _ => at(Row::File(f)),
            }
        });
        self.cursor = target.unwrap_or(self.cursor.min(self.rows.len().saturating_sub(1)));
    }

    /// Room for summary text after its indent.
    fn room(&self) -> usize {
        self.width.saturating_sub(6).max(20)
    }

    /// Under a file's heading: what the AI said about it, then what the code shows
    /// was added, changed and removed.
    fn file_notes(&self, changes: &Changes, file: &FileDiff) -> Vec<(Tone, String)> {
        let mut out = Vec::new();
        if let Some((said, now)) = changes.summary.as_ref().and_then(|s| s.note(file)) {
            // Said about changes it has since gone past: kept, but dimmed.
            let (tone, mut said) = if now {
                (Tone::Ai, said.to_string())
            } else {
                (Tone::Faint, format!("{said} (before newer changes · s)"))
            };
            if changes
                .summary
                .as_ref()
                .and_then(|s| s.files.get(&file.path))
                .is_some_and(|n| n.partial)
            {
                said.push_str(" (read in part: its changes are too large to send whole)");
            }
            for (i, line) in crate::ui::wrap_words(&said, self.room())
                .into_iter()
                .enumerate()
            {
                out.push((tone, format!("{} {line}", if i == 0 { "◆" } else { " " })));
            }
        }
        let outline = &file.outline;
        const SHOWN: usize = 5;
        for d in outline.added.iter().take(SHOWN) {
            let doc = d
                .doc
                .as_ref()
                .map(|doc| format!(" — {doc}"))
                .unwrap_or_default();
            out.push((Tone::New, format!("+ {} {}{doc}", d.kind, d.name)));
        }
        if outline.added.len() > SHOWN {
            let rest: Vec<&str> = outline.added[SHOWN..]
                .iter()
                .map(|d| d.name.as_str())
                .collect();
            out.push((
                Tone::New,
                format!("+ {} more: {}", rest.len(), rest.join(", ")),
            ));
        }
        if outline.tests > 0 {
            let tests = outline.tests;
            out.push((
                Tone::New,
                format!("+ {tests} test{}", if tests == 1 { "" } else { "s" }),
            ));
        }
        if !outline.changed.is_empty() {
            out.push((
                Tone::Changed,
                format!("~ changed {}", outline.changed.join(", ")),
            ));
        }
        if !outline.removed.is_empty() {
            out.push((
                Tone::Removed,
                format!("− removed {}", outline.removed.join(", ")),
            ));
        }
        if !outline.imports.is_empty() {
            out.push((Tone::Plain, format!("↳ {}", outline.imports.join(" · "))));
        }
        out
    }

    /// At the end: the AI's account of the whole change (or how to ask for one), and
    /// what the code shows across all files.
    fn overview(&self, changes: &Changes) -> Vec<(Tone, String)> {
        let files = &changes.files;
        let mut out = vec![(Tone::Title, "Summary".to_string())];
        let room = self.room();
        let wrapped = |tone, mark: &str, text: &str, out: &mut Vec<(Tone, String)>| {
            for (i, line) in crate::ui::wrap_words(text, room).into_iter().enumerate() {
                out.push((tone, format!("{} {line}", if i == 0 { mark } else { " " })));
            }
        };
        match (
            &changes.summary,
            changes.summarizing(),
            &changes.summary_error,
        ) {
            (_, Some(by), _) => out.push((Tone::Faint, format!("◆ {by} is reading the changes…"))),
            (_, _, Some(error)) => wrapped(
                Tone::Removed,
                "◆",
                &format!("The AI summary failed: {error}"),
                &mut out,
            ),
            (Some(summary), ..) if !summary.overall.is_empty() => {
                wrapped(Tone::Ai, "◆", &summary.overall, &mut out);
                // A request too large to send whole: say what it didn't read.
                let skipped = summary
                    .left_out
                    .iter()
                    .filter(|p| files.iter().any(|f| &&f.path == p))
                    .count();
                if skipped > 0 {
                    out.push((
                        Tone::Changed,
                        format!(
                            "  Explained {} of {} files: the rest didn't fit in one request · s explains them",
                            summary.sent,
                            summary.sent + skipped
                        ),
                    ));
                }
                let mut when = format!("  by {}", summary.by);
                if summary.sent + skipped < files.len() {
                    // Asked again: only what was new went.
                    when.push_str(&format!(
                        " · read {} of {} this time, the rest from before",
                        summary.sent,
                        files.len()
                    ));
                }
                if !changes.summary_current() {
                    when.push_str(", before the latest changes");
                }
                when.push_str(" · s asks again");
                out.push((Tone::Faint, when));
                // What it cost, when the AI says, and all the asking since Raido started.
                if let Some(usage) = summary.usage {
                    let mut cost = format!("  {}", usage.describe());
                    if changes.asked > 1 {
                        cost.push_str(&format!(
                            " · {} for {} explanations since Raido started",
                            changes.spent.describe(),
                            changes.asked
                        ));
                    }
                    out.push((Tone::Faint, cost));
                }
            }
            _ => out.push((
                Tone::Faint,
                "◆ s: an AI explains these changes (uses tokens · review_ai in config.toml)".into(),
            )),
        }
        let (added, removed) = changes.totals();
        let mut agents: Vec<&str> = files
            .iter()
            .flat_map(|f| f.by.iter().map(String::as_str))
            .collect();
        agents.sort_unstable();
        agents.dedup();
        let mut line = format!(
            "{} · +{added} −{removed}",
            crate::review::files(files.len())
        );
        if !agents.is_empty() {
            line.push_str(&format!(" · by {}", agents.join(", ")));
        }
        out.push((Tone::Plain, line));
        let new: Vec<&str> = files
            .iter()
            .filter(|f| f.status == Status::Added)
            .map(|f| f.name.as_str())
            .collect();
        if !new.is_empty() {
            out.push((Tone::New, format!("+ new: {}", new.join(", "))));
        }
        let gone: Vec<&str> = files
            .iter()
            .filter(|f| f.status == Status::Deleted)
            .map(|f| f.name.as_str())
            .collect();
        if !gone.is_empty() {
            out.push((Tone::Removed, format!("− deleted: {}", gone.join(", "))));
        }
        // Definitions by what they are.
        let count = |kinds: &[&str]| {
            files
                .iter()
                .flat_map(|f| &f.outline.added)
                .filter(|d| kinds.contains(&d.kind.as_str()))
                .count()
        };
        let plural = |n: usize, one: &str, many: &str| match n {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        };
        let types = [
            "struct",
            "class",
            "enum",
            "trait",
            "interface",
            "type",
            "union",
        ];
        let changed: usize = files.iter().map(|f| f.outline.changed.len()).sum();
        let dropped: usize = files.iter().map(|f| f.outline.removed.len()).sum();
        let tests: usize = files.iter().map(|f| f.outline.tests).sum();
        let parts: Vec<String> = [
            plural(count(&["function"]), "function", "functions").map(|p| format!("adds {p}")),
            plural(count(&types), "type", "types").map(|p| format!("adds {p}")),
            plural(count(&["section"]), "section", "sections").map(|p| format!("adds {p}")),
            plural(changed, "definition changed", "definitions changed"),
            plural(dropped, "removed", "removed"),
            plural(tests, "new test", "new tests"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !parts.is_empty() {
            out.push((Tone::Plain, parts.join(" · ")));
        }
        // What's new, in the words of the comments written for it.
        let documented: Vec<_> = files
            .iter()
            .flat_map(|f| &f.outline.added)
            .filter(|d| d.doc.is_some() && d.kind != "section")
            .take(8)
            .collect();
        for d in documented {
            let doc = d.doc.as_deref().unwrap_or_default();
            out.push((Tone::New, format!("+ {} — {doc}", d.name)));
        }
        out
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    /// `n` / `N`: the next / previous hunk; `]` / `[`: the next / previous file.
    pub fn jump(&mut self, files: bool, back: bool) {
        let wanted = |row: &Row| match row {
            Row::File(_) => files,
            Row::Hunk(..) => !files,
            _ => false,
        };
        let found = if back {
            self.rows[..self.cursor.min(self.rows.len())]
                .iter()
                .rposition(wanted)
        } else {
            self.rows
                .iter()
                .skip(self.cursor + 1)
                .position(wanted)
                .map(|i| i + self.cursor + 1)
        };
        if let Some(i) = found {
            self.cursor = i;
            // Show the hunk or file below its heading, not just the heading.
            self.scroll = i.saturating_sub(2);
        }
    }

    /// What `y` / `x` act on: the hunk under the cursor, or its file.
    pub fn target(&self, changes: &Changes, whole_file: bool) -> Option<Target> {
        let row = *self.rows.get(self.cursor)?;
        let row = if row == Row::Gap {
            *self.rows.get(self.cursor + 1)?
        } else {
            row
        };
        let file = changes.files.get(row.file()?)?;
        match row.hunk() {
            Some((_, h)) if !whole_file => {
                Some(Target::Hunk(file.path.clone(), file.hunks().get(h)?.span))
            }
            _ => Some(Target::File(file.path.clone())),
        }
    }

    /// The file and line (1-based) under the cursor, to open in the editor.
    pub fn location(&self, changes: &Changes) -> Option<(PathBuf, usize)> {
        let row = *self.rows.get(self.cursor)?;
        let file = changes.files.get(row.file()?)?;
        let line = match row {
            Row::Line(_, h, l) => {
                let hunk = &file.hunks()[h];
                // A removed line opens where it was, at the next line still there.
                hunk.lines[..=l]
                    .iter()
                    .rev()
                    .find_map(|line| line.new)
                    .map(|n| {
                        if hunk.lines[l].new.is_some() {
                            n
                        } else {
                            n + 1
                        }
                    })
                    .unwrap_or(hunk.span.new.0 + 1)
            }
            Row::Hunk(_, h) => file.hunks()[h].span.new.0 + 1,
            _ => file.hunks().first().map_or(1, |h| h.span.new.0 + 1),
        };
        Some((file.absolute.clone(), line))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=Raido",
                "-c",
                "user.email=raido@example.com",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .current_dir(root)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// Tick until a snapshot has come back.
    fn settle(changes: &mut Changes, busy: &[String]) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            changes.tick(busy, true, false);
            if changes.job.is_none() && !changes.wanted {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("snapshot never finished");
    }

    /// One agent turn: a snapshot while it waits, then it works (`edit`) and goes quiet.
    pub(crate) fn turn(changes: &mut Changes, agent: &str, edit: impl FnOnce(&mut Changes)) {
        changes.last = Instant::now() - WAITING;
        changes.tick(&[], true, false);
        settle(changes, &[]);
        edit(changes);
        let busy = vec![agent.to_string()];
        changes.last = Instant::now() - BUSY;
        changes.tick(&busy, true, false);
        settle(changes, &busy);
        // It went quiet: one more look covers its last edits.
        changes.last = Instant::now() - BUSY;
        changes.tick(&[], true, false);
        settle(changes, &[]);
    }

    pub(crate) fn project() -> (tempfile::TempDir, PathBuf, Changes) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q"]);
        // The files below are written as Git checks them out here: with LF, also where
        // Git for Windows' default `core.autocrlf` would check them out with CRLF.
        git(&root, &["config", "core.autocrlf", "false"]);
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    one();\n    two();\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("notes.txt"),
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        // Uncommitted work of yours from before any agent ran.
        fs::write(root.join("mine.txt"), "draft\n").unwrap();
        let changes = Changes::with_store(root.clone(), dir.path().join("store"));
        (dir, root, changes)
    }

    /// An agent turn that adds a documented function, changes `main` and adds a file.
    pub(crate) fn feature(changes: &mut Changes, root: &Path) {
        turn(changes, "Claude", |_| {
            fs::write(
                root.join("src/main.rs"),
                "fn main() {\n    one();\n    greet(\"you\");\n}\n\n/// Says hello to `name`.\nfn greet(name: &str) {\n    println!(\"hi {name}\");\n}\n",
            )
            .unwrap();
            fs::write(
                root.join("src/door.py"),
                "class Door:\n    def open(self):\n        \"\"\"Swing it wide.\"\"\"\n        return 1\n",
            )
            .unwrap();
        });
    }

    #[test]
    fn the_review_sums_up_each_file_and_the_whole_change() {
        let (_dir, root, mut changes) = project();
        feature(&mut changes, &root);
        let mut view = ChangesView {
            width: 100,
            show: true,
            ..Default::default()
        };
        view.sync(&changes);
        let notes = |view: &ChangesView, file: Option<usize>| -> Vec<String> {
            view.rows
                .iter()
                .filter_map(|row| match *row {
                    Row::Text(f, t) if f == file => Some(view.texts[t].1.clone()),
                    _ => None,
                })
                .collect()
        };
        // Files in path order: src/door.py, then src/main.rs.
        assert_eq!(
            notes(&view, Some(0)),
            ["+ class Door", "+ function Door.open — Swing it wide."]
        );
        assert_eq!(
            notes(&view, Some(1)),
            ["+ function greet — Says hello to `name`.", "~ changed main"]
        );
        let overview = notes(&view, None);
        assert_eq!(overview[0], "Summary");
        assert!(overview[1].starts_with("◆ s: an AI explains"));
        assert!(
            overview.contains(&"2 files · +10 −1 · by Claude".to_string()),
            "{overview:#?}"
        );
        assert!(overview.contains(&"+ new: src/door.py".to_string()));
        assert!(
            overview.contains(&"adds 2 functions · adds 1 type · 1 definition changed".to_string())
        );
        assert!(overview.contains(&"+ greet — Says hello to `name`.".to_string()));
        // The summary rows belong to their file: y and x there act on all of it.
        let first = view
            .rows
            .iter()
            .position(|r| matches!(r, Row::Text(Some(1), _)))
            .unwrap();
        view.cursor = first;
        assert_eq!(
            view.target(&changes, false),
            Some(Target::File("src/main.rs".into()))
        );
        // An AI's summary goes first, until the changes move on from it.
        let note = crate::summary::Note {
            key: crate::summary::file_key(&changes.files[1]),
            text: "Main now greets you.".into(),
            partial: false,
        };
        changes.summary = Some(crate::summary::Summary {
            files: [("src/main.rs".to_string(), note)].into(),
            overall: "Adds a greeting.".into(),
            key: crate::summary::key(&changes.files),
            by: "Claude".into(),
            sent: 2,
            left_out: Vec::new(),
            usage: Some(crate::summary::Usage {
                input: 3400,
                output: 120,
                cost: Some(0.004),
            }),
        });
        changes.version += 1;
        view.sync(&changes);
        assert_eq!(notes(&view, Some(1))[0], "◆ Main now greets you.");
        let overview = notes(&view, None);
        assert_eq!(overview[1], "◆ Adds a greeting.");
        assert_eq!(overview[2], "  by Claude · s asks again");
        assert_eq!(overview[3], "  3.5k tokens ($0.004)");
        changes.summary.as_mut().unwrap().key += 1;
        changes.version += 1;
        view.sync(&changes);
        assert!(notes(&view, None)[2].contains("before the latest changes"));
        // A file whose changes moved on keeps what was said, marked as older.
        changes
            .summary
            .as_mut()
            .unwrap()
            .files
            .get_mut("src/main.rs")
            .unwrap()
            .key += 1;
        changes.summary.as_mut().unwrap().sent = 1;
        changes.version += 1;
        view.sync(&changes);
        assert_eq!(
            notes(&view, Some(1))[0],
            "◆ Main now greets you. (before newer changes · s)"
        );
        assert!(notes(&view, None)[2].contains("read 1 of 2 this time"));
        // Too large to send whole: it says what it didn't read.
        let summary = changes.summary.as_mut().unwrap();
        summary.left_out = vec!["src/door.py".into()];
        summary.files.get_mut("src/main.rs").unwrap().partial = true;
        changes.version += 1;
        view.sync(&changes);
        let overview = notes(&view, None);
        assert!(
            overview
                .iter()
                .any(|l| l.contains("Explained 1 of 2 files: the rest didn't fit")),
            "{overview:#?}"
        );
        assert!(notes(&view, Some(1))[0].contains("read in part"));
        // Hidden: only the diff.
        view.show = false;
        view.sync(&changes);
        assert!(!view.rows.iter().any(|r| matches!(r, Row::Text(..))));
    }

    #[test]
    fn agent_edits_are_reviewed_and_yours_are_not() {
        let (_dir, root, mut changes) = project();
        let user_objects = || {
            let mut files = Vec::new();
            walk(&root.join(".git/objects"), &mut files);
            // Git's own background maintenance after a commit locks here for a moment.
            files.retain(|f| f.extension().is_none_or(|e| e != "lock"));
            files.sort();
            files
        };
        let before = user_objects();
        turn(&mut changes, "Claude", |_| {
            fs::write(
                root.join("src/main.rs"),
                "fn main() {\n    one();\n    three();\n}\n",
            )
            .unwrap();
            fs::write(root.join("src/new.rs"), "pub fn three() {}\n").unwrap();
            fs::remove_file(root.join("notes.txt")).unwrap();
            fs::create_dir_all(root.join("target")).unwrap();
            fs::write(root.join("target/build.o"), "ignored").unwrap();
        });
        let names: Vec<(&str, Status)> = changes
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.status))
            .collect();
        assert_eq!(
            names,
            [
                ("notes.txt", Status::Deleted),
                ("src/main.rs", Status::Modified),
                ("src/new.rs", Status::Added),
            ]
        );
        let main = &changes.files[1];
        assert_eq!((main.added, main.removed), (1, 1));
        assert_eq!(main.by, ["Claude"]);
        assert_eq!(changes.by("Claude"), 3);
        let text: Vec<String> = main.hunks()[0]
            .lines
            .iter()
            .map(|l| format!("{:?} {}", l.kind, l.text))
            .collect();
        assert!(text.contains(&"Removed     two();".to_string()), "{text:?}");
        assert!(text.contains(&"Added     three();".to_string()), "{text:?}");
        // Nothing was written into the repository itself.
        assert_eq!(user_objects(), before);

        // You edit a file while no agent works: not the agent's change.
        fs::write(root.join("mine.txt"), "draft two\n").unwrap();
        turn(&mut changes, "Claude", |_| {});
        assert_eq!(changes.files.len(), 3);
        assert!(changes.files.iter().all(|f| f.name != "mine.txt"));
    }

    /// Every file under `dir`, for comparing a repository's objects.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                walk(&entry.path(), out);
            } else {
                out.push(entry.path());
            }
        }
    }

    #[test]
    fn crlf_checkouts_review_just_the_changed_lines_and_undo_byte_for_byte() {
        // Git for Windows' default: LF in the repository, CRLF on disk. Each file's
        // version before is one CR a line longer than Git's header says.
        let (_dir, root, mut changes) = project();
        git(&root, &["config", "core.autocrlf", "true"]);
        let crlf = |text: &str| text.replace('\n', "\r\n");
        let main = crlf("fn main() {\n    one();\n    two();\n}\n");
        fs::write(root.join("src/main.rs"), &main).unwrap();
        fs::write(root.join("notes.txt"), crlf("a\nb\nc\n")).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "crlf"]);
        turn(&mut changes, "Claude", |_| {
            fs::write(root.join("notes.txt"), crlf("a\nB\nc\n")).unwrap();
            fs::write(root.join("src/main.rs"), main.replace("two", "three")).unwrap();
        });
        assert_eq!(changes.error, None);
        let lines: Vec<(&str, usize, usize)> = changes
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.added, f.removed))
            .collect();
        assert_eq!(lines, [("notes.txt", 1, 1), ("src/main.rs", 1, 1)]);
        changes.undo(&Target::File("src/main.rs".into())).unwrap();
        assert_eq!(fs::read(root.join("src/main.rs")).unwrap(), main.as_bytes());
    }

    #[test]
    fn split_batch_trusts_the_next_header_over_the_size() {
        // Sizes before filtering (4 and 2), bodies after (CRs added).
        let out = b"aaa blob 4\nx\r\ny\r\n\nbbb blob 2\nz\r\n\n";
        let wanted = [("aaa", "one"), ("bbb", "two")];
        assert_eq!(
            split_batch(out, &wanted).unwrap(),
            [b"x\r\ny\r\n".to_vec(), b"z\r\n".to_vec()]
        );
        let missing = b"aaa missing\n";
        assert!(split_batch(missing, &wanted[..1]).is_err());
    }

    #[test]
    fn saving_from_raido_while_an_agent_works_stays_yours() {
        let (_dir, root, mut changes) = project();
        turn(&mut changes, "Claude", |changes| {
            fs::write(root.join("mine.txt"), "saved in raido\n").unwrap();
            changes.saved(&root.join("mine.txt"));
            fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        });
        let names: Vec<&str> = changes.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["src/main.rs"]);
    }

    #[test]
    fn keep_and_undo_by_hunk_file_and_everything() {
        let (_dir, root, mut changes) = project();
        let notes = root.join("notes.txt");
        turn(&mut changes, "Codex", |_| {
            // Two hunks far apart, a new file and a removed one.
            fs::write(&notes, "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n").unwrap();
            fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
            fs::write(root.join("new.txt"), "new\n").unwrap();
        });
        assert_eq!(changes.files.len(), 3);
        let file =
            |c: &Changes, name: &str| c.files.iter().find(|f| f.name == name).cloned().unwrap();
        let hunks = file(&changes, "notes.txt").hunks().to_vec();
        assert_eq!(hunks.len(), 2);
        // Undo the first hunk: "A" goes back to "a"; "L" stays.
        changes
            .undo(&Target::Hunk("notes.txt".into(), hunks[0].span))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&notes).unwrap(),
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n"
        );
        assert_eq!(file(&changes, "notes.txt").hunks().len(), 1);
        // A stale hunk is refused rather than applied in the wrong place.
        assert!(changes
            .undo(&Target::Hunk("notes.txt".into(), hunks[0].span))
            .is_err());
        // Keep the other: the file leaves the review, its text stays.
        let last = file(&changes, "notes.txt").hunks()[0].span;
        changes
            .keep(&Target::Hunk("notes.txt".into(), last))
            .unwrap();
        assert!(changes.files.iter().all(|f| f.name != "notes.txt"));
        assert!(fs::read_to_string(&notes).unwrap().ends_with("L\n"));

        // Undo a whole file, then keep a new one.
        changes.undo(&Target::File("src/main.rs".into())).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() {\n    one();\n    two();\n}\n"
        );
        changes.keep(&Target::File("new.txt".into())).unwrap();
        assert!(changes.files.is_empty());
        assert!(root.join("new.txt").exists());

        // Raido's own undo, seen by the next snapshot, isn't the agent's change.
        changes.last = Instant::now() - BUSY;
        let busy = vec!["Codex".to_string()];
        changes.tick(&busy, true, false);
        settle(&mut changes, &busy);
        changes.last = Instant::now() - BUSY;
        changes.tick(&[], true, false);
        settle(&mut changes, &[]);
        assert!(changes.files.is_empty(), "{:?}", changes.files);

        // The next turn adds and removes; undoing everything puts it all back.
        turn(&mut changes, "Codex", |_| {
            fs::write(root.join("extra.txt"), "x\n").unwrap();
            fs::remove_file(root.join("src/main.rs")).unwrap();
        });
        assert_eq!(changes.files.len(), 2);
        changes.undo(&Target::All).unwrap();
        assert!(!root.join("extra.txt").exists());
        assert!(root.join("src/main.rs").exists());
        assert!(changes.files.is_empty());
    }

    #[test]
    fn an_agent_putting_a_file_back_takes_it_out_of_the_review() {
        let (_dir, root, mut changes) = project();
        let main = root.join("src/main.rs");
        let original = fs::read_to_string(&main).unwrap();
        turn(&mut changes, "Claude", |_| {
            fs::write(&main, "changed\n").unwrap()
        });
        assert_eq!(changes.files.len(), 1);
        turn(&mut changes, "Claude", |_| {
            fs::write(&main, &original).unwrap()
        });
        assert!(changes.files.is_empty());
    }

    #[test]
    fn outside_git_there_is_nothing_to_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut changes = Changes::with_store(dir.path().to_path_buf(), dir.path().join("store"));
        let busy = vec!["Claude".to_string()];
        changes.request();
        changes.tick(&busy, true, false);
        let start = Instant::now();
        while changes.problem.is_none() && start.elapsed() < Duration::from_secs(10) {
            changes.tick(&busy, true, false);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(changes.problem.as_deref(), Some("not a Git repository"));
    }

    #[test]
    fn hunks_number_lines_and_keep_context() {
        let old = [
            "a\n", "b\n", "c\n", "d\n", "e\n", "f\n", "g\n", "h\n", "i\n", "j",
        ];
        let new = [
            "a\n", "B\n", "c\n", "d\n", "e\n", "f\n", "g\n", "h\n", "i\n", "j\n",
        ];
        let hunks = hunks(&old, &new);
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            hunks[0].span,
            Span {
                old: (0, 5),
                new: (0, 5)
            }
        );
        let removed = &hunks[0].lines[1];
        assert_eq!(
            (
                removed.kind,
                removed.text.as_str(),
                removed.old,
                removed.new
            ),
            (Kind::Removed, "b", Some(2), None)
        );
        let added = &hunks[0].lines[2];
        assert_eq!((added.kind, added.new), (Kind::Added, Some(2)));
        // The last line only gained its newline.
        let end = hunks[1]
            .lines
            .iter()
            .find(|l| l.kind == Kind::Removed)
            .unwrap();
        assert!(end.unterminated);
    }

    #[test]
    fn the_cursor_stays_on_its_file_and_hunk_as_changes_come_and_go() {
        let (_dir, root, mut changes) = project();
        turn(&mut changes, "Claude", |_| {
            fs::write(
                root.join("notes.txt"),
                "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n",
            )
            .unwrap();
            fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        });
        let mut view = ChangesView::default();
        view.sync(&changes);
        assert_eq!(view.rows[0], Row::File(0));
        view.jump(false, false);
        assert_eq!(view.rows[view.cursor], Row::Hunk(0, 0));
        view.jump(false, false);
        assert_eq!(view.rows[view.cursor], Row::Hunk(0, 1));
        view.move_by(1);
        let target = view.target(&changes, false).unwrap();
        let Target::Hunk(path, _) = &target else {
            panic!("{target:?}")
        };
        assert_eq!(path, "notes.txt");
        let (file, line) = view.location(&changes).unwrap();
        assert!(file.ends_with("notes.txt"));
        assert_eq!(line, 9);
        // Keeping the first hunk leaves the cursor in what's now the only one.
        view.jump(false, true);
        view.jump(false, true);
        assert_eq!(view.rows[view.cursor], Row::Hunk(0, 0));
        let first = view.target(&changes, false).unwrap();
        changes.keep(&first).unwrap();
        view.sync(&changes);
        assert_eq!(view.rows[view.cursor], Row::Hunk(0, 0));
        // Next file, and its whole-file target.
        view.jump(true, false);
        assert_eq!(view.rows[view.cursor], Row::File(1));
        assert_eq!(
            view.target(&changes, false),
            Some(Target::File("src/main.rs".into()))
        );
        changes.keep(&Target::File("src/main.rs".into())).unwrap();
        view.sync(&changes);
        assert_eq!(view.rows[view.cursor].file(), Some(0));
    }
}
