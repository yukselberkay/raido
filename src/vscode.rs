//! Import a VS Code color theme (`.json`) as a Raido theme, so any theme from
//! the VS Code Marketplace can be dropped into the user themes directory.
use crate::config::{SyntaxColors, Theme};
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::{fs, path::Path};

const MAX_INCLUDES: usize = 4;
/// VS Code's integrated-terminal palette, used for colors a theme leaves out.
const DEFAULT_ANSI: [&str; 16] = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#e5e5e5",
];
const ANSI_KEYS: [&str; 16] = [
    "Black",
    "Red",
    "Green",
    "Yellow",
    "Blue",
    "Magenta",
    "Cyan",
    "White",
    "BrightBlack",
    "BrightRed",
    "BrightGreen",
    "BrightYellow",
    "BrightBlue",
    "BrightMagenta",
    "BrightCyan",
    "BrightWhite",
];

struct Rule {
    selectors: Vec<String>,
    foreground: String,
}

#[derive(Default)]
struct Source {
    colors: Map<String, Value>,
    rules: Vec<Rule>,
    light: bool,
}

pub fn import(path: &Path) -> Result<Theme> {
    let mut source = Source::default();
    read(path, &mut source, 0)?;
    Ok(convert(&source))
}

/// Load a theme file, following `include` chains (parents first, so the child wins).
fn read(path: &Path, source: &mut Source, depth: usize) -> Result<()> {
    if depth > MAX_INCLUDES {
        bail!("Theme include chain is too deep");
    }
    let text = fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
    let json: Value = serde_json::from_str(&strip_jsonc(&text))
        .with_context(|| format!("Invalid VS Code theme {}", path.display()))?;
    if let Some(include) = json.get("include").and_then(Value::as_str) {
        let parent = path.parent().unwrap_or(Path::new(".")).join(include);
        // Built-in VS Code parents (e.g. ./dark_modern.json) are often absent; skip them.
        if parent.exists() {
            read(&parent, source, depth + 1)?;
        }
    }
    if let Some(kind) = json.get("type").and_then(Value::as_str) {
        source.light = kind.contains("light");
    }
    if let Some(colors) = json.get("colors").and_then(Value::as_object) {
        source
            .colors
            .extend(colors.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    for rule in json
        .get("tokenColors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(foreground) = rule.pointer("/settings/foreground").and_then(Value::as_str) else {
            continue;
        };
        let selectors = match rule.get("scope") {
            Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().to_string())
                .collect(),
            // A scopeless rule holds the old-style global editor colors.
            _ => {
                source
                    .colors
                    .entry("editor.foreground")
                    .or_insert_with(|| Value::String(foreground.into()));
                continue;
            }
        };
        source.rules.push(Rule {
            selectors,
            foreground: foreground.into(),
        });
    }
    Ok(())
}

fn convert(source: &Source) -> Theme {
    let raw = |key: &str| source.colors.get(key).and_then(Value::as_str);
    let fallback_bg = if source.light { "#ffffff" } else { "#1e1e1e" };
    let fallback_fg = if source.light { "#333333" } else { "#d4d4d4" };
    let background = raw("editor.background")
        .and_then(|c| parse(c, None))
        .unwrap_or_else(|| fallback_bg.into());
    let foreground = ["editor.foreground", "foreground"]
        .iter()
        .find_map(|k| raw(k).and_then(|c| parse(c, None)))
        .unwrap_or_else(|| fallback_fg.into());
    // Text-like colors drop their alpha; fills are blended onto the editor background.
    let first = |keys: &[&str], blend: bool| {
        keys.iter()
            .find_map(|k| raw(k).and_then(|c| parse(c, blend.then_some(background.as_str()))))
    };
    let muted = first(
        &["descriptionForeground", "editorLineNumber.foreground"],
        false,
    )
    .unwrap_or_else(|| mix(&background, &foreground, 0.55));
    let accent = first(
        &[
            "focusBorder",
            "activityBarBadge.background",
            "button.background",
            "textLink.foreground",
        ],
        false,
    )
    .unwrap_or_else(|| foreground.clone());
    let terminal_bg = first(&["terminal.background", "panel.background"], true)
        .unwrap_or_else(|| background.clone());
    let ansi: [String; 16] = std::array::from_fn(|i| {
        first(&[&format!("terminal.ansi{}", ANSI_KEYS[i])], false).unwrap_or_else(|| {
            if i == 0 {
                terminal_bg.clone()
            } else {
                DEFAULT_ANSI[i].into()
            }
        })
    });
    let token = |scope: &str| resolve(&source.rules, scope).unwrap_or_else(|| foreground.clone());
    let keyword = token("keyword");
    let syntax_colors = SyntaxColors {
        comment: token("comment.line.double-slash"),
        control: Some(token("keyword.control")),
        function: token("entity.name.function"),
        r#type: token("entity.name.type"),
        string: token("string.quoted.double"),
        number: token("constant.numeric"),
        constant: token("constant.language"),
        operator: token("keyword.operator"),
        variable: token("variable.other.readwrite"),
        keyword,
    };
    Theme {
        secondary: first(&["textLink.foreground", "terminal.ansiCyan"], false)
            .unwrap_or_else(|| accent.clone()),
        warning: first(
            &[
                "editorWarning.foreground",
                "notificationsWarningIcon.foreground",
                "list.warningForeground",
            ],
            false,
        )
        .unwrap_or_else(|| "#cca700".into()),
        border: first(
            &[
                "panel.border",
                "editorGroup.border",
                "sideBar.border",
                "widget.border",
            ],
            true,
        )
        .unwrap_or_else(|| mix(&background, &foreground, 0.18)),
        selection: first(
            &[
                "editor.lineHighlightBackground",
                "list.inactiveSelectionBackground",
                "editor.selectionBackground",
            ],
            true,
        )
        .unwrap_or_else(|| mix(&background, &foreground, 0.1)),
        line_number: first(&["editorLineNumber.foreground"], false),
        syntax_colors: Some(syntax_colors),
        ansi: Some(ansi),
        background,
        foreground,
        muted,
        accent,
        ..Theme::default()
    }
}

/// TextMate-style lookup: the most specific selector that prefixes `scope` wins;
/// among equals, the later rule wins (as in VS Code).
fn resolve(rules: &[Rule], scope: &str) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for rule in rules {
        for selector in &rule.selectors {
            // Descendant and exclusion selectors need a scope stack; skip them.
            if selector.is_empty() || selector.contains(' ') {
                continue;
            }
            let matches = scope == selector
                || scope
                    .strip_prefix(selector.as_str())
                    .is_some_and(|rest| rest.starts_with('.'));
            let depth = selector.split('.').count();
            if matches && best.is_none_or(|(d, _)| depth >= d) {
                best = Some((depth, &rule.foreground));
            }
        }
    }
    best.and_then(|(_, c)| parse(c, None))
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` → `#rrggbb`. With a background,
/// alpha is blended onto it; otherwise it is dropped.
fn parse(value: &str, background: Option<&str>) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full: String = match hex.len() {
        3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => hex.to_string(),
        _ => return None,
    };
    let rgb = format!("#{}", full[..6].to_ascii_lowercase());
    match (full.len(), background) {
        (8, Some(bg)) => {
            let alpha = u8::from_str_radix(&full[6..], 16).ok()? as f32 / 255.0;
            Some(mix(bg, &rgb, alpha))
        }
        _ => Some(rgb),
    }
}

fn mix(a: &str, b: &str, t: f32) -> String {
    let channel = |s: &str, i: usize| u8::from_str_radix(&s[1 + 2 * i..3 + 2 * i], 16).unwrap_or(0);
    let mut out = String::from("#");
    for i in 0..3 {
        let (x, y) = (channel(a, i) as f32, channel(b, i) as f32);
        out.push_str(&format!("{:02x}", (x + (y - x) * t).round() as u8));
    }
    out
}

/// VS Code themes are JSONC: strip comments and trailing commas outside strings.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            (',', _) => {
                // Drop the comma when only whitespace/comments precede a closing bracket.
                let rest: String = chars.clone().collect();
                let next = strip_leading_trivia(&rest);
                if !next.starts_with(['}', ']']) {
                    out.push(',');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn strip_leading_trivia(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("//") {
            text = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = text.strip_prefix("/*") {
            text = rest.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_comments_and_trailing_commas_are_removed() {
        let text = "{\n // note\n \"a\": \"http://x\", /* c */\n \"b\": [1, 2,],\n}";
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["a"], "http://x");
        assert_eq!(value["b"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn colors_normalize_and_blend() {
        assert_eq!(parse("#ABC", None).unwrap(), "#aabbcc");
        assert_eq!(parse("#3994BCB3", None).unwrap(), "#3994bc");
        assert_eq!(parse("#ffffff80", Some("#000000")).unwrap(), "#808080");
        assert!(parse("red", None).is_none());
    }

    #[test]
    fn imports_a_vscode_theme_with_include_and_scope_specificity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("base.json"),
            r##"{ "colors": { "editor.background": "#101010", "focusBorder": "#ff0000" },
                 "tokenColors": [ { "scope": "keyword", "settings": { "foreground": "#111111" } } ] }"##,
        )
        .unwrap();
        let path = dir.path().join("child.json");
        std::fs::write(
            &path,
            r##"{ // child theme
              "include": "./base.json", "type": "dark",
              "colors": { "editor.foreground": "#eeeeee", "editor.lineHighlightBackground": "#ffffff20", },
              "tokenColors": [
                { "scope": ["keyword.control", "comment"], "settings": { "foreground": "#222222" } },
                { "scope": "string, constant.numeric", "settings": { "foreground": "#333333" } },
                { "scope": "source.rust keyword", "settings": { "foreground": "#999999" } },
              ] }"##,
        )
        .unwrap();
        let theme = import(&path).unwrap();
        assert_eq!(theme.background, "#101010");
        assert_eq!(theme.foreground, "#eeeeee");
        assert_eq!(theme.accent, "#ff0000");
        assert_eq!(theme.selection, "#2e2e2e");
        let syntax = theme.syntax_colors.as_ref().unwrap();
        assert_eq!(syntax.keyword, "#111111");
        assert_eq!(syntax.control.as_deref(), Some("#222222"));
        assert_eq!(syntax.comment, "#222222");
        assert_eq!(syntax.string, "#333333");
        assert_eq!(syntax.number, "#333333");
        // Unmatched scopes fall back to the editor foreground.
        assert_eq!(syntax.function, "#eeeeee");
        assert_eq!(theme.ansi.as_ref().unwrap()[0], "#101010");
    }
}
