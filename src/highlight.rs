use crate::{config, editor::Buffer};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};
use syntect::{
    highlighting::{
        Color, HighlightIterator, HighlightState, Highlighter, Style, StyleModifier, Theme,
        ThemeItem, ThemeSet, ThemeSettings,
    },
    parsing::{
        ParseState, ScopeStack, SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder,
    },
};

/// Grammars Raido adds to the bundled ones: TOML (`pyproject.toml`, `Cargo.toml`,
/// Raido's own settings).
const EXTRA: [&str; 1] = [include_str!("syntaxes/toml.sublime-syntax")];

pub struct Cached {
    revision: u64,
    name: String,
    /// The grammar is one of Raido's own (`Syntax::extra`), parsed with that set.
    extra: bool,
    parse: ParseState,
    state: HighlightState,
    pub lines: Vec<Vec<(Style, String)>>,
    /// The parser's state at the start of each highlighted line, and a hash of the
    /// line, so an edit is highlighted again from its first changed line only.
    starts: Vec<(ParseState, HighlightState)>,
    hashes: Vec<u64>,
    /// The minimap's colors for lines by their text.
    pub shades: crate::minimap::Shades,
}

fn line_hash(line: ropey::RopeSlice) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for chunk in line.chunks() {
        chunk.hash(&mut hasher);
    }
    hasher.finish()
}

pub struct Syntax {
    pub syntaxes: SyntaxSet,
    /// Raido's own grammars, kept in a set of their own: adding them to the
    /// bundled set means rebuilding it, a tenth of a second at every start.
    extra: SyntaxSet,
    pub theme: Theme,
    pub cache: HashMap<usize, Cached>,
}

impl Syntax {
    pub fn new(palette: &config::Theme) -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut extra = SyntaxSetBuilder::new();
        for source in EXTRA {
            extra
                .add(SyntaxDefinition::load_from_str(source, true, None).expect("bundled grammar"));
        }
        let extra = extra.build();
        let theme = if let Some(colors) = &palette.syntax_colors {
            let mut theme = Theme {
                settings: ThemeSettings {
                    foreground: Some(syntax_color(&palette.foreground)),
                    background: Some(syntax_color(&palette.background)),
                    ..ThemeSettings::default()
                },
                ..Theme::default()
            };
            // Static TextMate selectors work across all bundled language grammars.
            // Build a fresh theme, not an overlay: no unrelated legacy colors leak in.
            for (scope, value) in [
                ("comment, punctuation.definition.comment", &colors.comment),
                ("keyword, storage", &colors.keyword),
                (
                    "keyword.control - keyword.control.import - keyword.control.directive",
                    colors.control.as_ref().unwrap_or(&colors.keyword),
                ),
                ("entity.name.function, support.function, variable.function", &colors.function),
                ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class, storage.type", &colors.r#type),
                ("string, punctuation.definition.string", &colors.string),
                ("constant.numeric", &colors.number),
                ("constant.language, constant.character, constant.other", &colors.constant),
                ("keyword.operator, punctuation.accessor", &colors.operator),
                ("variable - variable.function", &colors.variable),
                ("markup.heading", &palette.accent),
                ("markup.inline.raw, markup.raw", &colors.string),
                ("markup.inserted", &colors.string),
                ("markup.deleted, invalid", &palette.warning),
            ] {
                theme.scopes.push(ThemeItem {
                    scope: scope.parse().expect("static syntax selector"),
                    style: StyleModifier {
                        foreground: Some(syntax_color(value)),
                        ..StyleModifier::default()
                    },
                });
            }
            theme
        } else {
            let themes = ThemeSet::load_defaults();
            themes
                .themes
                .get(&palette.syntax)
                .unwrap_or(&themes.themes["base16-ocean.dark"])
                .clone()
        };
        Self {
            syntaxes,
            extra,
            theme,
            cache: HashMap::new(),
        }
    }
    /// The grammar for files ending in `extension`, and the set to parse it with.
    pub fn by_extension(&self, extension: &str) -> Option<(&SyntaxReference, &SyntaxSet)> {
        (self
            .syntaxes
            .find_syntax_by_extension(extension)
            .map(|g| (g, &self.syntaxes)))
        .or_else(|| {
            self.extra
                .find_syntax_by_extension(extension)
                .map(|g| (g, &self.extra))
        })
    }
    /// The grammar a Markdown code block labeled `token` (`rust`, `toml`) means,
    /// and the set to parse it with.
    pub fn by_token(&self, token: &str) -> Option<(&SyntaxReference, &SyntaxSet)> {
        (self
            .syntaxes
            .find_syntax_by_token(token)
            .map(|g| (g, &self.syntaxes)))
        .or_else(|| {
            self.extra
                .find_syntax_by_token(token)
                .map(|g| (g, &self.extra))
        })
    }
    fn set(&self, extra: bool) -> &SyntaxSet {
        if extra {
            &self.extra
        } else {
            &self.syntaxes
        }
    }
    pub fn language(&self, buffer: &Buffer) -> String {
        buffer
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(|e| self.by_extension(e))
            .map(|(g, _)| g.name.clone())
            .unwrap_or_else(|| "Plain text".into())
    }
    /// Highlight `buffer` down to line `end`, a bounded amount per call; true when
    /// there's more to do. After an edit only the lines from the first changed one
    /// are done again, so typing never leaves the lines on screen uncolored.
    pub fn prepare(&mut self, id: usize, buffer: &Buffer, end: usize) -> bool {
        self.prepare_lines(id, buffer, end, 512)
    }
    /// `prepare`, highlighting at most `budget` lines in this call.
    pub fn prepare_lines(&mut self, id: usize, buffer: &Buffer, end: usize, budget: usize) -> bool {
        let name = buffer.name();
        if let Some(cache) = self
            .cache
            .get_mut(&id)
            .filter(|c| c.revision != buffer.revision && c.name == name)
        {
            let known = cache.hashes.len().min(buffer.lines());
            let first = (0..known)
                .find(|&row| line_hash(buffer.text.line(row)) != cache.hashes[row])
                .unwrap_or(known);
            if first < cache.starts.len() {
                let (parse, state) = cache.starts[first].clone();
                cache.parse = parse;
                cache.state = state;
            }
            cache.lines.truncate(first);
            cache.hashes.truncate(first);
            cache.starts.truncate(first);
            cache.revision = buffer.revision;
        }
        let reset = self
            .cache
            .get(&id)
            .is_none_or(|c| c.revision != buffer.revision || c.name != name);
        if reset {
            let (syntax, set) = buffer
                .path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|s| s.to_str())
                .and_then(|extension| self.by_extension(extension))
                .unwrap_or_else(|| (self.syntaxes.find_syntax_plain_text(), &self.syntaxes));
            let extra = std::ptr::eq(set, &self.extra);
            let highlighter = Highlighter::new(&self.theme);
            self.cache.insert(
                id,
                Cached {
                    revision: buffer.revision,
                    name,
                    extra,
                    parse: ParseState::new(syntax),
                    state: HighlightState::new(&highlighter, ScopeStack::new()),
                    lines: Vec::new(),
                    starts: Vec::new(),
                    hashes: Vec::new(),
                    shades: Default::default(),
                },
            );
        }
        let cache = self.cache.get_mut(&id).expect("cache just inserted");
        let set = if cache.extra {
            &self.extra
        } else {
            &self.syntaxes
        };
        let highlighter = Highlighter::new(&self.theme);
        let target = end.min(buffer.lines());
        let stop = target.min(cache.lines.len() + budget);
        for row in cache.lines.len()..stop {
            let slice = buffer.text.line(row);
            cache
                .starts
                .push((cache.parse.clone(), cache.state.clone()));
            cache.hashes.push(line_hash(slice));
            let line = slice.to_string();
            // Bound regex work on minified/generated lines.
            if line.len() > 16_384 {
                cache.lines.push(Vec::new());
                continue;
            }
            let regions = match cache.parse.parse_line(&line, set) {
                Ok(ops) => HighlightIterator::new(&mut cache.state, &ops, &line, &highlighter)
                    .map(|(style, text)| (style, text.to_owned()))
                    .collect(),
                Err(_) => Vec::new(),
            };
            cache.lines.push(regions);
        }
        cache.lines.len() < target
    }
    /// The grammar for a file named `name`, unless it's plain text, and the set to
    /// parse it with.
    pub fn grammar(&self, name: &str) -> Option<(&SyntaxReference, &SyntaxSet)> {
        let extension = std::path::Path::new(name).extension()?.to_str()?;
        self.by_extension(extension)
    }
    /// A review hunk's lines in the colors of file `name`'s language. The lines as
    /// they were (context and removed) and as they are (context and added) each go
    /// through a parser of their own from the hunk's start, so both versions color
    /// as they read. `None` for plain text.
    pub fn hunk(
        &self,
        name: &str,
        lines: &[crate::review::Line],
    ) -> Option<Vec<Vec<(Style, String)>>> {
        use crate::review::Kind;
        // A huge new file stays plain rather than holding up the screen.
        if lines.len() > 5_000 {
            return None;
        }
        let (grammar, set) = self.grammar(name)?;
        let highlighter = Highlighter::new(&self.theme);
        let fresh = || {
            (
                ParseState::new(grammar),
                HighlightState::new(&highlighter, ScopeStack::new()),
            )
        };
        let (mut before, mut after) = (fresh(), fresh());
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            let text = format!("{}\n", line.text);
            let color = |(parse, state): &mut (ParseState, HighlightState)| {
                if text.len() > 16_384 {
                    return Vec::new();
                }
                match parse.parse_line(&text, set) {
                    Ok(ops) => HighlightIterator::new(state, &ops, &text, &highlighter)
                        .map(|(style, piece)| (style, piece.trim_end_matches('\n').to_owned()))
                        .filter(|(_, piece)| !piece.is_empty())
                        .collect(),
                    Err(_) => Vec::new(),
                }
            };
            out.push(match line.kind {
                Kind::Removed => color(&mut before),
                Kind::Added => color(&mut after),
                Kind::Context => {
                    color(&mut before);
                    color(&mut after)
                }
            });
        }
        Some(out)
    }
    /// Whether the text just before column `col` (a char index) of `row` is inside a
    /// string or a comment, by the language's grammar. Suggestions don't pop up
    /// there by themselves.
    pub fn in_string_or_comment(
        &mut self,
        id: usize,
        buffer: &Buffer,
        row: usize,
        col: usize,
    ) -> bool {
        while self.prepare(id, buffer, row + 1) {}
        let Some(cache) = self.cache.get(&id) else {
            return false;
        };
        let set = self.set(cache.extra);
        let Some((parse, state)) = cache.starts.get(row) else {
            return false;
        };
        let (mut parse, mut scopes) = (parse.clone(), state.path.clone());
        let line = buffer.text.line(row).to_string();
        let byte = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        let Ok(ops) = parse.parse_line(&line, set) else {
            return false;
        };
        for (offset, op) in ops {
            if offset >= byte {
                break;
            }
            let _ = scopes.apply(&op);
        }
        scopes.as_slice().iter().any(|scope| {
            let name = scope.build_string();
            name.starts_with("string") || name.starts_with("comment")
        })
    }
}

fn syntax_color(value: &str) -> Color {
    let (r, g, b) = match config::color(value) {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        _ => (255, 255, 255),
    };
    Color { r, g, b, a: 255 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_edit_highlights_again_from_the_changed_line_only() {
        let palette = config::load_theme_from("visual-studio-dark", None).unwrap();
        let mut syntax = Syntax::new(&palette);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.py");
        let body: String = (0..2000)
            .map(|n| format!("value_{n} = {n}  # note\n"))
            .collect();
        std::fs::write(&path, &body).unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        let id = 0;
        // The first time, it works its way down to line 1900 a bounded step at a time.
        let mut calls = 0;
        while syntax.prepare(id, &buffer, 1950) {
            calls += 1;
        }
        assert!(calls >= 3);
        let comment = |syntax: &Syntax, row: usize| {
            let line = &syntax.cache[&id].lines[row];
            line.iter()
                .find(|(_, t)| t.contains("note"))
                .unwrap()
                .0
                .foreground
        };
        let before = comment(&syntax, 1900);
        // Typing at line 1900: one call covers the screen again.
        buffer.row = 1900;
        buffer.col = 0;
        buffer.insert("x");
        assert!(!syntax.prepare(id, &buffer, 1950), "done in one step");
        assert_eq!(syntax.cache[&id].lines.len(), 1950);
        assert_eq!(comment(&syntax, 1900), before);
        // An edit that changes what follows: a docstring opened at line 10 swallows
        // the lines below whole; closing it again splits them back into code.
        let pieces = |syntax: &Syntax, row: usize| syntax.cache[&id].lines[row].len();
        let code = pieces(&syntax, 30);
        assert!(code > 1);
        buffer.row = 10;
        buffer.col = 0;
        buffer.insert("\"\"\"");
        while syntax.prepare(id, &buffer, 40) {}
        assert_eq!(pieces(&syntax, 30), 1, "all docstring");
        buffer.backspace();
        buffer.backspace();
        buffer.backspace();
        while syntax.prepare(id, &buffer, 40) {}
        assert_eq!(pieces(&syntax, 30), code);
        assert_eq!(comment(&syntax, 30), before);
    }
    /// The scopes the TOML grammar gives `token` where it first appears after
    /// `after` in `text`, line by line as the editor parses it.
    fn toml_scope(text: &str, after: &str, token: &str) -> String {
        let syntax = Syntax::new(&config::Theme::default());
        let (grammar, set) = syntax.by_extension("toml").expect("toml grammar");
        let mut parse = ParseState::new(grammar);
        let mut stack = ScopeStack::new();
        let from = text.find(after).expect("anchor");
        let target = from + text[from..].find(token).expect("token");
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let ops = parse.parse_line(line, set).unwrap();
            for (at, op) in ops {
                if offset + at > target {
                    return stack.to_string();
                }
                stack.apply(&op).unwrap();
            }
            if offset + line.len() > target {
                return stack.to_string();
            }
            offset += line.len();
        }
        stack.to_string()
    }

    #[test]
    fn toml_is_highlighted_by_raidos_own_grammar() {
        let text = r#"# Project settings
[project]
name = "helios"  # trailing
version = '0.1.0'
dependencies = [
    "pyyaml==6.0.3",   # pinned
    "requests",
]
point = { x = 1, y = -2.5e3, label = "a\tb" }
site."google.com".enabled = true
hex = 0xDEAD_BEEF
big = 1_000_000
odd = nan
when = 1979-05-27T07:32:00Z
day = 1979-05-27
at = 07:32:00
text = """
two "quoted" lines \
here"""
raw = '''C:\path'''

[[servers.alpha]]
ip = "10.0.0.1"
"#;
        for (after, token, scope) in [
            ("# Project", "Project", "comment.line"),
            ("[project]", "project", "entity.name.type.table"),
            ("name =", "name", "variable.other.key"),
            ("name =", "=", "keyword.operator.assignment"),
            ("name =", "helios", "string.quoted.double"),
            ("helios\"", "trailing", "comment.line"),
            ("version", "0.1.0", "string.quoted.single"),
            ("dependencies", "pyyaml", "string.quoted.double"),
            ("pyyaml", "pinned", "comment.line"),
            ("\"requests\"", "requests", "string.quoted.double"),
            ("point", "x", "variable.other.key"),
            ("point", "1", "constant.numeric"),
            ("point", "-2.5e3", "constant.numeric"),
            ("label", "\\t", "constant.character.escape"),
            ("site.", "google.com", "variable.other.key"),
            ("enabled", "true", "constant.language.boolean"),
            ("hex", "0xDEAD", "constant.numeric"),
            ("big", "1_000", "constant.numeric"),
            ("odd", "nan", "constant.numeric"),
            ("when", "1979", "constant.other.datetime"),
            ("day", "1979", "constant.other.datetime"),
            ("at =", "07:32", "constant.other.time"),
            ("text", "quoted", "string.quoted.triple"),
            ("text", "here", "string.quoted.triple"),
            ("raw", "path", "string.quoted.triple.single"),
            ("[[servers", "alpha", "entity.name.type.table"),
            ("ip", "10.0.0.1", "string.quoted.double"),
        ] {
            let found = toml_scope(text, after, token);
            assert!(found.contains(scope), "{token} after {after:?}: {found}");
        }
        // After the multi-line string, keys are keys again.
        assert!(!toml_scope(text, "here\"\"\"", "raw").contains("string"));
    }

    #[test]
    fn every_palette_colors_toml_in_the_editor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pyproject.toml");
        std::fs::write(&path, "[project]\nname = \"x\"  # note\nversion = 3\n").unwrap();
        let buffer = Buffer::open(&path).unwrap();
        for (name, _) in config::BUNDLED_THEMES {
            let palette = config::load_theme_from(name, None).unwrap();
            let colors = palette.syntax_colors.as_ref().unwrap();
            let mut syntax = Syntax::new(&palette);
            assert_eq!(syntax.language(&buffer), "TOML");
            while syntax.prepare(0, &buffer, 3) {}
            let lines = &syntax.cache[&0].lines;
            let color = |row: usize, text: &str| {
                lines[row]
                    .iter()
                    .find(|(_, t)| t.contains(text))
                    .map(|(s, _)| s.foreground)
            };
            assert_eq!(color(1, "x"), Some(syntax_color(&colors.string)), "{name}");
            assert_eq!(
                color(1, "note"),
                Some(syntax_color(&colors.comment)),
                "{name}"
            );
            assert_eq!(color(2, "3"), Some(syntax_color(&colors.number)), "{name}");
            assert_eq!(
                color(0, "project"),
                Some(syntax_color(&colors.r#type)),
                "{name}"
            );
            assert!(syntax.in_string_or_comment(0, &buffer, 1, 9));
            assert!(!syntax.in_string_or_comment(0, &buffer, 2, 3));
        }
    }

    #[test]
    fn all_required_languages_are_bundled() {
        let syntax = Syntax::new(&config::Theme::default());
        for extension in ["rs", "c", "cpp", "py", "go"] {
            assert!(
                syntax
                    .syntaxes
                    .find_syntax_by_extension(extension)
                    .is_some(),
                "{extension}"
            );
        }
    }
    #[test]
    fn every_palette_highlights_all_five_languages_with_its_own_colors() {
        use syntect::easy::HighlightLines;
        for (name, _) in config::BUNDLED_THEMES {
            let palette = config::load_theme_from(name, None).unwrap();
            let colors = palette.syntax_colors.as_ref().unwrap();
            let syntax = Syntax::new(&palette);
            for (ext, comment, code) in [
                (
                    "rs",
                    "// comment\n",
                    "fn main() { let value = 42; let s = \"hello\"; }\n",
                ),
                (
                    "c",
                    "// comment\n",
                    "int main(void) { int value = 42; char *s = \"hello\"; }\n",
                ),
                (
                    "cpp",
                    "// comment\n",
                    "int main() { auto value = 42; auto s = \"hello\"; }\n",
                ),
                ("py", "# comment\n", "value = 42; text = \"hello\"\n"),
                (
                    "go",
                    "// comment\n",
                    "var value = 42; var text = \"hello\"\n",
                ),
            ] {
                let grammar = syntax.syntaxes.find_syntax_by_extension(ext).unwrap();
                let mut highlighter = HighlightLines::new(grammar, &syntax.theme);
                let regions = highlighter
                    .highlight_line(comment, &syntax.syntaxes)
                    .unwrap();
                assert!(
                    regions.iter().any(|(s, t)| t.contains("comment")
                        && s.foreground == syntax_color(&colors.comment)),
                    "{name}/{ext}: comment"
                );
                let regions = highlighter.highlight_line(code, &syntax.syntaxes).unwrap();
                for (text, expected) in [("42", &colors.number), ("hello", &colors.string)] {
                    assert!(
                        regions.iter().any(
                            |(s, t)| t.contains(text) && s.foreground == syntax_color(expected)
                        ),
                        "{name}/{ext}: {text}"
                    );
                }
            }
        }
    }
}
