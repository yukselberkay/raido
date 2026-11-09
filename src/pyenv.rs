//! The Python environment of a project, for its language server.
//!
//! Pyright and the other Python servers resolve imports with the Python they find on
//! PATH. Raido starts them on the project's virtual environment instead (uv's
//! `.venv`), as VS Code does once you select its interpreter, and tells a server that
//! watches files (Pyright does) about packages installed or removed and modules added
//! or deleted while it runs, as VS Code's file watcher does. Without both, those
//! imports show "could not be resolved".
use crate::workspace::folder_moved;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver},
        Weak,
    },
    thread,
    time::{Duration, SystemTime},
};

/// How often the project and its environment are looked at. Changes are reported
/// once a look finds nothing new, so an install arrives whole.
const LOOK: Duration = Duration::from_secs(1);
/// Project folders followed; a bigger tree is followed in part.
const MAX_FOLDERS: usize = 3_000;
/// Folders that hold no modules of the project.
const SKIPPED: [&str; 4] = ["__pycache__", "node_modules", "target", "site-packages"];
/// Files Pyright reads its settings from, in the project root.
const SETTINGS: [&str; 2] = ["pyproject.toml", "pyrightconfig.json"];

/// What happened to a file, numbered as `workspace/didChangeWatchedFiles` has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

pub enum News {
    /// Files made, changed or removed; `packages` when installs or removals were
    /// among them.
    Files {
        changes: Vec<(PathBuf, Change)>,
        packages: bool,
    },
    /// The environment was made, removed or rebuilt (now `Some` venv, or none): the
    /// server must start again on it.
    Environment(Option<PathBuf>),
}

/// The virtual environment for files in `folder`: the activated one
/// (`$VIRTUAL_ENV`), else `.venv` or `venv` there or in a folder above it up to the
/// project root, where uv, Poetry (in-project) and `python -m venv` make it.
pub fn find(root: &Path, folder: &Path) -> Option<PathBuf> {
    find_in(
        root,
        folder,
        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
    )
}

fn find_in(root: &Path, folder: &Path, active: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(active) = active.filter(|dir| is_venv(dir)) {
        return Some(active);
    }
    let start = if folder.starts_with(root) {
        folder
    } else {
        root
    };
    for dir in start.ancestors() {
        for name in [".venv", "venv"] {
            let venv = dir.join(name);
            if is_venv(&venv) {
                return Some(venv);
            }
        }
        if dir == root {
            break;
        }
    }
    None
}

fn is_venv(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file()
}

/// Where the environment's programs are: its `python`, and maybe the project's own
/// language server (`uv add --dev pyright`).
pub fn bin(venv: &Path) -> PathBuf {
    venv.join(if cfg!(windows) { "Scripts" } else { "bin" })
}

/// The variables a server needs to run on `venv`, as `activate` sets them: the
/// environment's programs first on PATH, and VIRTUAL_ENV naming it.
pub fn variables(venv: &Path) -> [(OsString, OsString); 2] {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(bin(venv)).chain(std::env::split_paths(&path));
    let path = std::env::join_paths(dirs).unwrap_or(path);
    [("VIRTUAL_ENV".into(), venv.into()), ("PATH".into(), path)]
}

/// Follow the project at `root` and `venv`, the environment chosen for `folder`,
/// from a thread that ends when `alive` is dropped.
pub fn watch(
    root: PathBuf,
    folder: PathBuf,
    venv: Option<PathBuf>,
    alive: Weak<()>,
) -> Receiver<News> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A rebuilt environment has a new pyvenv.cfg, maybe for another Python.
        let made = |venv: Option<&Path>| {
            venv.map(|dir| (dir.to_path_buf(), modified(&dir.join("pyvenv.cfg"))))
        };
        let environment = made(venv.as_deref());
        let mut watched = Watched::new(root.clone(), venv);
        let mut pending = Vec::new();
        let mut packages = false;
        loop {
            thread::sleep(LOOK);
            if alive.strong_count() == 0 {
                return;
            }
            let now = find(&root, &folder);
            if made(now.as_deref()) != environment {
                let _ = sender.send(News::Environment(now));
                return;
            }
            let (found, installed) = watched.look(true);
            if !found.is_empty() {
                pending.extend(found);
                packages |= installed;
            } else if !pending.is_empty() {
                let changes = std::mem::take(&mut pending);
                if sender.send(News::Files { changes, packages }).is_err() {
                    return;
                }
                packages = false;
            }
        }
    });
    receiver
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A folder as last read: its time, the entries that matter in it, and which of
/// them are folders to follow.
#[derive(Default)]
struct Listing {
    time: Option<SystemTime>,
    names: BTreeSet<OsString>,
    folders: Vec<OsString>,
}

/// What the watcher has seen of the project and its environment.
struct Watched {
    root: PathBuf,
    venv: Option<PathBuf>,
    /// The environment's site-packages folders: each entry is a package, a module
    /// or their metadata.
    packages: BTreeMap<PathBuf, Listing>,
    /// The project's folders, with their Python files and folders.
    folders: BTreeMap<PathBuf, Listing>,
    settings: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Watched {
    fn new(root: PathBuf, venv: Option<PathBuf>) -> Self {
        let settings = SETTINGS
            .iter()
            .map(|name| {
                let path = root.join(name);
                let time = modified(&path);
                (path, time)
            })
            .collect();
        let mut watched = Self {
            root,
            venv,
            packages: BTreeMap::new(),
            folders: BTreeMap::new(),
            settings,
        };
        watched.look(false);
        watched
    }

    /// Look again: what changed since the last look (when `report`), and whether
    /// packages were among it.
    fn look(&mut self, report: bool) -> (Vec<(PathBuf, Change)>, bool) {
        let mut changes = Vec::new();
        self.look_at_packages(report, &mut changes);
        let installed = !changes.is_empty();
        self.look_at_folders(report, &mut changes);
        for (path, then) in &mut self.settings {
            let now = modified(path);
            if now != *then {
                let change = match (then.is_some(), now.is_some()) {
                    (false, _) => Change::Created,
                    (_, false) => Change::Deleted,
                    _ => Change::Changed,
                };
                if report {
                    changes.push((path.clone(), change));
                }
                *then = now;
            }
        }
        (changes, installed)
    }

    fn look_at_packages(&mut self, report: bool, changes: &mut Vec<(PathBuf, Change)>) {
        let Some(venv) = &self.venv else {
            return;
        };
        let found = site_packages(venv);
        self.packages.retain(|dir, _| found.contains(dir));
        for dir in found {
            let listing = self.packages.entry(dir.clone()).or_default();
            let (moved, time) = folder_moved(&dir, listing.time);
            if !moved {
                continue;
            }
            let names = fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|name| name != "__pycache__")
                .collect();
            if report {
                compare(&dir, &listing.names, &names, changes);
            }
            *listing = Listing {
                time,
                names,
                folders: Vec::new(),
            };
        }
    }

    fn look_at_folders(&mut self, report: bool, changes: &mut Vec<(PathBuf, Change)>) {
        let mut stack = vec![self.root.clone()];
        let mut seen = BTreeSet::new();
        while let Some(dir) = stack.pop() {
            if seen.len() >= MAX_FOLDERS {
                break;
            }
            seen.insert(dir.clone());
            let listing = self.folders.entry(dir.clone()).or_default();
            let (moved, time) = folder_moved(&dir, listing.time);
            if moved {
                let (names, folders) = python_entries(&dir);
                // A folder new since the last look is new with all it holds.
                if report {
                    compare(&dir, &listing.names, &names, changes);
                }
                *listing = Listing {
                    time,
                    names,
                    folders,
                };
            }
            stack.extend(listing.folders.iter().rev().map(|name| dir.join(name)));
        }
        self.folders.retain(|dir, _| seen.contains(dir));
    }
}

/// The site-packages folders of `venv`: `lib/python3.X/site-packages`, or on
/// Windows `Lib\site-packages`.
fn site_packages(venv: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![venv.join("Lib").join("site-packages")];
    }
    let mut found: Vec<PathBuf> = fs::read_dir(venv.join("lib"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("site-packages"))
        .filter(|dir| dir.is_dir())
        .collect();
    found.sort();
    found
}

/// The Python files (`.py`, `.pyi`) and the folders worth following in `dir`.
fn python_entries(dir: &Path) -> (BTreeSet<OsString>, Vec<OsString>) {
    let mut names = BTreeSet::new();
    let mut folders = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let text = name.to_string_lossy();
            if text.starts_with('.') || SKIPPED.contains(&&*text) || is_venv(&entry.path()) {
                continue;
            }
            names.insert(name.clone());
            folders.push(name);
        } else if matches!(
            Path::new(&name).extension().and_then(|e| e.to_str()),
            Some("py" | "pyi")
        ) {
            names.insert(name);
        }
    }
    folders.sort();
    (names, folders)
}

fn compare(
    dir: &Path,
    before: &BTreeSet<OsString>,
    after: &BTreeSet<OsString>,
    changes: &mut Vec<(PathBuf, Change)>,
) {
    changes.extend(
        after
            .difference(before)
            .map(|name| (dir.join(name), Change::Created)),
    );
    changes.extend(
        before
            .difference(after)
            .map(|name| (dir.join(name), Change::Deleted)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venv(at: &Path) -> PathBuf {
        let site = at.join("lib/python3.14/site-packages");
        fs::create_dir_all(&site).unwrap();
        fs::write(at.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        site
    }

    #[test]
    fn the_projects_venv_is_found_beside_the_file_or_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("app/pkg")).unwrap();
        assert_eq!(find_in(root, &root.join("app/pkg"), None), None);
        // uv's .venv in the project root serves every folder.
        venv(&root.join(".venv"));
        assert_eq!(
            find_in(root, &root.join("app/pkg"), None),
            Some(root.join(".venv"))
        );
        // A sub-project's own venv is nearer; `venv` counts as well as `.venv`.
        venv(&root.join("app/venv"));
        assert_eq!(
            find_in(root, &root.join("app/pkg"), None),
            Some(root.join("app/venv"))
        );
        // A file outside the project: the project's environment.
        assert_eq!(
            find_in(root, Path::new("/elsewhere"), None),
            Some(root.join(".venv"))
        );
        // An activated environment wins, unless it is gone.
        let other = tempfile::tempdir().unwrap();
        venv(other.path());
        assert_eq!(
            find_in(root, root, Some(other.path().into())),
            Some(other.path().into())
        );
        assert_eq!(
            find_in(root, root, Some(root.join("gone"))),
            Some(root.join(".venv"))
        );
        // A folder named .venv that isn't an environment is passed over.
        fs::remove_file(root.join(".venv/pyvenv.cfg")).unwrap();
        assert_eq!(find_in(root, root, None), None);
    }

    #[test]
    fn the_server_runs_with_the_venvs_programs_first() {
        let [(name, value), (path, dirs)] = variables(Path::new("/p/.venv"));
        assert_eq!(
            (name.to_str(), value.to_str()),
            (Some("VIRTUAL_ENV"), Some("/p/.venv"))
        );
        assert_eq!(path, "PATH");
        let first = std::env::split_paths(&dirs).next().unwrap();
        assert_eq!(first, bin(Path::new("/p/.venv")));
    }

    #[test]
    fn installs_new_modules_and_settings_are_reported_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let site = venv(&root.join(".venv"));
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("main.py"), "").unwrap();
        let mut watched = Watched::new(root.into(), Some(root.join(".venv")));
        assert_eq!(watched.look(true), (Vec::new(), false));
        let sorted = |(mut changes, packages): (Vec<(PathBuf, Change)>, bool)| {
            changes.sort_by(|a, b| a.0.cmp(&b.0));
            let changes: Vec<_> = changes
                .into_iter()
                .map(|(path, change)| {
                    (
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        change,
                    )
                })
                .collect();
            (changes, packages)
        };
        // `uv add six`: the module and its metadata.
        fs::write(site.join("six.py"), "").unwrap();
        fs::create_dir(site.join("six-1.17.0.dist-info")).unwrap();
        fs::create_dir(site.join("__pycache__")).unwrap();
        let rel = ".venv/lib/python3.14/site-packages";
        assert_eq!(
            sorted(watched.look(true)),
            (
                vec![
                    (format!("{rel}/six-1.17.0.dist-info"), Change::Created),
                    (format!("{rel}/six.py"), Change::Created),
                ],
                true
            )
        );
        // A module and a package of the project; other files and caches are not.
        fs::write(root.join("app/helper.py"), "").unwrap();
        fs::write(root.join("app/notes.txt"), "").unwrap();
        fs::create_dir_all(root.join("app/sub")).unwrap();
        fs::write(root.join("app/sub/__init__.py"), "").unwrap();
        fs::create_dir_all(root.join(".mypy_cache/x")).unwrap();
        fs::create_dir_all(root.join("app/__pycache__")).unwrap();
        assert_eq!(
            sorted(watched.look(true)),
            (
                vec![
                    ("app/helper.py".into(), Change::Created),
                    ("app/sub".into(), Change::Created),
                    ("app/sub/__init__.py".into(), Change::Created),
                ],
                false
            )
        );
        // Removed and renamed.
        fs::remove_file(site.join("six.py")).unwrap();
        fs::rename(root.join("main.py"), root.join("cli.py")).unwrap();
        assert_eq!(
            sorted(watched.look(true)),
            (
                vec![
                    (format!("{rel}/six.py"), Change::Deleted),
                    ("cli.py".into(), Change::Created),
                    ("main.py".into(), Change::Deleted),
                ],
                true
            )
        );
        // Pyright's settings.
        fs::write(root.join("pyproject.toml"), "[tool.pyright]\n").unwrap();
        assert_eq!(
            sorted(watched.look(true)),
            (vec![("pyproject.toml".into(), Change::Created)], false)
        );
        assert_eq!(watched.look(true), (Vec::new(), false));
    }
}
