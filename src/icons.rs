//! File-type icons for the explorer: Nerd Font glyphs with each language's usual
//! color, the same set Omarchy's terminal tools (Neovim, eza, yazi) show. They need a
//! Nerd Font in the terminal (Omarchy ships JetBrainsMono Nerd Font). Without one,
//! `icons = true` falls back to [`basic`] markers in the same colors.
use ratatui::style::Color;
use std::path::{Path, PathBuf};

/// `(glyph, color)`; `None` color means "use the theme's text color".
pub type Icon = (&'static str, Option<Color>);

const fn rgb(hex: u32) -> Option<Color> {
    Some(Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8))
}

pub fn folder(open: bool, name: &str) -> Icon {
    match name {
        ".git" => ("\u{e5fb}", rgb(0xf14c28)),
        ".github" => ("\u{e5fd}", None),
        ".config" => ("\u{e5fc}", None),
        "node_modules" => ("\u{e5fa}", rgb(0xe8274b)),
        _ if open => ("\u{f07c}", None),
        _ => ("\u{f07b}", None),
    }
}

pub fn file(path: &Path) -> Icon {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // Whole-name matches first: they beat the extension.
    let by_name = match name.to_ascii_lowercase().as_str() {
        "cargo.toml" | "cargo.lock" => Some(("\u{e7a8}", rgb(0xdea584))),
        "dockerfile" | "containerfile" | "docker-compose.yml" | "compose.yaml" => {
            Some(("\u{f0868}", rgb(0x458ee6)))
        }
        "makefile" | "gnumakefile" | "justfile" => Some(("\u{e779}", rgb(0x6d8086))),
        "license" | "license.md" | "license.txt" | "copying" => Some(("\u{e60a}", rgb(0xd0bf41))),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" => {
            Some(("\u{e702}", rgb(0xf14c28)))
        }
        ".env" | ".envrc" => Some(("\u{f462}", rgb(0xfaf743))),
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => Some(("\u{e795}", rgb(0x89e051))),
        "readme" | "readme.md" => Some(("\u{f48a}", rgb(0x42a5f5))),
        _ => None,
    };
    if let Some(icon) = by_name {
        return icon;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => ("\u{e7a8}", rgb(0xdea584)),
        "c" => ("\u{e61e}", rgb(0x599eff)),
        "h" => ("\u{f0fd}", rgb(0xa074c4)),
        "cpp" | "cc" | "cxx" | "c++" => ("\u{e61d}", rgb(0xf34b7d)),
        "hpp" | "hh" | "hxx" | "h++" => ("\u{f0fd}", rgb(0xa074c4)),
        "py" | "pyi" => ("\u{e606}", rgb(0xffbc03)),
        "go" => ("\u{e627}", rgb(0x00add8)),
        "js" | "mjs" | "cjs" => ("\u{e74e}", rgb(0xcbcb41)),
        "jsx" => ("\u{e7ba}", rgb(0x20c2e3)),
        "ts" | "mts" | "cts" => ("\u{e628}", rgb(0x519aba)),
        "tsx" => ("\u{e7ba}", rgb(0x1354bf)),
        "json" | "jsonc" | "json5" => ("\u{e60b}", rgb(0xcbcb41)),
        "toml" => ("\u{e6b2}", rgb(0x9c4221)),
        "yml" | "yaml" => ("\u{e6a8}", rgb(0x6d8086)),
        "md" | "markdown" | "mdx" => ("\u{e609}", rgb(0xdddddd)),
        "html" | "htm" => ("\u{e736}", rgb(0xe44d26)),
        "css" => ("\u{e749}", rgb(0x42a5f5)),
        "scss" | "sass" => ("\u{e603}", rgb(0xf55385)),
        "sh" | "bash" | "zsh" | "fish" => ("\u{e795}", rgb(0x89e051)),
        "lua" => ("\u{e620}", rgb(0x51a0cf)),
        "vim" => ("\u{e62b}", rgb(0x019833)),
        "java" => ("\u{e738}", rgb(0xcc3e44)),
        "kt" | "kts" => ("\u{e634}", rgb(0x7f52ff)),
        "swift" => ("\u{e755}", rgb(0xe37933)),
        "rb" => ("\u{e739}", rgb(0x701516)),
        "php" => ("\u{e73d}", rgb(0xa074c4)),
        "cs" => ("\u{f031b}", rgb(0x596706)),
        "zig" => ("\u{e6a9}", rgb(0xf69a1b)),
        "hs" => ("\u{e777}", rgb(0xa074c4)),
        "ex" | "exs" => ("\u{e62d}", rgb(0xa074c4)),
        "nix" => ("\u{f313}", rgb(0x7ebae4)),
        "sql" => ("\u{e706}", rgb(0xdad8d8)),
        "vue" => ("\u{e6a0}", rgb(0x8dc149)),
        "svelte" => ("\u{e697}", rgb(0xff3e00)),
        "tf" | "tfvars" => ("\u{e69a}", rgb(0x5f43e9)),
        "lock" => ("\u{f023}", rgb(0xbbbbbb)),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" => {
            ("\u{e60d}", rgb(0xa074c4))
        }
        "svg" => ("\u{f0721}", rgb(0xffb13b)),
        "pdf" => ("\u{e67d}", rgb(0xb30b00)),
        "zip" | "tar" | "gz" | "tgz" | "xz" | "zst" | "bz2" | "7z" | "rar" => {
            ("\u{f410}", rgb(0xeca517))
        }
        "txt" | "text" => ("\u{f0219}", rgb(0x89e051)),
        "log" => ("\u{f0331}", rgb(0xdddddd)),
        "conf" | "cfg" | "ini" => ("\u{e615}", rgb(0x6d8086)),
        "xml" => ("\u{f05c0}", rgb(0xe37933)),
        "csv" | "tsv" => ("\u{e64a}", rgb(0x89e051)),
        "mp3" | "flac" | "wav" | "ogg" => ("\u{f0386}", rgb(0x66d8ef)),
        "mp4" | "mkv" | "webm" | "mov" => ("\u{f03d}", rgb(0xfd971f)),
        _ => ("\u{f15b}", None),
    }
}

/// The portable stand-in for a Nerd Font icon: a dot in the language's color for
/// files and a square for folders, drawn by every monospace font.
pub fn basic((glyph, tint): Icon, directory: bool, open: bool) -> Icon {
    let glyph = match (directory, open) {
        (true, true) => "□",
        (true, false) => "■",
        // The generic file icon has no language color: keep it hollow.
        (false, _) if glyph == "\u{f15b}" => "○",
        (false, _) => "●",
    };
    (glyph, tint)
}

/// Whether this terminal can draw Nerd Font glyphs: it bundles them (kitty, WezTerm,
/// Ghostty), it runs on Omarchy, or a Nerd Font is installed. An installed font may
/// still not be the terminal's font; `icons = "nerd"`/`"basic"` overrides the guess.
pub fn nerd_font_available() -> bool {
    let env = |key: &str| std::env::var(key).unwrap_or_default();
    let bundled = matches!(env("TERM_PROGRAM").as_str(), "WezTerm" | "ghostty")
        || env("TERM") == "xterm-kitty"
        || std::env::var_os("KITTY_WINDOW_ID").is_some();
    bundled
        || std::env::var_os("OMARCHY_PATH").is_some()
        || font_dirs().iter().any(|d| has_nerd_font(d, 3))
}

/// Whether a font family is installed: fontconfig's list where it exists (Linux),
/// else font files whose names start with the family, ignoring spaces and case.
pub(crate) fn font_installed(family: &str) -> bool {
    let wanted = squash(family);
    if wanted.is_empty() {
        return false;
    }
    if !cfg!(target_os = "macos") {
        let listed = std::process::Command::new("fc-list")
            .args([":", "family"])
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(output) = listed {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .flat_map(|line| line.split(','))
                    .any(|name| squash(name) == wanted);
            }
        }
    }
    font_dirs().iter().any(|d| has_font_file(d, &wanted, 3))
}

/// `JetBrainsMono Nerd Font Mono` → `jetbrainsmononerdfontmono`.
fn squash(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn has_font_file(dir: &Path, wanted: &str, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return depth > 0 && has_font_file(&path, wanted, depth - 1);
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        [".ttf", ".otf", ".ttc"]
            .iter()
            .any(|ext| name.ends_with(ext))
            && squash(&name).starts_with(wanted)
    })
}

fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend(home.map(|h| h.join("Library/Fonts")));
        dirs.push("/Library/Fonts".into());
        dirs.push("/System/Library/Fonts".into());
    } else if cfg!(windows) {
        dirs.extend(
            std::env::var_os("LOCALAPPDATA")
                .map(|d| PathBuf::from(d).join("Microsoft/Windows/Fonts")),
        );
        dirs.extend(std::env::var_os("WINDIR").map(|d| PathBuf::from(d).join("Fonts")));
    } else {
        if let Some(home) = home {
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
        dirs.push("/usr/share/fonts".into());
        dirs.push("/usr/local/share/fonts".into());
    }
    dirs
}

/// A font file named like a Nerd Font ("JetBrainsMonoNerdFont-Regular.ttf",
/// "Hack Nerd Font.ttf") within `depth` levels of `dir`.
fn has_nerd_font(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return depth > 0 && has_nerd_font(&path, depth - 1);
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let font = [".ttf", ".otf", ".ttc"]
            .iter()
            .any(|ext| name.ends_with(ext));
        font && name.replace([' ', '-', '_'], "").contains("nerdfont")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_fonts_are_found_by_family_name() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("JetBrainsMono");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("JetBrainsMonoNerdFontMono-Regular.ttf"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        let wanted = squash("JetBrainsMono Nerd Font Mono");
        assert_eq!(wanted, "jetbrainsmononerdfontmono");
        assert!(has_font_file(dir.path(), &wanted, 2));
        assert!(has_font_file(dir.path(), &squash("JetBrains Mono"), 2));
        assert!(!has_font_file(dir.path(), &squash("Fira Code"), 2));
        assert!(
            !has_font_file(dir.path(), &squash("notes"), 2),
            "only font files count"
        );
        assert!(!font_installed(""));
        assert!(!font_installed("No Such Font Raido Test"));
    }

    #[test]
    fn common_files_get_their_language_icon() {
        assert_eq!(file(Path::new("src/main.rs")).0, "\u{e7a8}");
        assert_eq!(file(Path::new("Cargo.toml")).0, "\u{e7a8}");
        assert_eq!(file(Path::new("x.py")).0, "\u{e606}");
        assert_eq!(file(Path::new(".gitignore")).0, "\u{e702}");
        assert_eq!(file(Path::new("README.md")).0, "\u{f48a}");
        assert_eq!(file(Path::new("unknown.zzz")), ("\u{f15b}", None));
        assert_eq!(folder(true, "src").0, "\u{f07c}");
        assert_eq!(folder(false, ".git").0, "\u{e5fb}");
    }

    #[test]
    fn basic_icons_keep_the_language_color() {
        let rust = file(Path::new("main.rs"));
        assert_eq!(basic(rust, false, false), ("●", rust.1));
        assert_eq!(basic(file(Path::new("x.zzz")), false, false), ("○", None));
        assert_eq!(basic(folder(false, "src"), true, false).0, "■");
        assert_eq!(basic(folder(true, "src"), true, true).0, "□");
    }

    #[test]
    fn nerd_fonts_are_found_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Menlo.ttc"), "").unwrap();
        assert!(!has_nerd_font(dir.path(), 3));
        let nested = dir.path().join("TTF");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("JetBrainsMonoNerdFont-Regular.ttf"), "").unwrap();
        assert!(has_nerd_font(dir.path(), 3));
        assert!(!has_nerd_font(dir.path(), 0));
    }
}
