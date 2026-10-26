//! Fonts belong to the terminal Raido runs in, so `:font` works through it: it saves
//! a size or family into the terminal's config file (foot, Ghostty, Kitty,
//! Alacritty), uses `omarchy font set` on Omarchy, and otherwise explains where the
//! setting lives. Live zoom is `textsize` (Ctrl-b + / - / 0).
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emulator {
    Foot,
    Ghostty,
    Kitty,
    Alacritty,
    WezTerm,
    WindowsTerminal,
    Xterm,
    Apple,
    Unknown,
}

impl Emulator {
    pub fn name(self) -> &'static str {
        match self {
            Emulator::Foot => "foot",
            Emulator::Ghostty => "Ghostty",
            Emulator::Kitty => "Kitty",
            Emulator::Alacritty => "Alacritty",
            Emulator::WezTerm => "WezTerm",
            Emulator::WindowsTerminal => "Windows Terminal",
            Emulator::Xterm => "xterm",
            Emulator::Apple => "your macOS terminal",
            Emulator::Unknown => "your terminal",
        }
    }
    /// The config file Raido can edit for a lasting change.
    fn config(self) -> Option<PathBuf> {
        let base = crate::platform::config_base();
        match self {
            Emulator::Foot => Some(base.join("foot/foot.ini")),
            Emulator::Ghostty => Some(base.join("ghostty/config")),
            Emulator::Kitty => Some(base.join("kitty/kitty.conf")),
            Emulator::Alacritty => Some(base.join("alacritty/alacritty.toml")),
            _ => None,
        }
    }
    /// How to apply a saved config change.
    fn reload(self) -> &'static str {
        match self {
            Emulator::Foot => "open a new foot window to see it",
            Emulator::Ghostty => "press Ctrl+Shift+, in Ghostty to reload",
            Emulator::Kitty => "press Ctrl+Shift+F5 in Kitty to reload",
            Emulator::Alacritty => "Alacritty reloads it automatically",
            _ => "restart the terminal",
        }
    }
    fn manual(self) -> &'static str {
        match self {
            Emulator::WezTerm => "set config.font / config.font_size in ~/.wezterm.lua",
            Emulator::WindowsTerminal => "Settings → your profile → Appearance → Font face / size",
            Emulator::Apple => "the terminal's Settings → Profiles → Text → Font",
            Emulator::Xterm => "set XTerm*faceName / XTerm*faceSize in ~/.Xresources",
            _ => "change it in your terminal's settings",
        }
    }
}

/// Which terminal Raido is running in: environment hints first, then (Linux) the
/// name of the closest ancestor process that is a known terminal.
pub fn detect() -> Emulator {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    from_hints(
        |k| std::env::var_os(k).is_some(),
        &env("TERM_PROGRAM"),
        &env("TERM"),
        &ancestors(),
    )
}

fn from_hints(
    has: impl Fn(&str) -> bool,
    term_program: &str,
    term: &str,
    ancestors: &[String],
) -> Emulator {
    let program = term_program.to_ascii_lowercase();
    if has("KITTY_WINDOW_ID") || term == "xterm-kitty" {
        return Emulator::Kitty;
    }
    if has("ALACRITTY_WINDOW_ID") || has("ALACRITTY_SOCKET") || term == "alacritty" {
        return Emulator::Alacritty;
    }
    if has("GHOSTTY_RESOURCES_DIR") || program == "ghostty" || term == "xterm-ghostty" {
        return Emulator::Ghostty;
    }
    if has("WEZTERM_PANE") || program == "wezterm" {
        return Emulator::WezTerm;
    }
    if has("WT_SESSION") {
        return Emulator::WindowsTerminal;
    }
    if program == "iterm.app" || program == "apple_terminal" {
        return Emulator::Apple;
    }
    if term.starts_with("foot") {
        return Emulator::Foot;
    }
    for name in ancestors {
        match name.as_str() {
            "foot" | "footclient" => return Emulator::Foot,
            "ghostty" => return Emulator::Ghostty,
            "kitty" => return Emulator::Kitty,
            "alacritty" => return Emulator::Alacritty,
            "wezterm-gui" | "wezterm" => return Emulator::WezTerm,
            "xterm" => return Emulator::Xterm,
            _ => {}
        }
    }
    if has("XTERM_VERSION") {
        return Emulator::Xterm;
    }
    Emulator::Unknown
}

/// Names of parent processes, nearest first (Linux only; empty elsewhere).
fn ancestors() -> Vec<String> {
    let mut names = Vec::new();
    let mut pid = std::process::id().to_string();
    for _ in 0..16 {
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
            break;
        };
        let Some(parent) = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .map(|p| p.trim().to_string())
        else {
            break;
        };
        if parent == "0" || parent == "1" {
            break;
        }
        if let Ok(comm) = fs::read_to_string(format!("/proc/{parent}/comm")) {
            names.push(comm.trim().to_string());
        }
        pid = parent;
    }
    names
}

fn omarchy() -> bool {
    crate::platform::which("omarchy").is_some()
}

/// `:font` with no arguments: where the font comes from and how to change it.
pub fn summary(term: Emulator) -> String {
    let lasting = match term.config() {
        Some(_) => " · :font size 12 saves a size".to_string(),
        None => format!(" · to keep a size: {}", term.manual()),
    };
    let family = if omarchy() {
        " · :font family \"Name\" (via omarchy font set) · :font list"
    } else {
        " · :font family \"Name\" · :font list"
    };
    format!(
        "Font is set by {}: Ctrl-b + / - / 0 zooms{lasting}{family}",
        term.name()
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("Run {program}"))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// `:font size N`: save in the terminal's config (and apply live where possible).
pub fn set_size(term: Emulator, size: f32) -> Result<String> {
    if !(4.0..=96.0).contains(&size) {
        bail!("Pick a font size between 4 and 96");
    }
    let Some(path) = term.config() else {
        bail!(
            "Raido can't change {}'s font for you: {} · Ctrl-b + / - zooms now",
            term.name(),
            term.manual()
        );
    };
    edit(&path, |text| set_in(term, text, Some(size), None))?;
    if term == Emulator::Alacritty {
        let _ = run(
            "alacritty",
            &["msg", "config", &format!("font.size={size}")],
        );
    }
    if term == Emulator::Kitty {
        let _ = run("kitty", &["@", "set-font-size", "--", &size.to_string()]);
    }
    Ok(format!(
        "Font size {size} saved to {} · {}",
        path.display(),
        term.reload()
    ))
}

/// `:font family NAME`: Omarchy's own command when available, else the terminal config.
pub fn set_family(term: Emulator, family: &str) -> Result<String> {
    let family = family.trim().trim_matches('"');
    if family.is_empty() {
        bail!("Usage: :font family \"JetBrainsMono Nerd Font\" · :font list shows choices");
    }
    if omarchy() {
        run("omarchy", &["font", "set", family])?;
        return Ok(format!(
            "Omarchy font set to {family} for all terminals · {}",
            term.reload()
        ));
    }
    let Some(path) = term.config() else {
        bail!(
            "Raido can't change {}'s font for you: {}",
            term.name(),
            term.manual()
        );
    };
    edit(&path, |text| set_in(term, text, None, Some(family)))?;
    if term == Emulator::Alacritty {
        let _ = run(
            "alacritty",
            &["msg", "config", &format!("font.normal.family=\"{family}\"")],
        );
    }
    Ok(format!(
        "Font {family} saved to {} · {}",
        path.display(),
        term.reload()
    ))
}

/// Monospace font families installed on this system.
pub fn list() -> Result<Vec<String>> {
    let output = if omarchy() {
        Command::new("omarchy").args(["font", "list"]).output()
    } else {
        Command::new("fc-list")
            .args([":spacing=mono", "family"])
            .output()
    }
    .context("Listing fonts needs fontconfig (fc-list)")?;
    let mut fonts: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.split(',').next().unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    fonts.sort();
    fonts.dedup();
    Ok(fonts)
}

fn edit(path: &Path, change: impl Fn(&str) -> String) -> Result<()> {
    let old = fs::read_to_string(path).unwrap_or_default();
    let new = change(&old);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("raido-tmp");
    fs::write(&temporary, new)?;
    fs::rename(&temporary, path).with_context(|| format!("Write {}", path.display()))?;
    Ok(())
}

/// Change only the font lines of a terminal config, keeping everything else.
fn set_in(term: Emulator, text: &str, size: Option<f32>, family: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    match term {
        Emulator::Foot => {
            // font=Family:size=N inside [main] (or before any section).
            let start = lines
                .iter()
                .position(|l| l.trim() == "[main]")
                .map_or(0, |i| i + 1);
            let end = lines[start..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |e| start + e);
            let found = lines[start..end]
                .iter()
                .position(|l| l.trim_start().starts_with("font="))
                .map(|i| start + i);
            let current = found.map(|i| lines[i].trim_start()["font=".len()..].to_string());
            let mut fonts: Vec<String> = current
                .as_deref()
                .unwrap_or("monospace:size=10")
                .split(',')
                .map(String::from)
                .collect();
            let mut parts: Vec<String> = fonts[0].split(':').map(String::from).collect();
            if let Some(family) = family {
                parts[0] = family.to_string();
            }
            if let Some(size) = size {
                parts.retain(|p| !p.starts_with("size=") && !p.starts_with("pixelsize="));
                parts.push(format!("size={size}"));
            }
            fonts[0] = parts.join(":");
            let line = format!("font={}", fonts.join(","));
            match found {
                Some(i) => lines[i] = line,
                None => lines.insert(start, line),
            }
        }
        Emulator::Ghostty | Emulator::Kitty => {
            let (size_key, family_key, sep) = if term == Emulator::Ghostty {
                ("font-size", "font-family", " = ")
            } else {
                ("font_size", "font_family", " ")
            };
            let mut put = |key: &str, value: String| {
                let line = format!("{key}{sep}{value}");
                match lines.iter().position(|l| {
                    l.trim_start()
                        .strip_prefix(key)
                        .is_some_and(|rest| rest.starts_with([' ', '=', '\t']))
                }) {
                    Some(i) => lines[i] = line,
                    None => lines.push(line),
                }
            };
            if let Some(size) = size {
                put(size_key, size.to_string());
            }
            if let Some(family) = family {
                let value = if term == Emulator::Ghostty {
                    format!("\"{family}\"")
                } else {
                    family.to_string()
                };
                put(family_key, value);
            }
        }
        Emulator::Alacritty => {
            let mut put = |section: &str, key: &str, value: String| {
                let header = format!("[{section}]");
                let line = format!("{key} = {value}");
                match lines.iter().position(|l| l.trim() == header) {
                    Some(start) => {
                        let end = lines[start + 1..]
                            .iter()
                            .position(|l| l.trim_start().starts_with('['))
                            .map_or(lines.len(), |e| start + 1 + e);
                        match lines[start + 1..end].iter().position(|l| {
                            l.trim_start()
                                .strip_prefix(key)
                                .is_some_and(|r| r.trim_start().starts_with('='))
                        }) {
                            Some(i) => lines[start + 1 + i] = line,
                            None => lines.insert(start + 1, line),
                        }
                    }
                    None => {
                        lines.push(String::new());
                        lines.push(header);
                        lines.push(line);
                    }
                }
            };
            if let Some(size) = size {
                put("font", "size", size.to_string());
            }
            if let Some(family) = family {
                put("font.normal", "family", format!("\"{family}\""));
            }
        }
        _ => {}
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_terminals_from_environment_and_parents() {
        let none = |_: &str| false;
        assert_eq!(
            from_hints(|k| k == "KITTY_WINDOW_ID", "", "", &[]),
            Emulator::Kitty
        );
        assert_eq!(from_hints(none, "ghostty", "", &[]), Emulator::Ghostty);
        assert_eq!(from_hints(none, "", "foot-extra", &[]), Emulator::Foot);
        // foot with TERM=xterm-256color (Omarchy): found through the parent process.
        let parents = ["bash".to_string(), "foot".to_string()];
        assert_eq!(
            from_hints(none, "", "xterm-256color", &parents),
            Emulator::Foot
        );
        assert_eq!(
            from_hints(|k| k == "WT_SESSION", "", "", &[]),
            Emulator::WindowsTerminal
        );
        assert_eq!(
            from_hints(none, "", "xterm-256color", &[]),
            Emulator::Unknown
        );
    }

    #[test]
    fn foot_font_line_changes_size_and_family_only() {
        let ini = "[main]\ninclude=~/theme.ini\nfont=JetBrainsMono Nerd Font:size=9\n\n[cursor]\nstyle=beam\n";
        let out = set_in(Emulator::Foot, ini, Some(12.0), None);
        assert_eq!(out, ini.replace("size=9", "size=12"));
        let out = set_in(Emulator::Foot, ini, None, Some("Iosevka"));
        assert!(out.contains("font=Iosevka:size=9\n"));
        let out = set_in(Emulator::Foot, "[cursor]\nstyle=beam\n", Some(11.0), None);
        assert!(out.starts_with("font=monospace:size=11\n"), "{out}");
    }

    #[test]
    fn ghostty_kitty_and_alacritty_keys_are_updated_or_added() {
        let out = set_in(
            Emulator::Ghostty,
            "font-family = \"A\"\nfont-size = 9\n",
            Some(13.0),
            Some("B"),
        );
        assert_eq!(out, "font-family = \"B\"\nfont-size = 13\n");
        let out = set_in(
            Emulator::Kitty,
            "font_size 9.0\n",
            Some(14.0),
            Some("Fira Code"),
        );
        assert_eq!(out, "font_size 14\nfont_family Fira Code\n");
        let toml = "[window]\nopacity = 0.9\n\n[font]\nsize = 9\n";
        let out = set_in(Emulator::Alacritty, toml, Some(12.5), Some("Hack"));
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(parsed["font"]["size"].as_float(), Some(12.5));
        assert_eq!(parsed["font"]["normal"]["family"].as_str(), Some("Hack"));
        assert_eq!(parsed["window"]["opacity"].as_float(), Some(0.9));
    }

    #[test]
    fn unsupported_terminals_explain_instead_of_failing_silently() {
        assert!(set_size(Emulator::WezTerm, 12.0).is_err());
        assert!(set_size(Emulator::Foot, 200.0).is_err());
        assert!(summary(Emulator::Ghostty).contains("Ghostty"));
        assert!(summary(Emulator::Foot).contains("Ctrl-b +"));
    }
}
