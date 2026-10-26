//! The Raido agent skill: interactive documentation that AI agents pick up on their
//! own. It uses the open Agent Skills format (a folder with `SKILL.md`), which
//! Claude Code, Codex, Gemini CLI, OpenCode, Copilot and others read.
//!
//! Raido keeps one copy in its data directory and links it into the shared
//! `~/.agents/skills` (read by Codex, Gemini CLI, OpenCode, Copilot, …) and into
//! `~/.claude/skills` and `~/.codex/skills` when those agents are set up — the same
//! layout Omarchy uses for its own skills. Windows gets copies.
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

const NAME: &str = "raido";
/// Present in every copy Raido writes, so Raido only ever replaces its own files.
const MARKER: &str = ".raido-skill";

/// Files of the skill; the references are the shipped docs, so they never drift.
const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skills/raido/SKILL.md")),
    ("reference/manual.md", include_str!("../README.md")),
    ("reference/themes.md", include_str!("../themes/README.md")),
    (
        "reference/config.example.toml",
        include_str!("../config.example.toml"),
    ),
];

/// Where Raido keeps the skill: `~/.local/share/raido/skills/raido` on Linux,
/// `~/Library/Application Support/raido/…` on macOS, `%APPDATA%\raido\…` on Windows.
pub fn source() -> PathBuf {
    data_dir().join("raido/skills").join(NAME)
}

fn data_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The skill's name and marker when Raido was called Odin.
const ODIN_NAME: &str = "odin";
const ODIN_MARKER: &str = ".odin-skill";

/// Remove the skill Raido installed as Odin: its links and copies in the agents'
/// folders (only its own, never someone else's `odin` skill) and its data folder.
/// Returns how many links and copies there were.
fn retire_odin(home: &Path, data: &Path) -> usize {
    let mut removed = 0;
    for dir in targets(home, &|_| true).into_iter().chain(redundant(home)) {
        let link = dir.join(ODIN_NAME);
        let gone = match fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::read_link(&link).is_ok_and(|target| target.ends_with("odin/skills/odin"))
                    && fs::remove_file(&link).is_ok()
            }
            Ok(_) => link.join(ODIN_MARKER).is_file() && fs::remove_dir_all(&link).is_ok(),
            Err(_) => false,
        };
        removed += usize::from(gone);
    }
    let old = data.join("odin/skills/odin");
    if old.join(ODIN_MARKER).is_file() && fs::remove_dir_all(&old).is_ok() {
        // The folders around it, when nothing else is in them.
        let _ = fs::remove_dir(data.join("odin/skills"));
        let _ = fs::remove_dir(data.join("odin"));
    }
    removed
}

/// The full manual, for `:docs` and `$RAIDO_DOCS`.
pub fn manual() -> PathBuf {
    source().join("reference/manual.md")
}

/// Skills folders to link into: the shared one, plus Claude Code's and Codex's own
/// folders when those agents are set up (Raido never creates config for tools you
/// don't use). Agents that read `~/.agents/skills` need nothing else.
/// Agent CLIs, and the settings folders that show one has been used.
const AGENT_PROGRAMS: &[&str] = &["claude", "codex", "gemini", "agy", "opencode", "copilot"];
const AGENT_HOMES: &[&str] = &[
    ".claude",
    ".codex",
    ".gemini",
    ".copilot",
    ".config/opencode",
];

/// Whether any AI agent is installed or has been used here. Without one, Raido
/// leaves the agents' folders alone.
pub fn agents_present() -> bool {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return false;
    };
    AGENT_PROGRAMS
        .iter()
        .any(|p| crate::platform::which(p).is_some())
        || AGENT_HOMES.iter().any(|d| home.join(d).is_dir())
}

/// Skill folders to link into. `installed` says whether an agent's program is on
/// PATH: a fresh install of Claude Code or Codex has no settings folder until its
/// first run, and its first session should already find the skill.
fn targets(home: &Path, installed: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".agents/skills")];
    for (program, agent_home, skills) in [
        ("claude", ".claude", ".claude/skills"),
        ("codex", ".codex", ".codex/skills"),
    ] {
        if home.join(agent_home).is_dir() || installed(program) {
            dirs.push(home.join(skills));
        }
    }
    dirs
}

fn on_path(program: &str) -> bool {
    crate::platform::which(program).is_some()
}

/// Folders that also read `~/.agents/skills`: a Raido link here would duplicate the
/// skill (Gemini CLI reports a conflict), so Raido removes its own links from them.
fn redundant(home: &Path) -> Vec<PathBuf> {
    [
        ".gemini/skills",
        ".copilot/skills",
        ".config/opencode/skills",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect()
}

/// Remove Raido's own link or copy at `dir/raido`; returns whether there was one.
fn unlink(dir: &Path, source: &Path) -> Result<bool> {
    let link = dir.join(NAME);
    match link_state(&link, source) {
        State::Ours => fs::remove_file(&link)?,
        State::OursCopy => fs::remove_dir_all(&link)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// Write the skill files into `dir` (only what changed). Returns whether anything did.
fn write_files(dir: &Path) -> Result<bool> {
    let mut changed = false;
    for (name, content) in FILES
        .iter()
        .copied()
        .chain([(MARKER, env!("CARGO_PKG_VERSION"))])
    {
        let path = dir.join(name);
        if fs::read_to_string(&path).ok().as_deref() != Some(content) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content).with_context(|| format!("Write {}", path.display()))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Make sure the skill files exist in Raido's data folder.
pub fn write_source() -> Result<PathBuf> {
    let dir = source();
    write_files(&dir)?;
    Ok(dir)
}

/// Install or update the skill for every agent; returns the folders it is visible in.
pub fn install() -> Result<Vec<PathBuf>> {
    // Tests launch agents; they must never touch the real home folders.
    if cfg!(test) {
        return Ok(Vec::new());
    }
    let source = write_source()?;
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("No home directory")?;
    retire_odin(&home, &data_dir());
    for dir in redundant(&home) {
        unlink(&dir, &source)?;
    }
    install_into(&source, &targets(&home, &on_path))
}

fn install_into(source: &Path, targets: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut installed = Vec::new();
    for dir in targets {
        let link = dir.join(NAME);
        match link_state(&link, source) {
            State::Ours => {}
            State::OursCopy => {
                write_files(&link)?;
            }
            // Someone else's `raido` skill: leave it alone.
            State::Foreign => continue,
            State::Missing => {
                fs::create_dir_all(dir)?;
                place(source, &link)?;
            }
        }
        installed.push(link);
    }
    Ok(installed)
}

enum State {
    Ours,
    OursCopy,
    Foreign,
    Missing,
}

fn link_state(link: &Path, source: &Path) -> State {
    match fs::symlink_metadata(link) {
        Err(_) => State::Missing,
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(link) {
            Ok(target) if target == source => State::Ours,
            // A dangling link to an older Raido location is ours to refresh.
            Ok(target) if target.ends_with("raido/skills/raido") => {
                let _ = fs::remove_file(link);
                State::Missing
            }
            _ => State::Foreign,
        },
        Ok(_) if link.join(MARKER).is_file() => State::OursCopy,
        Ok(_) => State::Foreign,
    }
}

#[cfg(unix)]
fn place(source: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, link).with_context(|| format!("Link {}", link.display()))
}

/// Windows symlinks need extra rights, so agents get a copy there.
#[cfg(not(unix))]
fn place(_source: &Path, link: &Path) -> Result<()> {
    write_files(link).map(|_| ())
}

/// Remove every link or copy Raido made, and its own folder.
pub fn remove() -> Result<usize> {
    if cfg!(test) {
        return Ok(0);
    }
    let source = source();
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("No home directory")?;
    let mut removed = retire_odin(&home, &data_dir());
    // Every place Raido could have linked it, whatever is installed now.
    for dir in targets(&home, &|_| true)
        .into_iter()
        .chain(redundant(&home))
    {
        removed += usize::from(unlink(&dir, &source)?);
    }
    if source.join(MARKER).is_file() {
        fs::remove_dir_all(&source)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_has_agent_skills_frontmatter_and_the_manual() {
        let skill = FILES[0].1;
        assert!(skill.starts_with("---\nname: raido\ndescription:"));
        assert!(skill.contains(":font"));
        assert!(FILES
            .iter()
            .any(|(n, c)| *n == "reference/manual.md" && c.contains("Raido")));
    }

    #[test]
    fn a_freshly_installed_agent_gets_the_skill_before_its_first_run() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        // Claude Code is on PATH but has never run: no ~/.claude yet.
        let claude = |program: &str| program == "claude";
        let dirs = targets(home, &claude);
        assert!(dirs.contains(&home.join(".claude/skills")));
        assert!(!dirs.contains(&home.join(".codex/skills")));
        let source = home.join("data/raido/skills/raido");
        write_files(&source).unwrap();
        install_into(&source, &dirs).unwrap();
        assert!(home.join(".claude/skills/raido/SKILL.md").is_file());
        // No agent at all: only the shared folder.
        assert_eq!(targets(home.join("empty").as_path(), &|_| false).len(), 1);
    }

    #[test]
    fn installs_for_present_agents_and_respects_foreign_skills() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".gemini")).unwrap();
        fs::create_dir_all(home.join(".codex/skills/raido")).unwrap(); // someone else's
        fs::write(home.join(".codex/skills/raido/SKILL.md"), "mine").unwrap();
        let source = home.join("data/raido/skills/raido");
        write_files(&source).unwrap();
        let none = |_: &str| false;
        let dirs = targets(home, &none);
        // .agents always; .claude and .codex exist; Gemini reads .agents, so no link.
        assert!(dirs.contains(&home.join(".agents/skills")));
        assert!(dirs.contains(&home.join(".claude/skills")));
        assert!(!dirs.contains(&home.join(".gemini/skills")));
        let installed = install_into(&source, &dirs).unwrap();
        assert!(installed.contains(&home.join(".claude/skills/raido")));
        assert!(!installed.contains(&home.join(".codex/skills/raido")));
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/raido/SKILL.md")).unwrap(),
            "mine"
        );
        let skill = fs::read_to_string(home.join(".agents/skills/raido/SKILL.md")).unwrap();
        assert!(skill.contains("name: raido"));
        assert!(home
            .join(".claude/skills/raido/reference/manual.md")
            .is_file());
        // Running again is a no-op that still reports the same places.
        assert_eq!(install_into(&source, &dirs).unwrap(), installed);
        assert!(!write_files(&source).unwrap());
        // A link left in a folder that also reads ~/.agents/skills is removed.
        install_into(&source, &[home.join(".gemini/skills")]).unwrap();
        assert!(unlink(&home.join(".gemini/skills"), &source).unwrap());
        assert!(!home.join(".gemini/skills/raido").exists());
        assert!(
            !unlink(&home.join(".codex/skills"), &source).unwrap(),
            "never someone else's"
        );
    }

    #[test]
    fn the_skill_installed_as_odin_is_retired_but_not_someone_elses() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let data = home.join("data");
        // Odin's data folder, a link to it, a copy (Windows), and a foreign skill.
        let old = data.join("odin/skills/odin");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(ODIN_MARKER), "0.1.0").unwrap();
        fs::create_dir_all(home.join(".agents/skills")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&old, home.join(".agents/skills/odin")).unwrap();
        fs::create_dir_all(home.join(".claude/skills/odin")).unwrap();
        fs::write(home.join(".claude/skills/odin").join(ODIN_MARKER), "0.1.0").unwrap();
        fs::create_dir_all(home.join(".codex/skills/odin")).unwrap();
        fs::write(home.join(".codex/skills/odin/SKILL.md"), "the language's").unwrap();
        let removed = retire_odin(home, &data);
        assert_eq!(removed, if cfg!(unix) { 2 } else { 1 });
        assert!(fs::symlink_metadata(home.join(".agents/skills/odin")).is_err());
        assert!(!home.join(".claude/skills/odin").exists());
        assert!(home.join(".codex/skills/odin/SKILL.md").is_file());
        assert!(!data.join("odin").exists());
    }
}
