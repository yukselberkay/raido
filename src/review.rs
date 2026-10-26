//! Review what AI agents change, like VS Code's chat edits. While AI panes are open,
//! Odin snapshots the project with Git into its own index and object store (your
//! repository, index and stash are never written): every couple of seconds while
//! an agent works, every few seconds while they wait. A file that changes between
//! two snapshots while an agent worked is remembered as it was before, so the review
//! shows everything the agents changed since you last looked, and each change can be
//! kept or undone: one hunk, one file or all. Files that change while no agent
//! works, or that you save from Odin's editor, are yours and stay out of it.
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
/// Stores left behind by an Odin that didn't quit cleanly are cleared after this.
const STALE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Git, pointed at the project but with Odin's own index and object store. Objects
/// the repository already has are read from it (an alternate), never written to it.
struct Store {
    /// The repository's top level: Git's paths are relative to it.
    top: PathBuf,
    /// Odin's project folder, `top` or inside it.
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
            // Only Odin uses this index, one snapshot at a time: a lock is left over
            // from a git that took too long and was stopped.
            let _ = fs::remove_file(self.dir.join("index.lock"));
            // A file that can't be read is left as it was; the rest still counts.
            let _ = self.git(
                &self.root,
                &["add", "-A", "--ignore-errors", "--", ".", ":(exclude).odin"],
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
        let mut blobs = Vec::new();
        let mut rest = &out[..];
        for _ in wanted {
            let end = rest
                .iter()
                .position(|&b| b == b'\n')
                .ok_or("short answer from git cat-file")?;
            let header = String::from_utf8_lossy(&rest[..end]).to_string();
            let size: usize = header
                .rsplit(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("git cat-file: {header}"))?;
            let start = end + 1;
            let body = rest
                .get(start..start + size)
                .ok_or("short answer from git cat-file")?;
            blobs.push(body.to_vec());
            rest = rest.get(start + size + 1..).unwrap_or_default();
        }
        Ok(blobs)
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

/// Run `command`, feeding it `input`, and return what it printed.
fn run(command: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let mut stdout = child.stdout.take().ok_or("git: no output")?;
    let mut stderr = child.stderr.take().ok_or("git: no output")?;
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
            Ok(None) if start.elapsed() < TIMEOUT => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git took too long".into());
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    let errors = errors.join().unwrap_or_default();
    if status.success() {
        Ok(bytes)
    } else {
        let line = errors.lines().next().unwrap_or("failed").to_string();
        Err(format!("git: {line}"))
    }
}

/// Remove stores of other Odins that are long gone.
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
    /// The path shown: relative to Odin's project folder.
    pub name: String,
    pub absolute: PathBuf,
    pub status: Status,
    pub by: Vec<String>,
    pub body: Body,
    pub added: usize,
    pub removed: usize,
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
    let body = if is_text(old) && is_text(new) {
        Body::Text(hunks(&lines(old), &lines(new)))
    } else {
        Body::Binary
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
    Some(FileDiff {
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
    /// Files that changed since the last snapshot (not counting Odin's own writes),
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
    /// Files Odin wrote itself (saved, undone), by their fingerprint.
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
}

impl Changes {
    pub fn new(root: PathBuf) -> Self {
        let base = directories::BaseDirs::new()
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base
            .join("odin")
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
        }
    }

    /// Snapshot regularly while an agent works and once after it stops, now and then
    /// while AI panes wait (`agents`), and while the review is `watching`. `busy`
    /// names the AI panes working now. True when the review changed.
    pub fn tick(&mut self, busy: &[String], agents: bool, watching: bool) -> bool {
        let mut changed = false;
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

    /// Odin saved `absolute`: that change is yours, even while an agent works.
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
    /// Space between files.
    Gap,
}

impl Row {
    pub fn file(self) -> Option<usize> {
        match self {
            Row::File(f) | Row::Hunk(f, _) | Row::Line(f, _, _) | Row::Note(f) => Some(f),
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
    version: Option<u64>,
    /// The files' paths when the rows were built, to find them again.
    paths: Vec<String>,
    /// An undo waiting for `y`: what, and the question asked.
    pub confirm: Option<(Target, String)>,
}

impl ChangesView {
    /// Rebuild the rows when the changes did, keeping the cursor on the same file
    /// and hunk (or the one that took its place).
    pub fn sync(&mut self, changes: &Changes) {
        if self.version == Some(changes.version) {
            return;
        }
        let anchor = self.rows.get(self.cursor).copied().map(|row| {
            let path = row.file().and_then(|f| self.paths.get(f).cloned());
            (row, path)
        });
        self.version = Some(changes.version);
        self.paths = changes.files.iter().map(|f| f.path.clone()).collect();
        self.rows.clear();
        for (f, file) in changes.files.iter().enumerate() {
            if f > 0 {
                self.rows.push(Row::Gap);
            }
            self.rows.push(Row::File(f));
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
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.name=Odin", "-c", "user.email=odin@example.com"])
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
    fn turn(changes: &mut Changes, agent: &str, edit: impl FnOnce(&mut Changes)) {
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

    fn project() -> (tempfile::TempDir, PathBuf, Changes) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q"]);
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
    fn saving_from_odin_while_an_agent_works_stays_yours() {
        let (_dir, root, mut changes) = project();
        turn(&mut changes, "Claude", |changes| {
            fs::write(root.join("mine.txt"), "saved in odin\n").unwrap();
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

        // Odin's own undo, seen by the next snapshot, isn't the agent's change.
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
