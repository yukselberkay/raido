//! Omarchy desktop themes as Raido themes. Every Omarchy theme ships a `colors.toml`
//! with named colors (accent, background, foreground, red, …); Raido reads it
//! directly, so installed and custom Omarchy themes appear in the theme picker.
//!
//! - `omarchy-<name>`: a specific theme from `~/.config/omarchy/themes` (user,
//!   preferred) or the system's Omarchy install.
//! - `omarchy`: whatever theme Omarchy is currently using; it follows
//!   `omarchy theme set` live.
use crate::config::{SyntaxColors, Theme};
use anyhow::{Context, Result};
use std::{collections::BTreeSet, fs, path::Path, path::PathBuf};

pub const FOLLOW: &str = "omarchy";
pub const PREFIX: &str = "omarchy-";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Theme directories, user first so a customized copy wins.
fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home() {
        dirs.push(home.join(".config/omarchy/themes"));
    }
    if let Some(path) = std::env::var_os("OMARCHY_PATH") {
        dirs.push(PathBuf::from(path).join("themes"));
    }
    if let Some(home) = home() {
        dirs.push(home.join(".local/share/omarchy/themes"));
    }
    dirs.push(PathBuf::from("/usr/share/omarchy/themes"));
    dirs
}

fn current_dir() -> Option<PathBuf> {
    Some(home()?.join(".local/state/omarchy/current"))
}

/// Display name of the active Omarchy theme, e.g. "Tokyo Night".
pub fn current_name() -> Option<String> {
    let name = fs::read_to_string(current_dir()?.join("theme.name")).ok()?;
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

/// Raido names for every Omarchy theme found, plus `omarchy` when one is active.
pub fn themes() -> Vec<String> {
    let mut names = BTreeSet::new();
    for dir in theme_dirs() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let valid = name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if valid && path.join("colors.toml").is_file() {
                names.insert(format!("{PREFIX}{name}"));
            }
        }
    }
    let mut names: Vec<String> = names.into_iter().collect();
    if file(FOLLOW).is_some() {
        names.insert(0, FOLLOW.to_string());
    }
    names
}

/// The `colors.toml` behind a Raido theme name, if it is an Omarchy theme.
pub fn file(name: &str) -> Option<PathBuf> {
    if name == FOLLOW {
        return Some(current_dir()?.join("theme/colors.toml")).filter(|p| p.is_file());
    }
    let theme = name.strip_prefix(PREFIX)?;
    if theme.is_empty()
        || !theme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    theme_dirs()
        .into_iter()
        .map(|dir| dir.join(theme).join("colors.toml"))
        .find(|p| p.is_file())
}

pub fn load(path: &Path) -> Result<Theme> {
    let text = fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
    convert(&text).with_context(|| format!("Invalid Omarchy theme {}", path.display()))
}

fn convert(text: &str) -> Result<Theme> {
    let table: toml::Table = toml::from_str(text)?;
    let get = |key: &str| table.get(key).and_then(|v| v.as_str()).and_then(normalize);
    // Each color falls back through related keys, then to a neutral default.
    let pick = |keys: &[&str], fallback: &str| {
        keys.iter()
            .find_map(|k| get(k))
            .unwrap_or_else(|| fallback.to_string())
    };
    let light = table.get("mode").and_then(|v| v.as_str()) == Some("light");
    let background = pick(&["background"], if light { "#ffffff" } else { "#1e1e1e" });
    let foreground = pick(&["foreground"], if light { "#333333" } else { "#d4d4d4" });
    let comment = pick(&["dark_foreground", "muted"], &foreground);
    let red = pick(&["red"], "#cd3131");
    let green = pick(&["green"], "#0dbc79");
    let yellow = pick(&["yellow"], "#e5e510");
    let blue = pick(&["blue", "accent"], "#2472c8");
    let magenta = pick(&["magenta"], "#bc3fbc");
    let cyan = pick(&["cyan"], "#11a8cd");
    let orange = pick(&["orange", "bright_yellow", "yellow"], &yellow);
    let ansi = [
        pick(&["lighter_background", "dark_background"], &background),
        red.clone(),
        green.clone(),
        yellow.clone(),
        blue.clone(),
        magenta.clone(),
        cyan.clone(),
        foreground.clone(),
        comment.clone(),
        pick(&["bright_red"], &red),
        pick(&["bright_green"], &green),
        pick(&["bright_yellow"], &yellow),
        pick(&["bright_blue"], &blue),
        pick(&["bright_magenta"], &magenta),
        pick(&["bright_cyan"], &cyan),
        pick(&["bright_foreground", "light_foreground"], &foreground),
    ];
    Ok(Theme {
        accent: pick(&["accent", "blue"], &blue),
        secondary: cyan.clone(),
        warning: yellow,
        border: pick(&["muted", "lighter_background"], &comment),
        selection: pick(&["selection", "lighter_background"], &background),
        line_number: Some(comment.clone()),
        muted: comment.clone(),
        syntax_colors: Some(SyntaxColors {
            comment,
            keyword: magenta.clone(),
            control: Some(pick(&["bright_magenta"], &magenta)),
            function: blue,
            r#type: cyan,
            string: green,
            number: orange.clone(),
            constant: orange,
            operator: pick(&["bright_cyan", "cyan"], &foreground),
            variable: foreground.clone(),
        }),
        ansi: Some(ansi),
        background,
        foreground,
        ..Theme::default()
    })
}

/// `#rgb` / `#rrggbb` / `#rrggbbaa` → `#rrggbb`; anything else is ignored.
fn normalize(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full: String = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => hex[..6].to_string(),
        _ => return None,
    };
    Some(format!("#{}", full.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: &str = r##"mode = "dark"
accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
bright_foreground = "#c0caf5"
red = "#f7768e"
yellow = "#e0af68"
orange = "#eb927b"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
bright_magenta = "#bb9af7"
hyprland_active_border = "rgba(ffb000ee) rgba(33ff66ee) 45deg"
"##;

    #[test]
    fn colors_toml_maps_onto_an_raido_theme() {
        let theme = convert(TOKYO).unwrap();
        assert_eq!(theme.background, "#1a1b26");
        assert_eq!(theme.accent, "#7aa2f7");
        assert_eq!(theme.border, "#414868");
        assert_eq!(theme.muted, "#565f89");
        let syntax = theme.syntax_colors.as_ref().unwrap();
        assert_eq!(syntax.string, "#9ece6a");
        assert_eq!(syntax.keyword, "#ad8ee6");
        assert_eq!(syntax.number, "#eb927b");
        let ansi = theme.ansi.as_ref().unwrap();
        assert_eq!(ansi[0], "#24283b");
        assert_eq!(ansi[1], "#f7768e");
        // Missing bright red falls back to red; non-color values are ignored.
        assert_eq!(ansi[9], "#f7768e");
        assert_eq!(ansi[15], "#c0caf5");
    }

    #[test]
    fn sparse_or_light_themes_still_load() {
        let theme = convert("mode = \"light\"\naccent = \"#abc\"\n").unwrap();
        assert_eq!(theme.background, "#ffffff");
        assert_eq!(theme.accent, "#aabbcc");
        assert!(convert("not toml [").is_err());
    }

    #[test]
    fn names_resolve_only_with_the_prefix() {
        assert!(file("tokyo-night").is_none());
        assert!(file("omarchy-../../etc").is_none());
    }
}
