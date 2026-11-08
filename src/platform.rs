//! The few places Linux, macOS and Windows differ: finding executables, the
//! default shell, where user configuration lives, and what keys are called; and
//! the one way terminals differ that Raido draws around.
use std::path::{Path, PathBuf};

const MAC: bool = cfg!(target_os = "macos");

/// The terminal's own copy shortcut, for hints: ⌘C on a Mac, Ctrl-Shift-C on Linux
/// and in Windows Terminal.
pub const COPY_KEY: &str = if MAC { "⌘C" } else { "Ctrl-Shift-C" };
/// The Alt key as the keyboard labels it.
pub const ALT_KEY: &str = if MAC { "Option" } else { "Alt" };
/// The logo key as the keyboard labels it.
pub const SUPER_KEY: &str = if MAC {
    "Cmd"
} else if cfg!(windows) {
    "Win"
} else {
    "Super"
};
/// The guide's line for moving by word and line: Option and Cmd arrows on a Mac,
/// Ctrl arrows and Home/End elsewhere.
pub const WORD_KEYS: (&str, &str) = if MAC {
    (
        "Option-←/→ · Option-⌫ · Cmd-arrows",
        "Word left/right (⇧ selects) · delete word · line/file ends",
    )
} else {
    (
        "Ctrl-←/→ · Alt-⌫ · Home/End",
        "Word left/right (⇧ selects) · delete word · line ends",
    )
};

/// Full path of `program` if it can be run: a path that exists, or a name found
/// on PATH. On Windows the PATHEXT extensions (`.exe`, `.cmd`, …) are tried too,
/// so npm-installed tools such as `pyright-langserver.cmd` are found.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        return with_extensions(path).into_iter().find(|p| p.is_file());
    }
    let dirs = std::env::var_os("PATH")?;
    std::env::split_paths(&dirs)
        .flat_map(|dir| with_extensions(&dir.join(program)))
        .find(|p| p.is_file())
}

/// The files `path` may name: itself, or on Windows, when it has no extension, only
/// its PATHEXT names (`opencode.cmd`). Windows can't run an extension-less file, such
/// as the shell script npm installs beside each `.cmd` shim, so that one never counts.
fn with_extensions(path: &Path) -> Vec<PathBuf> {
    if !cfg!(windows) || path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|ext| {
            let mut name = path.as_os_str().to_owned();
            name.push(ext.to_ascii_lowercase());
            PathBuf::from(name)
        })
        .collect()
}

/// Program to run: resolved to a full path when found, so Windows `.cmd` shims work.
pub fn resolve(program: &str) -> String {
    which(program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}

/// `dir` as programs can start in it. Windows' canonical paths begin `\\?\`, and
/// cmd.exe, which runs every `.cmd` shim (npm-installed CLIs, language servers),
/// can't start in one: it starts in C:\Windows instead. The same place written the
/// usual way (`C:\…`, `\\server\share\…`) works for every program; a path too long
/// to be written so stays as it is. Elsewhere `dir` is already fine.
pub fn startable(dir: &Path) -> PathBuf {
    if !cfg!(windows) {
        return dir.to_path_buf();
    }
    let Some(rest) = dir.to_str().and_then(|d| d.strip_prefix(r"\\?\")) else {
        return dir.to_path_buf();
    };
    let usual = if let Some(share) = rest.strip_prefix(r"UNC\") {
        format!(r"\\{share}")
    } else if rest.as_bytes().get(1) == Some(&b':') {
        rest.to_string()
    } else {
        return dir.to_path_buf();
    };
    // Windows' limit for a current directory, with room for its trailing `\`.
    if usual.len() < 259 {
        PathBuf::from(usual)
    } else {
        dir.to_path_buf()
    }
}

/// The interactive shell for terminal panes: the configured one, else `$SHELL`
/// (`/bin/sh` fallback) on Unix, or PowerShell (`cmd.exe` fallback) on Windows.
pub fn shell(configured: Option<&str>) -> (String, Vec<String>) {
    if cfg!(windows) {
        let program = configured.map(String::from).unwrap_or_else(|| {
            ["pwsh", "powershell"]
                .iter()
                .find(|p| which(p).is_some())
                .map(|p| p.to_string())
                .or_else(|| std::env::var("COMSPEC").ok())
                .unwrap_or_else(|| "cmd.exe".into())
        });
        let powershell = program.to_ascii_lowercase().contains("pwsh")
            || program.to_ascii_lowercase().contains("powershell");
        let args = if powershell {
            vec!["-NoLogo".into()]
        } else {
            Vec::new()
        };
        return (program, args);
    }
    let program = configured
        .map(String::from)
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".into());
    (program, vec!["-i".into()])
}

/// Base directory for Raido's config: `$XDG_CONFIG_HOME` if set, else
/// `~/.config` on Linux and macOS, `%APPDATA%` on Windows.
/// Whether the terminal colors underlines (SGR 58), for the editor's problem marks.
/// One that doesn't know the code reads its numbers as other styles, `2` as faint,
/// and dims text all over the screen (macOS Terminal.app), so only terminals known
/// to support it get colored underlines; the rest get plain ones.
pub fn colored_underlines() -> bool {
    static KNOWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KNOWN.get_or_init(|| underline_colors(|name| std::env::var(name).ok()))
}

fn underline_colors(var: impl Fn(&str) -> Option<String>) -> bool {
    let program = var("TERM_PROGRAM").unwrap_or_default();
    let term = var("TERM").unwrap_or_default();
    matches!(
        program.as_str(),
        "iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "tmux"
    ) || ["kitty", "ghostty", "wezterm", "foot"]
        .iter()
        .any(|name| term.contains(name))
        || var("KITTY_WINDOW_ID").is_some()
}

pub fn config_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    let dirs = directories::BaseDirs::new();
    let base = if cfg!(windows) {
        dirs.map(|d| d.config_dir().to_path_buf())
    } else {
        dirs.map(|d| d.home_dir().join(".config"))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_path_and_rejects_missing_ones() {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which(shell).is_some(), "{shell} should be on PATH");
        assert!(which("raido-definitely-missing-tool").is_none());
        assert!(which("").is_none());
        assert_eq!(
            resolve("raido-definitely-missing-tool"),
            "raido-definitely-missing-tool"
        );
    }

    #[test]
    fn programs_start_in_the_project_written_the_usual_way() {
        let plain = Path::new("/home/me/project");
        assert_eq!(startable(plain), plain);
        if cfg!(windows) {
            let s = |p: &str| startable(Path::new(p));
            assert_eq!(
                s(r"\\?\C:\Users\me\project"),
                Path::new(r"C:\Users\me\project")
            );
            assert_eq!(
                s(r"\\?\UNC\server\share\project"),
                Path::new(r"\\server\share\project")
            );
            assert_eq!(s(r"C:\Users\me\project"), Path::new(r"C:\Users\me\project"));
            // Not a drive or a share (a volume GUID): left alone, as is a path too
            // long to be written without the prefix.
            let volume = r"\\?\Volume{0b1c}\project";
            assert_eq!(s(volume), Path::new(volume));
            let long = format!(r"\\?\C:\{}", "d".repeat(300));
            assert_eq!(s(&long), Path::new(&long));
            // What Raido's root is: a canonical path.
            let here = std::env::current_dir().unwrap().canonicalize().unwrap();
            assert!(here.to_string_lossy().starts_with(r"\\?\"));
            assert!(!startable(&here).to_string_lossy().starts_with(r"\\?\"));
            assert!(startable(&here).is_dir());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_runs_the_npm_cmd_shim_not_the_shell_script_beside_it() {
        // What `npm install -g` leaves in %APPDATA%\npm for each CLI.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("tool.cmd"), "@echo off\n").unwrap();
        let tool = dir.path().join("tool");
        assert_eq!(
            which(tool.to_str().unwrap()),
            Some(dir.path().join("tool.cmd"))
        );
        std::fs::remove_file(dir.path().join("tool.cmd")).unwrap();
        assert_eq!(
            which(tool.to_str().unwrap()),
            None,
            "a shell script alone can't run"
        );
    }

    #[test]
    fn only_terminals_known_to_color_underlines_get_colored_ones() {
        let with = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            underline_colors(move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
        };
        // macOS Terminal.app reads SGR 58's numbers as other styles (2 is faint).
        assert!(!with(&[
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM", "xterm-256color")
        ]));
        assert!(!with(&[("TERM", "xterm-256color")]), "unknown: plain");
        assert!(with(&[("TERM_PROGRAM", "iTerm.app")]));
        assert!(with(&[("TERM_PROGRAM", "ghostty")]));
        assert!(with(&[("TERM", "xterm-kitty")]));
        assert!(with(&[("TERM", "foot")]));
    }

    #[test]
    fn shell_honours_configuration() {
        let (program, _) = shell(Some("my-shell"));
        assert_eq!(program, "my-shell");
        let (program, args) = shell(None);
        assert!(!program.is_empty());
        if cfg!(unix) {
            assert_eq!(args, vec!["-i"]);
        }
    }
}
