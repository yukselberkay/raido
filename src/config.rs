use anyhow::{bail, Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub secondary: String,
    pub warning: String,
    pub border: String,
    pub selection: String,
    /// Optional editor gutter color; falls back to `muted`.
    pub line_number: Option<String>,
    pub syntax: String,
    pub syntax_colors: Option<SyntaxColors>,
    pub ansi: Option<[String; 16]>,
}

/// Optional semantic syntax palette. Older themes can keep using `syntax`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntaxColors {
    pub comment: String,
    pub keyword: String,
    pub function: String,
    pub r#type: String,
    pub string: String,
    pub number: String,
    pub constant: String,
    pub operator: String,
    pub variable: String,
    /// Optional control-flow keyword color (`if`, `return`); falls back to `keyword`.
    #[serde(default)]
    pub control: Option<String>,
}

pub const BUNDLED_THEMES: &[(&str, &str)] = &[
    (
        "visual-studio-dark",
        include_str!("../themes/visual-studio-dark.toml"),
    ),
    (
        "vscode-dark-2026",
        include_str!("../themes/vscode-dark-2026.toml"),
    ),
    ("dracula", include_str!("../themes/dracula.toml")),
    ("nord", include_str!("../themes/nord.toml")),
    ("one-dark", include_str!("../themes/one-dark.toml")),
    ("gruvbox-dark", include_str!("../themes/gruvbox-dark.toml")),
    ("tokyo-night", include_str!("../themes/tokyo-night.toml")),
    (
        "catppuccin-mocha",
        include_str!("../themes/catppuccin-mocha.toml"),
    ),
    (
        "solarized-dark",
        include_str!("../themes/solarized-dark.toml"),
    ),
    ("monokai", include_str!("../themes/monokai.toml")),
    ("github-dark", include_str!("../themes/github-dark.toml")),
];

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#0b1215".into(),
            foreground: "#c9d8cf".into(),
            muted: "#6f8b85".into(),
            accent: "#80efac".into(),
            secondary: "#8bbfd0".into(),
            warning: "#e6bd77".into(),
            border: "#29433f".into(),
            selection: "#172c2a".into(),
            line_number: None,
            syntax: "base16-ocean.dark".into(),
            syntax_colors: None,
            ansi: None,
        }
    }
}

pub fn color(value: &str) -> Color {
    let hex = value.strip_prefix('#').unwrap_or(value);
    u32::from_str_radix(hex, 16)
        .map(|v| Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
        .unwrap_or(Color::Reset)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPlugin {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// JetBrains Mono with Nerd Font icons, one cell wide.
pub const DEFAULT_FONT: &str = "JetBrainsMono Nerd Font Mono";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub theme: String,
    pub sidebar_width: u16,
    pub tab_width: usize,
    /// Enter indents after a line that opens a block and dedents after `return`;
    /// closers and Python's `else:` go back under their opener (see `language`).
    pub smart_indent: bool,
    /// Brackets and quotes close themselves; typing the closer steps over it.
    pub auto_pairs: bool,
    /// Completions pop up after two letters of a word: the language server's, or
    /// keywords and words from your open files when there is none.
    pub autocomplete: bool,
    pub shell: Option<String>,
    pub relative_numbers: bool,
    /// File-type icons in the explorer: `true` (auto), `"nerd"`, `"basic"` or `false`.
    pub icons: Icons,
    /// Show dotfiles (`.github`, `.gitignore`, …) in the explorer.
    /// Ignored: dotfiles are always shown. Accepted so older config files still load.
    #[allow(dead_code)]
    pub show_hidden: bool,
    /// Open the Git pane (branches and commit graph) at startup.
    pub git_pane: bool,
    /// Save each project's panes, buffers and terminals to `.raido/` and reopen them.
    pub restore_session: bool,
    /// Font for Raido's terminal window while Raido runs; `""` keeps the terminal's.
    /// Applied in Terminal.app when installed (see `textsize::WindowFont`).
    pub font: String,
    /// Font size in points, with `font`.
    pub font_size: f32,
    /// Where new AI panes open: "right" (side by side) or "below" (stacked).
    pub ai_split: String,
    /// Install the Raido skill (interactive docs) for AI agents when one is launched.
    pub ai_skill: bool,
    /// Sound when an AI pane finishes a prompt: "default", "off", "bell", a macOS
    /// sound name ("Glass", "Hero"…) or a sound file (see `sound::play`).
    pub ai_sound: String,
    /// Track the files AI panes change, to review, keep or undo (Ctrl-b v).
    pub review_changes: bool,
    /// Mouse wheel scrolls what's under the pointer and a click focuses it. Raido then
    /// owns the mouse: copy view (Ctrl-b Z) gives it back for selecting text.
    pub mouse: bool,
    /// Your own AI launchers: `[ai.<name>] program = "…" args = […] env = {…}`.
    /// `[ai.local]` is used by the launcher's "Local LLM" entry.
    pub ai: BTreeMap<String, crate::local_ai::Launcher>,
    pub plugins: BTreeMap<String, CommandPlugin>,
    /// Language servers (diagnostics, hover, definition, completion, format).
    pub lsp_enabled: bool,
    /// Languages whose files get their imports organized and are formatted by their
    /// language server on save, as VS Code does for Go (keys: go, rust, python, c).
    pub format_on_save: Vec<String>,
    /// Per-language server overrides: `[lsp.python] program = "pylsp"`.
    /// Keys: rust, go, python, c (C and C++).
    pub lsp: BTreeMap<String, CommandPlugin>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "phosphor".into(),
            sidebar_width: 29,
            tab_width: 4,
            smart_indent: true,
            auto_pairs: true,
            autocomplete: true,
            shell: None,
            relative_numbers: false,
            icons: Icons::Auto,
            show_hidden: true,
            git_pane: false,
            restore_session: true,
            font: DEFAULT_FONT.into(),
            font_size: 14.0,
            ai_split: "right".into(),
            ai_skill: true,
            ai_sound: "default".into(),
            review_changes: true,
            mouse: true,
            ai: BTreeMap::new(),
            plugins: BTreeMap::new(),
            lsp_enabled: true,
            format_on_save: vec!["go".into()],
            lsp: BTreeMap::new(),
        }
    }
}

/// Explorer icon style. `Auto` resolves to `Nerd` or `Basic` when the config loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icons {
    Auto,
    /// Nerd Font glyphs; they show as `?` or boxes without a Nerd Font.
    Nerd,
    /// Plain Unicode markers that every monospace font can draw.
    Basic,
    Off,
}

impl<'de> Deserialize<'de> for Icons {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Name(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Flag(true) => Ok(Self::Auto),
            Raw::Flag(false) => Ok(Self::Off),
            Raw::Name(name) => match name.as_str() {
                "auto" => Ok(Self::Auto),
                "nerd" => Ok(Self::Nerd),
                "basic" => Ok(Self::Basic),
                "none" | "off" => Ok(Self::Off),
                _ => Err(serde::de::Error::custom(format!(
                    "icons must be true, false, \"auto\", \"nerd\" or \"basic\", not \"{name}\""
                ))),
            },
        }
    }
}

pub fn config_dir() -> PathBuf {
    // XDG on Linux and macOS, %APPDATA% on Windows. Config is user-owned, never
    // project-owned.
    crate::platform::config_base().join("raido")
}

/// Raido was called Odin: its config folder (settings, themes) moves to the new
/// name the first time Raido starts.
pub fn adopt_odin_config() {
    adopt_odin(&crate::platform::config_base().join("odin"), &config_dir());
}

/// Move Odin's `old` folder to `new` when there's no `new` yet. Only a folder with
/// Odin's files (`config.toml`, `themes/`) moves, never someone else's `odin`.
fn adopt_odin(old: &Path, new: &Path) -> bool {
    let odins = old.join("config.toml").is_file() || old.join("themes").is_dir();
    odins && !new.exists() && fs::rename(old, new).is_ok()
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("config.toml");
        let mut config: Self = if path.exists() {
            toml::from_str(&fs::read_to_string(&path)?).context("Invalid Raido config")?
        } else {
            Self::default()
        };
        config.sidebar_width = config.sidebar_width.clamp(18, 60);
        config.tab_width = config.tab_width.clamp(1, 16);
        if config.icons == Icons::Auto {
            config.icons = if crate::icons::nerd_font_available() {
                Icons::Nerd
            } else {
                Icons::Basic
            };
        }
        Ok(config)
    }
}

/// Remember `name` as the startup theme in the user's config.toml.
pub fn save_theme(name: &str) -> Result<PathBuf> {
    let path = config_dir().join("config.toml");
    if !cfg!(test) {
        save_theme_in(&path, name)?;
    }
    Ok(path)
}

/// Rewrite only the top-level `theme = ...` line (or add one), keeping every
/// other setting and comment as the user wrote it.
fn save_theme_in(path: &Path, name: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("Theme names may contain letters, numbers, - and _ only");
    }
    let old = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let setting = format!("theme = \"{name}\"");
    let mut lines: Vec<String> = old.lines().map(String::from).collect();
    // Top-level keys end at the first [table] header.
    let top = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let existing = lines[..top].iter().position(|l| {
        l.trim_start()
            .strip_prefix("theme")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    match existing {
        Some(i) => lines[i] = setting,
        None => lines.insert(0, setting),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    // Never write a config Raido itself could not load back.
    let check: Config = toml::from_str(&text).context("Config would become invalid")?;
    if check.theme != name {
        bail!("Could not update the theme setting in {}", path.display());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

pub fn themes_dir() -> PathBuf {
    config_dir().join("themes")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The file behind a theme, if any: the user's `<name>.toml` or VS Code `<name>.json`,
/// else an Omarchy theme's `colors.toml`.
pub fn theme_file(name: &str) -> Option<PathBuf> {
    theme_file_in(name, &themes_dir()).or_else(|| crate::omarchy::file(name))
}

/// Where a theme comes from, for the theme picker.
pub fn theme_origin(name: &str) -> &'static str {
    if theme_file_in(name, &themes_dir()).is_some() {
        "your file"
    } else if crate::omarchy::file(name).is_some() {
        "omarchy"
    } else {
        "built in"
    }
}

fn theme_file_in(name: &str, dir: &Path) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    ["toml", "json"]
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|p| p.is_file())
}

/// Modification time of the user's theme file, for live reloading.
pub fn theme_stamp(name: &str) -> Option<SystemTime> {
    fs::metadata(theme_file(name)?).ok()?.modified().ok()
}

/// Every selectable theme: built-ins plus `.toml`/`.json` files in the themes directory.
pub fn available_themes() -> Vec<String> {
    let mut names: BTreeSet<String> = ["phosphor", "amber"]
        .into_iter()
        .chain(BUNDLED_THEMES.iter().map(|(n, _)| *n))
        .map(String::from)
        .collect();
    names.extend(crate::omarchy::themes());
    if let Ok(entries) = fs::read_dir(themes_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if let (Some("toml" | "json"), Some(stem)) =
                (ext, path.file_stem().and_then(|s| s.to_str()))
            {
                if valid_name(stem) {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

/// Load a theme. Files in the user themes directory win over built-ins, so any
/// theme can be added or customized without rebuilding Raido.
pub fn load_theme(name: &str) -> Result<Theme> {
    load_theme_from(name, Some(&themes_dir()))
}

pub(crate) fn load_theme_from(name: &str, dir: Option<&Path>) -> Result<Theme> {
    if !valid_name(name) {
        bail!("Theme names may contain letters, numbers, - and _ only");
    }
    let theme = if let Some(path) = dir.and_then(|d| theme_file_in(name, d)) {
        if path.extension().is_some_and(|e| e == "json") {
            crate::vscode::import(&path)?
        } else {
            toml::from_str(
                &fs::read_to_string(&path).with_context(|| format!("Read {}", path.display()))?,
            )
            .with_context(|| format!("Invalid theme {}", path.display()))?
        }
    } else if let Some(path) = dir.and(crate::omarchy::file(name)) {
        crate::omarchy::load(&path)?
    } else if name == "phosphor" {
        Theme::default()
    } else if name == "amber" {
        Theme {
            accent: "#ffca80".into(),
            secondary: "#b9b4dc".into(),
            background: "#15120d".into(),
            selection: "#302619".into(),
            ..Theme::default()
        }
    } else if let Some((_, source)) = BUNDLED_THEMES.iter().find(|(id, _)| *id == name) {
        toml::from_str(source).with_context(|| format!("Invalid bundled theme {name}"))?
    } else {
        bail!(
            "No theme named {name}. :themes lists them; add {name}.toml or a VS Code {name}.json to {}",
            themes_dir().display()
        );
    };
    validate_theme(&theme)?;
    Ok(theme)
}

fn validate_theme(theme: &Theme) -> Result<()> {
    let mut colors = vec![
        &theme.background,
        &theme.foreground,
        &theme.muted,
        &theme.accent,
        &theme.secondary,
        &theme.warning,
        &theme.border,
        &theme.selection,
    ];
    if let Some(syntax) = &theme.syntax_colors {
        colors.extend([
            &syntax.comment,
            &syntax.keyword,
            &syntax.function,
            &syntax.r#type,
            &syntax.string,
            &syntax.number,
            &syntax.constant,
            &syntax.operator,
            &syntax.variable,
        ]);
        colors.extend(&syntax.control);
    }
    colors.extend(&theme.line_number);
    if let Some(ansi) = &theme.ansi {
        colors.extend(ansi);
    }
    for value in colors {
        if value.len() != 7
            || !value.starts_with('#')
            || u32::from_str_radix(&value[1..], 16).is_err()
        {
            bail!("Theme colors must use #RRGGBB");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_path_cannot_escape_directory() {
        assert!(load_theme("../../secrets").is_err());
        assert!(load_theme("phosphor").is_ok());
    }
    #[test]
    fn user_theme_files_override_builtins_and_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dracula.toml"), "background = '#010203'\n").unwrap();
        std::fs::write(
            dir.path().join("mine.json"),
            r##"{ "colors": { "editor.background": "#0a0b0c" } }"##,
        )
        .unwrap();
        let theme = load_theme_from("dracula", Some(dir.path())).unwrap();
        assert_eq!(theme.background, "#010203");
        let theme = load_theme_from("mine", Some(dir.path())).unwrap();
        assert_eq!(theme.background, "#0a0b0c");
        assert!(load_theme_from("mine", None).is_err());
        assert!(load_theme_from("missing", Some(dir.path())).is_err());
    }
    #[test]
    fn saving_a_theme_edits_only_the_theme_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raido/config.toml");
        save_theme_in(&path, "dracula").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "theme = \"dracula\"\n");
        let original = "# my settings\ntab_width = 2\ntheme = \"nord\" # old\n\n[plugins.test]\nprogram = \"cargo\"\n";
        fs::write(&path, original).unwrap();
        save_theme_in(&path, "vscode-dark-2026").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original.replace("theme = \"nord\" # old", "theme = \"vscode-dark-2026\"")
        );
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            (config.theme.as_str(), config.tab_width),
            ("vscode-dark-2026", 2)
        );
        // A config without a top-level theme gets one at the top, above any table.
        fs::write(&path, "[plugins.test]\nprogram = \"cargo\"\n").unwrap();
        save_theme_in(&path, "monokai").unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .starts_with("theme = \"monokai\"\n[plugins"));
        assert!(save_theme_in(&path, "../bad").is_err());
    }
    #[test]
    fn icons_accept_booleans_and_names() {
        let icons =
            |value: &str| toml::from_str::<Config>(&format!("icons = {value}")).map(|c| c.icons);
        assert_eq!(icons("true").unwrap(), Icons::Auto);
        assert_eq!(icons("false").unwrap(), Icons::Off);
        assert_eq!(icons("\"nerd\"").unwrap(), Icons::Nerd);
        assert_eq!(icons("\"basic\"").unwrap(), Icons::Basic);
        assert!(icons("\"emoji\"").is_err());
    }

    #[test]
    fn font_defaults_to_jetbrains_mono_and_can_be_turned_off() {
        let config = Config::default();
        assert_eq!(config.font, "JetBrainsMono Nerd Font Mono");
        assert_eq!(config.font_size, 14.0);
        let config: Config = toml::from_str("font = \"\"\nfont_size = 13").unwrap();
        assert!(config.font.is_empty());
        assert_eq!(config.font_size, 13.0);
        let example: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(example.font, DEFAULT_FONT);
    }
    #[test]
    fn config_rejects_unknown_fields() {
        assert!(toml::from_str::<Config>("silent_exec = 'bad'").is_err());
    }
    #[test]
    fn all_bundled_themes_have_complete_valid_palettes() {
        assert_eq!(BUNDLED_THEMES.len(), 11);
        let mut backgrounds = std::collections::HashSet::new();
        for (name, _) in BUNDLED_THEMES {
            let theme = load_theme_from(name, None).unwrap();
            validate_theme(&theme).unwrap();
            assert!(theme.syntax_colors.is_some(), "{name}");
            assert!(theme.ansi.is_some(), "{name}");
            assert!(
                backgrounds.insert(theme.background),
                "duplicate palette {name}"
            );
        }
    }
    #[test]
    fn legacy_theme_still_loads_and_extended_colors_are_validated() {
        let legacy: Theme = toml::from_str(include_str!("../themes/ice.toml")).unwrap();
        validate_theme(&legacy).unwrap();
        assert!(legacy.syntax_colors.is_none());
        assert!(legacy.ansi.is_none());
        let mut theme = load_theme_from("dracula", None).unwrap();
        theme.syntax_colors.as_mut().unwrap().comment = "not-a-color".into();
        assert!(validate_theme(&theme).is_err());
        let mut theme = load_theme_from("nord", None).unwrap();
        theme.ansi.as_mut().unwrap()[2] = "#xyzxyz".into();
        assert!(validate_theme(&theme).is_err());
        assert!(toml::from_str::<Theme>("ansi = ['#000000']").is_err());
        assert!(toml::from_str::<Theme>("[syntax_colors]\ncomment = '#000000'").is_err());
    }

    #[test]
    fn odins_config_folder_moves_to_raido_once() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("odin"), dir.path().join("raido"));
        fs::create_dir_all(old.join("themes")).unwrap();
        fs::write(old.join("config.toml"), "theme = \"nord\"\n").unwrap();
        assert!(adopt_odin(&old, &new));
        assert!(new.join("config.toml").is_file() && new.join("themes").is_dir());
        assert!(!old.exists());
        // An unrelated `odin` folder, or one when Raido's exists, is left alone.
        fs::create_dir_all(old.join("cache")).unwrap();
        assert!(!adopt_odin(&old, &new));
        fs::write(old.join("config.toml"), "").unwrap();
        assert!(!adopt_odin(&old, &new));
        assert!(old.exists());
    }
}
