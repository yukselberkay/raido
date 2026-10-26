//! Per-project sessions: the pane layout, open buffers (with unsaved text), terminal
//! transcripts and working directories, AI panes and the Git pane, saved to
//! `<project>/.odin/session.json` and restored the next time Odin opens the project.
//! The `.odin` directory ignores itself in Git, since transcripts and unsaved text
//! may hold anything you typed.
use crate::{layout::Tile, terminal::Launch};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const DIR: &str = ".odin";
const FILE: &str = "session.json";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub tiles: Tile,
    pub focused: usize,
    pub zoom: bool,
    pub sidebar_visible: bool,
    pub buffers: Vec<SavedBuffer>,
    pub panes: Vec<(usize, SavedPane)>,
    /// Panes in the layout but docked (Ctrl-b d): hidden, still running.
    #[serde(default)]
    pub docked: Vec<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedBuffer {
    /// Relative to the project root when inside it.
    pub path: Option<PathBuf>,
    /// The buffer's text when it differs from the file: unsaved edits, or scratch text.
    pub text: Option<String>,
    pub row: usize,
    pub col: usize,
    pub scroll: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedPane {
    Editor {
        buffer: usize,
    },
    Preview {
        buffer: usize,
        scroll: usize,
    },
    Terminal {
        launch: Launch,
        cwd: Option<PathBuf>,
        /// Screen and scrollback as text with color escapes, replayed on restore.
        transcript: String,
    },
    Ai,
    Git {
        only: Option<String>,
    },
    /// The review of AI agents' changes; what it shows is found again.
    Changes,
}

impl Session {
    pub fn new(
        tiles: Tile,
        focused: usize,
        zoom: bool,
        sidebar_visible: bool,
        buffers: Vec<SavedBuffer>,
        panes: Vec<(usize, SavedPane)>,
    ) -> Self {
        Self {
            version: VERSION,
            tiles,
            focused,
            zoom,
            sidebar_visible,
            buffers,
            panes,
            docked: Vec::new(),
        }
    }

    /// Reject a session whose layout and panes disagree (hand-edited or truncated).
    pub fn check(&self) -> Result<()> {
        if self.version != VERSION {
            bail!("session version {} is not supported", self.version);
        }
        let mut leaves = Vec::new();
        self.tiles.leaves(&mut leaves);
        let mut ids: Vec<usize> = self.panes.iter().map(|(id, _)| *id).collect();
        leaves.sort_unstable();
        ids.sort_unstable();
        if leaves.is_empty() || leaves != ids {
            bail!("session layout does not match its panes");
        }
        Ok(())
    }
}

pub fn file(root: &Path) -> PathBuf {
    root.join(DIR).join(FILE)
}

/// Sessions save automatically in Git repositories and wherever `.odin` exists
/// (`:session save` creates it), so opening Odin in any directory leaves no trace.
pub fn wanted(root: &Path) -> bool {
    root.join(DIR).is_dir() || root.join(".git").exists()
}

pub fn load(root: &Path) -> Result<Option<Session>> {
    let path = file(root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let session: Session =
        serde_json::from_str(&text).with_context(|| format!("Invalid {}", path.display()))?;
    session.check()?;
    Ok(Some(session))
}

/// Write the session atomically, readable only by you.
pub fn save(root: &Path, json: &str) -> Result<()> {
    let dir = root.join(DIR);
    fs::create_dir_all(&dir).with_context(|| format!("Create {}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "# Odin session state: never commit it.\n*\n")?;
    }
    let path = file(root);
    let temporary = dir.join(format!("{FILE}.tmp"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options
        .open(&temporary)
        .with_context(|| format!("Write {}", temporary.display()))?;
    out.write_all(json.as_bytes())?;
    out.sync_all()?;
    drop(out);
    fs::rename(&temporary, &path).with_context(|| format!("Write {}", path.display()))?;
    Ok(())
}

pub fn forget(root: &Path) -> Result<()> {
    match fs::remove_file(file(root)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Axis;

    #[test]
    fn sessions_round_trip_and_ignore_themselves_in_git() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!wanted(dir.path()));
        assert!(load(dir.path()).unwrap().is_none());
        let mut tiles = Tile::Leaf(0);
        tiles.split(0, 3, Axis::Horizontal);
        let session = Session::new(
            tiles,
            3,
            false,
            true,
            vec![SavedBuffer {
                path: Some("a.txt".into()),
                text: Some("unsaved".into()),
                row: 1,
                col: 2,
                scroll: 0,
            }],
            vec![
                (0, SavedPane::Editor { buffer: 0 }),
                (
                    3,
                    SavedPane::Terminal {
                        launch: Launch::Shell,
                        cwd: None,
                        transcript: "\u{1b}[31mred\u{1b}[0m".into(),
                    },
                ),
            ],
        );
        save(dir.path(), &serde_json::to_string(&session).unwrap()).unwrap();
        assert!(wanted(dir.path()));
        let ignore = fs::read_to_string(dir.path().join(".odin/.gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l == "*"));
        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.focused, 3);
        assert_eq!(loaded.buffers[0].text.as_deref(), Some("unsaved"));
        assert!(matches!(
            &loaded.panes[1].1,
            SavedPane::Terminal { transcript, .. } if transcript.contains("red")
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(file(dir.path())).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "session is private");
        }
        forget(dir.path()).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn mismatched_layouts_are_rejected() {
        let session = Session::new(Tile::Leaf(1), 1, false, true, Vec::new(), Vec::new());
        assert!(session.check().is_err());
    }
}
