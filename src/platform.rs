//! The few places Linux, macOS and Windows differ: finding executables, the
//! default shell, where user configuration lives, and what keys are called.
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

fn with_extensions(path: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![path.to_path_buf()];
    if cfg!(windows) && path.extension().is_none() {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for ext in pathext.split(';').filter(|e| !e.is_empty()) {
            let mut name = path.as_os_str().to_owned();
            name.push(ext.to_ascii_lowercase());
            candidates.push(PathBuf::from(name));
        }
    }
    candidates
}

/// Program to run: resolved to a full path when found, so Windows `.cmd` shims work.
pub fn resolve(program: &str) -> String {
    which(program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
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
