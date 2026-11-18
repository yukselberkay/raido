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
/// Raido's own settings), Dockerfiles, PowerShell, `.env` and INI files, ignore
/// files, SCSS, Terraform / HCL and Protocol Buffers.
const EXTRA: [&str; 9] = [
    include_str!("syntaxes/toml.sublime-syntax"),
    include_str!("syntaxes/dockerfile.sublime-syntax"),
    include_str!("syntaxes/powershell.sublime-syntax"),
    include_str!("syntaxes/dotenv.sublime-syntax"),
    include_str!("syntaxes/ini.sublime-syntax"),
    include_str!("syntaxes/ignore.sublime-syntax"),
    include_str!("syntaxes/scss.sublime-syntax"),
    include_str!("syntaxes/hcl.sublime-syntax"),
    include_str!("syntaxes/protobuf.sublime-syntax"),
];

pub struct Cached {
    revision: u64,
    name: String,
    /// The grammar is one of Raido's own ([`extra`]), parsed with that set.
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

/// TypeScript and JavaScript, from one grammar (its header is TypeScript's).
const SCRIPT: &str = include_str!("syntaxes/typescript.sublime-syntax");
const SCRIPT_HEADER: &str =
    "name: TypeScript\nfile_extensions:\n  - ts\n  - mts\n  - cts\nscope: source.ts\n";

/// The script grammar's variants: TypeScript without JSX (in .ts, `<T>(x) => x` is a
/// generic), and TSX and JavaScript with it.
fn script_grammars() -> [String; 3] {
    let variant = |name: &str, extensions: &[&str], scope: &str| {
        let extensions: String = extensions.iter().map(|e| format!("  - {e}\n")).collect();
        let header = format!("name: {name}\nfile_extensions:\n{extensions}scope: {scope}\n");
        SCRIPT.replacen(SCRIPT_HEADER, &header, 1)
    };
    [
        SCRIPT.replacen("    - include: jsx\n", "", 1),
        variant("TSX", &["tsx"], "source.tsx"),
        variant("JavaScript", &["js", "mjs", "cjs", "jsx"], "source.js"),
    ]
}

/// The extensions Raido's own script grammars take over from the bundled
/// JavaScript one, which knows no JSX or TypeScript.
const SCRIPTS: [&str; 8] = ["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

/// Raido's own grammars, kept in a set of their own: adding them to the bundled set
/// means rebuilding it, a tenth of a second at every start. Built once, the first
/// time a file has no bundled grammar.
fn extra() -> &'static SyntaxSet {
    static EXTRA_SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    EXTRA_SET.get_or_init(|| {
        let mut set = SyntaxSetBuilder::new();
        for source in EXTRA.into_iter().map(String::from).chain(script_grammars()) {
            set.add(SyntaxDefinition::load_from_str(&source, true, None).expect("bundled grammar"));
        }
        set.build()
    })
}

pub struct Syntax {
    pub syntaxes: SyntaxSet,
    pub theme: Theme,
    pub cache: HashMap<usize, Cached>,
}

impl Syntax {
    pub fn new(palette: &config::Theme) -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
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
                // HTML, XML and JSX tags and their attributes, as VS Code colors them.
                ("entity.name.tag", &colors.keyword),
                ("entity.other.attribute-name", &colors.variable),
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
            extra()
                .find_syntax_by_extension(extension)
                .map(|g| (g, extra()))
        })
    }
    /// The grammar for the file at `path`, and the set to parse it with: by its whole
    /// name first (`Makefile`, `.bashrc`, `Dockerfile`), else by its extension.
    /// Dockerfiles go by their kind too (`Dockerfile.dev`, `api.dockerfile`), and
    /// shell scripts by every name they go by (`.zshrc`, `PKGBUILD`, `.ksh`).
    pub fn for_path(&self, path: &std::path::Path) -> Option<(&SyntaxReference, &SyntaxSet)> {
        if crate::language::is_dockerfile(path) {
            return self.by_extension("Dockerfile");
        }
        if crate::language::shell(Some(path), "").is_some() {
            return self.by_extension("sh");
        }
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if SCRIPTS.iter().any(|s| s.eq_ignore_ascii_case(extension)) {
            return extra()
                .find_syntax_by_extension(extension)
                .map(|g| (g, extra()));
        }
        // JSON by other names: with comments (VS Code's), and tools' dotfiles.
        let json_named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                matches!(name, ".babelrc" | ".swcrc" | ".jshintrc")
                    || name.ends_with(".jsonc")
                    || name.ends_with(".code-workspace")
            });
        if json_named {
            return self.by_extension("json");
        }
        // .env.local, .env.production, .env.example
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with(".env."))
        {
            return self.by_extension("env");
        }
        let keys = [
            path.file_name().and_then(|n| n.to_str()),
            path.extension().and_then(|e| e.to_str()),
        ];
        // The bundled grammars first, so Raido's own are built only when needed.
        let find = |set: &'static SyntaxSet| {
            keys.iter()
                .flatten()
                .find_map(|key| set.find_syntax_by_extension(key))
                .map(|g| (g, set))
        };
        keys.iter()
            .flatten()
            .find_map(|key| self.syntaxes.find_syntax_by_extension(key))
            .map(|g| (g, &self.syntaxes))
            .or_else(|| find(extra()))
    }
    /// The grammar for `buffer`: by its file's name, else by its first line, as for a
    /// script without an extension (`#!/usr/bin/env bash`, `#!/usr/bin/env python3`).
    pub fn for_buffer(&self, buffer: &Buffer) -> Option<(&SyntaxReference, &SyntaxSet)> {
        if let Some(found) = buffer.path.as_deref().and_then(|p| self.for_path(p)) {
            return Some(found);
        }
        let first = crate::language::first_line(buffer);
        if crate::language::shell(buffer.path.as_deref(), &first).is_some() {
            return self.by_extension("sh");
        }
        self.syntaxes
            .find_syntax_by_first_line(&first)
            .map(|g| (g, &self.syntaxes))
    }
    /// The grammar a Markdown code block labeled `token` (`rust`, `toml`) means,
    /// and the set to parse it with.
    pub fn by_token(&self, token: &str) -> Option<(&SyntaxReference, &SyntaxSet)> {
        // The other names GitHub knows these blocks by.
        let token = match token.to_ascii_lowercase().as_str() {
            "shell" | "shellscript" | "ksh" | "dash" | "posix" => "sh",
            "pwsh" | "posh" => "ps1",
            "terraform" => "tf",
            "protobuf" => "proto",
            "dotenv" => "env",
            "ignore" => "gitignore",
            "javascript" | "node" => "js",
            "typescript" => "ts",
            _ => token,
        };
        if SCRIPTS.iter().any(|s| s.eq_ignore_ascii_case(token)) {
            return extra()
                .find_syntax_by_extension(token)
                .map(|g| (g, extra()));
        }
        (self
            .syntaxes
            .find_syntax_by_token(token)
            .map(|g| (g, &self.syntaxes)))
        .or_else(|| extra().find_syntax_by_token(token).map(|g| (g, extra())))
    }
    fn set(&self, own: bool) -> &SyntaxSet {
        if own {
            extra()
        } else {
            &self.syntaxes
        }
    }
    pub fn language(&self, buffer: &Buffer) -> String {
        self.for_buffer(buffer)
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
        let mut first_line_edited = false;
        if let Some(cache) = self
            .cache
            .get_mut(&id)
            .filter(|c| c.revision != buffer.revision && c.name == name)
        {
            let known = cache.hashes.len().min(buffer.lines());
            let first = (0..known)
                .find(|&row| line_hash(buffer.text.line(row)) != cache.hashes[row])
                .unwrap_or(known);
            first_line_edited = first == 0;
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
        // A new first line may mean a new grammar: `#!/bin/bash` typed into a new file.
        if first_line_edited {
            self.cache.remove(&id);
        }
        let reset = self
            .cache
            .get(&id)
            .is_none_or(|c| c.revision != buffer.revision || c.name != name);
        if reset {
            let (syntax, set) = self
                .for_buffer(buffer)
                .unwrap_or_else(|| (self.syntaxes.find_syntax_plain_text(), &self.syntaxes));
            let extra = std::ptr::eq(set, extra());
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
        let set = if cache.extra { extra() } else { &self.syntaxes };
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
        self.for_path(std::path::Path::new(name))
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
        scope_in("a.toml", text, after, token)
    }

    /// The scopes the grammar for a file named `name` gives `token` where it first
    /// appears after `after` in `text`.
    fn scope_in(name: &str, text: &str, after: &str, token: &str) -> String {
        // Loading the grammars takes a while; once per test thread is enough.
        thread_local! {
            static SYNTAX: Syntax = Syntax::new(&config::Theme::default());
        }
        SYNTAX.with(|syntax| scope_with(syntax, name, text, after, token))
    }

    fn scope_with(syntax: &Syntax, name: &str, text: &str, after: &str, token: &str) -> String {
        let (grammar, set) = syntax.grammar(name).expect("grammar");
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
    fn dockerfiles_are_highlighted_by_raidos_own_grammar() {
        let text = r#"# syntax=docker/dockerfile:1
# A build stage
ARG GO_VERSION=1.22
FROM --platform=$BUILDPLATFORM golang:${GO_VERSION} AS build
ENV CGO_ENABLED=0 PATH="/go/bin:$PATH"
RUN --mount=type=cache,target=/go/pkg/mod \
    # fetch first
    go mod download && echo "done" \
    || exit 1
RUN echo it's
WORKDIR /src
COPY --from=build /out/app /app
RUN <<EOF
set -e
echo "in a heredoc"
EOF
COPY <<CONF /etc/app.conf
listen = 8080
CONF
EXPOSE 8080/tcp
HEALTHCHECK --interval=30s CMD curl -f http://localhost/ || exit 1
CMD ["/app", "--port"]
LABEL org.opencontainers.image.source="https://example.com" version=1
user app
"#;
        for (after, token, scope) in [
            ("# syntax", "syntax", "keyword.other.directive"),
            (
                "syntax=",
                "docker/dockerfile:1",
                "string.unquoted.directive",
            ),
            ("# A build", "build", "comment.line"),
            ("ARG", "ARG", "keyword.other.instruction"),
            ("ARG", "GO_VERSION", "variable.other.key"),
            ("FROM", "FROM", "keyword.other.instruction"),
            ("FROM", "--platform", "variable.parameter.flag"),
            ("--platform=", "$BUILDPLATFORM", "variable.other"),
            ("FROM", "golang", "support.class.image"),
            ("golang:", "${GO_VERSION}", "variable.other"),
            ("} AS", "AS", "keyword.other"),
            ("AS ", "build", "entity.name.type.stage"),
            ("ENV", "CGO_ENABLED", "variable.other.key"),
            ("PATH=", "/go/bin", "string.quoted.double"),
            ("/go/bin:", "$PATH", "variable.other"),
            ("RUN --mount", "--mount", "variable.parameter.flag"),
            // A comment line inside an instruction that goes on.
            ("# fetch", "fetch", "comment.line"),
            ("download ", "&&", "keyword.operator.shell"),
            ("&& ", "echo", "support.function.builtin"),
            ("echo \"", "done", "string.quoted.double"),
            ("|| exit", "exit", "support.function.builtin"),
            // A quote left open ends with its line.
            ("it's\n", "WORKDIR", "keyword.other.instruction"),
            ("--from=", "build", "entity.name.type.stage"),
            ("RUN <<", "<<", "keyword.operator.heredoc"),
            ("set -e", "set", "support.function.builtin"),
            ("in a heredoc", "heredoc", "string.quoted.double"),
            ("heredoc\"\n", "EOF", "entity.name.tag.heredoc"),
            ("listen", "listen", "string.unquoted.heredoc"),
            ("8080\n", "CONF", "entity.name.tag.heredoc"),
            ("EXPOSE", "EXPOSE", "keyword.other.instruction"),
            ("EXPOSE", "8080", "constant.numeric"),
            ("8080/", "tcp", "constant.language.protocol"),
            ("HEALTHCHECK", "--interval", "variable.parameter.flag"),
            ("30s ", "CMD", "keyword.other.instruction"),
            ("localhost/ ", "||", "keyword.operator.shell"),
            ("CMD [", "/app", "string.quoted.double"),
            (
                "LABEL",
                "org.opencontainers.image.source",
                "variable.other.key",
            ),
            ("com\" ", "version", "variable.other.key"),
            // Instructions are read in any case.
            ("user app", "user", "keyword.other.instruction"),
        ] {
            let found = scope_in("Dockerfile", text, after, token);
            assert!(found.contains(scope), "{token} after {after}: {found}");
        }
        // With Windows line breaks too: the heredoc still ends.
        let crlf = "RUN <<EOF\r\necho hi\r\nEOF\r\nUSER app\r\n";
        assert!(scope_in("Dockerfile", crlf, "USER", "USER").contains("keyword.other"));
        // Plain words stay plain: a path, a command's arguments.
        for (after, token) in [("COPY --from=build ", "/out/app"), ("go mod", "mod")] {
            let found = scope_in("Dockerfile", text, after, token);
            assert_eq!(found.trim(), "source.dockerfile", "{token}");
        }
    }

    #[test]
    fn grammars_are_found_by_file_name_too() {
        let syntax = Syntax::new(&config::Theme::default());
        let name = |file: &str| syntax.grammar(file).map(|(g, _)| g.name.clone());
        for file in [
            "Dockerfile",
            "dockerfile",
            "Containerfile",
            "Dockerfile.dev",
            "docker/api.Dockerfile",
        ] {
            assert_eq!(name(file).as_deref(), Some("Dockerfile"), "{file}");
        }
        assert_ne!(
            name("Dockerfile.dockerignore").as_deref(),
            Some("Dockerfile")
        );
        assert_eq!(name("Makefile").as_deref(), Some("Makefile"));
        assert_eq!(name("compose.yaml").as_deref(), Some("YAML"));
        assert_eq!(name("main.rs").as_deref(), Some("Rust"));
        // Shell scripts by every name they go by.
        for file in [
            ".zshrc",
            "PKGBUILD",
            "a.ksh",
            "t.bats",
            "x.command",
            ".envrc",
        ] {
            assert_eq!(
                name(file).as_deref(),
                Some("Bourne Again Shell (bash)"),
                "{file}"
            );
        }
        // The new grammars by their files' names.
        for (file, grammar) in [
            ("deploy.ps1", "PowerShell"),
            ("Module.psm1", "PowerShell"),
            (".env", "dotenv"),
            (".env.production", "dotenv"),
            ("app.env", "dotenv"),
            ("php.ini", "INI"),
            ("setup.cfg", "INI"),
            (".editorconfig", "INI"),
            (".gitconfig", "INI"),
            ("web.service", "INI"),
            (".gitignore", "Ignore List"),
            (".dockerignore", "Ignore List"),
            ("Dockerfile.dockerignore", "Ignore List"),
            ("site.scss", "SCSS"),
            ("main.tf", "HCL"),
            ("prod.tfvars", "HCL"),
            ("docker-bake.hcl", "HCL"),
            ("shop.proto", "Protocol Buffers"),
        ] {
            assert_eq!(name(file).as_deref(), Some(grammar), "{file}");
        }
        // .envrc is a shell script.
        assert_eq!(name(".envrc").as_deref(), Some("Bourne Again Shell (bash)"));
        for (token, grammar) in [
            ("powershell", "PowerShell"),
            ("pwsh", "PowerShell"),
            ("terraform", "HCL"),
            ("hcl", "HCL"),
            ("protobuf", "Protocol Buffers"),
            ("ini", "INI"),
            ("dotenv", "dotenv"),
            ("gitignore", "Ignore List"),
            ("scss", "SCSS"),
        ] {
            let found = syntax.by_token(token).map(|(g, _)| g.name.clone());
            assert_eq!(found.as_deref(), Some(grammar), "{token}");
        }
        // Markdown code blocks labeled for Docker, or as `shell`.
        assert!(syntax.by_token("shell").is_some());
        assert!(syntax.by_token("dockerfile").is_some());
        assert!(syntax.by_token("Containerfile").is_some());
    }

    #[test]
    fn scripts_without_an_extension_are_highlighted_by_their_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy");
        std::fs::write(&path, "echo hi\n").unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        let mut syntax = Syntax::new(&config::Theme::default());
        assert_eq!(syntax.language(&buffer), "Plain text");
        syntax.prepare(0, &buffer, 10);
        let styles = |syntax: &Syntax| {
            let mut seen: Vec<Style> = Vec::new();
            for (style, _) in &syntax.cache[&0].lines[1] {
                if !seen.contains(style) {
                    seen.push(*style);
                }
            }
            seen.len()
        };
        // Typing its #! line makes it a shell script, colored as one at once.
        buffer.set_index(0);
        buffer.insert("#!/usr/bin/env bash\n");
        assert_eq!(syntax.language(&buffer), "Bourne Again Shell (bash)");
        syntax.prepare(0, &buffer, 10);
        assert!(styles(&syntax) > 1, "echo is colored");
        // Other languages' #! lines too.
        std::fs::write(&path, "#!/usr/bin/env python3\nprint(1)\n").unwrap();
        assert_eq!(syntax.language(&Buffer::open(&path).unwrap()), "Python");
    }

    /// Every (after, token, scope) holds in `text` read as file `name`.
    fn scopes(name: &str, text: &str, expected: &[(&str, &str, &str)]) {
        for (after, token, scope) in expected {
            let found = scope_in(name, text, after, token);
            assert!(
                found.contains(scope),
                "{name}: {token} after {after}: {found}"
            );
        }
    }

    #[test]
    fn typescript_is_highlighted() {
        let text = r#"import { readFile } from "node:fs";
// A user.
export interface User { id: number; name?: string }
type Id = string | number;
enum Role { Admin = 1 }
@Component({ selector: "app" })
export class Store<T> extends Base implements Repo {
  private readonly items: Map<string, T> = new Map();
  async load(path: string): Promise<User[]> {
    const text = await readFile(path, "utf8");
    if (!text) return [];
    const ratio = total / count / 2;
    const words = text.split(/\s+/g);
    const f = <T,>(x: T) => x;
    const ids = useState<string>("");
    return JSON.parse(`${text}`) as User[];
  }
  #secret = 0x1F;
}
const MAX_SIZE = 10_000n;
"#;
        scopes(
            "store.ts",
            text,
            &[
                ("import", "import", "keyword.control.import"),
                ("from ", "\"node:fs\"", "string.quoted.double"),
                ("// A", "user", "comment.line"),
                ("export ", "interface", "keyword.declaration"),
                ("interface ", "User", "entity.name.type"),
                ("id: ", "number", "support.type.primitive"),
                ("type ", "Id", "entity.name.type"),
                ("enum ", "Role", "entity.name.type"),
                ("Admin = ", "1", "constant.numeric"),
                ("@", "@Component", "entity.name.function.decorator"),
                ("class ", "Store", "entity.name.type"),
                ("extends ", "Base", "support.class"),
                ("private", "readonly", "keyword.declaration"),
                ("= ", "new", "keyword.declaration"),
                ("async", "async", "keyword.declaration"),
                ("async ", "load", "variable.function"),
                (": ", "Promise", "support.class"),
                ("const", "const", "keyword.declaration"),
                ("= ", "await", "keyword.control"),
                ("await ", "readFile", "variable.function"),
                ("if", "if", "keyword.control"),
                ("text) ", "return", "keyword.control"),
                // Division, then a regular expression.
                ("total ", "/", "keyword.operator"),
                ("split(", "/", "string.regexp"),
                ("/\\s+/", "g", "keyword.other.flags"),
                // Generics, not JSX, in TypeScript.
                ("f = ", "<", "keyword.operator"),
                ("useState", "<", "keyword.operator"),
                ("`${", "text", "meta.interpolation"),
                ("JSON.parse(", "`", "string.template"),
                (") ", "as", "keyword.declaration"),
                ("#secret", "#secret", "variable.other.private"),
                ("= ", "0x1F", "constant.numeric"),
                ("const ", "MAX_SIZE", "constant.other"),
                ("= ", "10_000n", "constant.numeric"),
            ],
        );
    }

    #[test]
    fn javascript_and_tsx_read_jsx() {
        let text = r#"export default function App({ items }) {
  const [open, setOpen] = useState(false);
  return (
    <Layout title="Shop" onClose={() => setOpen(false)}>
      <ul className='list'>
        {items.map((item) => <li key={item.id}>{item.name} &amp; more</li>)}
      </ul>
      <>Don't panic</>
      <Modal.Body />
    </Layout>
  );
}
const after = 1 < 2;
"#;
        for name in ["App.jsx", "App.js", "App.tsx"] {
            scopes(
                name,
                text,
                &[
                    ("export ", "default", "keyword.control.import"),
                    ("function ", "App", "entity.name.function"),
                    ("<", "Layout", "support.class.component"),
                    ("Layout ", "title", "entity.other.attribute-name"),
                    ("title=", "\"Shop\"", "string.quoted.double"),
                    ("onClose={() => ", "setOpen", "variable.function"),
                    ("<", "ul", "entity.name.tag"),
                    ("ul ", "className", "entity.other.attribute-name"),
                    ("{items.", "map", "variable.function"),
                    ("=> <", "li", "entity.name.tag"),
                    ("{item.name} ", "&amp;", "constant.character.entity"),
                    ("Modal", ".Body", "support.class.component"),
                    ("</", "Layout", "entity.name.tag"),
                    // After the elements, code again.
                    ("const ", "after", "source"),
                    ("1 ", "<", "keyword.operator"),
                ],
            );
        }
        // Text isn't code: the apostrophe in it opens no string.
        for name in ["App.jsx", "App.tsx"] {
            let found = scope_in(name, text, "<>Don", "'t");
            assert!(!found.contains("string"), "{name}: {found}");
        }
        // JavaScript in Markdown, and by the names it goes by.
        let syntax = Syntax::new(&config::Theme::default());
        for (token, grammar) in [
            ("js", "JavaScript"),
            ("javascript", "JavaScript"),
            ("jsx", "JavaScript"),
            ("ts", "TypeScript"),
            ("typescript", "TypeScript"),
            ("tsx", "TSX"),
        ] {
            let found = syntax.by_token(token).map(|(g, _)| g.name.clone());
            assert_eq!(found.as_deref(), Some(grammar), "{token}");
        }
        for (file, grammar) in [
            ("a.mjs", "JavaScript"),
            ("a.cjs", "JavaScript"),
            ("a.mts", "TypeScript"),
            ("a.d.ts", "TypeScript"),
        ] {
            let found = syntax.grammar(file).map(|(g, _)| g.name.clone());
            assert_eq!(found.as_deref(), Some(grammar), "{file}");
        }
    }

    #[test]
    fn powershell_is_highlighted() {
        let text = r#"<# Deploys the site #>
[CmdletBinding()]
param([string]$Name = "web", [int]$Count = 2)
function Get-Site([string]$Path) {
    # one site
    $items = Get-ChildItem -Path $Path -Recurse
    if ($items.Count -gt 10MB) { return $null }
    foreach ($item in $items) {
        Write-Host "Found $($item.Name) in $env:TEMP"
    }
    $text = @"
Hello $Name
"@
    [Math]::Round(2.5)
}
"#;
        scopes(
            "deploy.ps1",
            text,
            &[
                ("<#", "Deploys", "comment.block"),
                ("[", "CmdletBinding", "entity.name.type.attribute"),
                ("param", "param", "keyword.other"),
                ("param(", "[string]", "storage.type"),
                ("[string]", "$Name", "variable.other"),
                ("= ", "\"web\"", "string.quoted.double"),
                ("function", "function", "keyword.other.function"),
                ("function ", "Get-Site", "entity.name.function"),
                ("# one", "site", "comment.line"),
                ("= ", "Get-ChildItem", "support.function.cmdlet"),
                ("Get-ChildItem ", "-Path", "variable.parameter"),
                ("if ", "if", "keyword.control"),
                ("Count ", "-gt", "keyword.operator.word"),
                ("-gt ", "10MB", "constant.numeric"),
                ("return ", "$null", "constant.language"),
                ("foreach", "foreach", "keyword.control"),
                ("Write-Host", "Write-Host", "support.function.cmdlet"),
                ("Found ", "$(", "punctuation.section.embedded"),
                ("$($item.", "Name", "meta.embedded"),
                ("in ", "$env:TEMP", "variable.other"),
                ("Hello", "Hello", "string.quoted.double.heredoc"),
                ("Hello ", "$Name", "variable.other"),
                ("[Math]", "::", "keyword.operator"),
            ],
        );
    }

    #[test]
    fn env_and_ini_files_are_highlighted() {
        let env = "# Database\nexport DB_HOST=localhost\nDB_PORT=5432 # default\nURL=\"postgres://${DB_HOST}\"\nKEY='a b'\n";
        scopes(
            ".env",
            env,
            &[
                ("# Database", "Database", "comment.line"),
                ("export", "export", "keyword.other"),
                ("export ", "DB_HOST", "variable.other.key"),
                ("DB_HOST=", "localhost", "string.unquoted"),
                ("5432 ", "default", "comment.line"),
                ("URL=", "postgres", "string.quoted.double"),
                ("postgres://", "${DB_HOST}", "variable.other.reference"),
                ("KEY=", "a b", "string.quoted.single"),
            ],
        );
        assert!(scope_in(".env.local", env, "export ", "DB_HOST").contains("variable.other.key"));
        let ini = "; settings\n[core]\neditor = vim\ntabs: 4\nenabled = true\n# note\n";
        scopes(
            "setup.cfg",
            ini,
            &[
                ("; settings", "settings", "comment.line"),
                ("[", "core", "entity.name.type.section"),
                ("editor", "editor", "variable.other.key"),
                ("editor ", "=", "keyword.operator.assignment"),
                ("= ", "vim", "string.unquoted"),
                ("tabs: ", "4", "constant.numeric"),
                ("enabled = ", "true", "constant.language"),
                ("# note", "note", "comment.line"),
            ],
        );
        assert!(scope_in(".editorconfig", ini, "[", "core").contains("entity.name.type.section"));
    }

    #[test]
    fn ignore_files_are_highlighted() {
        let text =
            "# Build output\n/target/\n**/*.log\n!keep.log\nfile[0-9].txt\n\\#not-a-comment\n";
        for name in [
            ".gitignore",
            ".dockerignore",
            ".npmignore",
            "Dockerfile.dockerignore",
        ] {
            scopes(
                name,
                text,
                &[
                    ("# Build", "output", "comment.line"),
                    ("/target/\n", "**", "keyword.operator.wildcard"),
                    ("**/", "*", "keyword.operator.wildcard"),
                    ("log\n", "!", "keyword.operator.negation"),
                    ("file", "[0-9]", "constant.other.character-class"),
                    ("txt\n", "\\#", "constant.character.escape"),
                ],
            );
        }
    }

    #[test]
    fn scss_is_highlighted() {
        let text = r#"@use "sass:math";
// Colors
$primary: #3366ff !default;
@mixin card($radius: 4px) {
  border-radius: $radius;
}
.nav {
  &:hover { color: darken($primary, 10%); }
  #logo > a.active, %plain {
    @include card(2px);
    margin: math.div(10px, 2) auto;
    font: { family: "Inter", sans-serif; }
  }
  @if $primary == red { display: none; } @else { display: block; }
  .item-#{$name} { width: calc(100% - 2rem); }
}
"#;
        scopes(
            "site.scss",
            text,
            &[
                ("@use", "@use", "keyword.other.at-rule"),
                ("@use ", "sass:math", "string.quoted.double"),
                ("// Colors", "Colors", "comment.line"),
                ("$primary", "$primary", "variable.other"),
                ("$primary: ", "#3366ff", "constant.other.color"),
                ("#3366ff ", "!default", "keyword.other.important"),
                ("@mixin ", "card", "entity.name.function"),
                (
                    "border-radius",
                    "border-radius",
                    "support.type.property-name",
                ),
                ("border-radius: ", "$radius", "variable.other"),
                (".nav", ".nav", "entity.name.type.class"),
                ("&", "&", "keyword.operator.parent"),
                ("&", ":hover", "entity.name.function.pseudo"),
                ("color: ", "darken", "support.function"),
                ("$primary, ", "10%", "constant.numeric"),
                ("  #logo", "#logo", "constant.other.id"),
                ("a", ".active", "entity.name.type.class"),
                (", ", "%plain", "entity.name.type.placeholder"),
                ("@include ", "card", "entity.name.function"),
                ("margin: math.div(", "10px", "constant.numeric"),
                ("font: { ", "family", "support.type.property-name"),
                ("@if", "@if", "keyword.control"),
                ("} ", "@else", "keyword.control"),
                (".item-", "#{", "punctuation.section.interpolation"),
                ("#{", "$name", "variable.other"),
                ("width: ", "calc", "support.function"),
            ],
        );
    }

    #[test]
    fn terraform_and_hcl_are_highlighted() {
        let text = r#"# Web server
resource "aws_instance" "web" {
  ami           = var.ami_id
  count         = 2
  tags = {
    Name = "web-${var.env}"
    Plan = "%{ if var.big }large%{ else }small%{ endif }"
  }
  lifecycle {
    create_before_destroy = true
  }
  user_data = <<-EOT
    echo ${local.greeting}
  EOT
}
locals {
  names = [for s in var.list : upper(s) if s != ""]
}
"#;
        scopes(
            "main.tf",
            text,
            &[
                ("# Web", "server", "comment.line"),
                ("resource", "resource", "keyword.other.block"),
                ("resource ", "\"aws_instance\"", "string.quoted.double"),
                ("ami", "ami", "variable.other.key"),
                ("= ", "var", "variable.language"),
                ("count", "2", "constant.numeric"),
                ("tags", "tags", "variable.other.key"),
                ("Name", "Name", "variable.other.key"),
                ("\"web-", "${", "punctuation.section.interpolation"),
                ("${", "var", "variable.language"),
                ("%{ ", "if", "keyword.control"),
                ("lifecycle", "lifecycle", "keyword.other.block"),
                ("create_before_destroy = ", "true", "constant.language"),
                ("<<-", "EOT", "entity.name.tag.heredoc"),
                ("echo", "echo", "string.unquoted.heredoc"),
                ("echo ${", "local", "variable.language"),
                ("  EOT\n}\n", "locals", "keyword.other.block"),
                ("[", "for", "keyword.control"),
                (": ", "upper", "support.function"),
                ("s ", "!=", "keyword.operator"),
            ],
        );
        // docker-bake.hcl and .tfvars too.
        assert!(scope_in("docker-bake.hcl", text, "resource", "resource")
            .contains("keyword.other.block"));
        assert!(scope_in("prod.tfvars", text, "ami", "ami").contains("variable.other.key"));
    }

    #[test]
    fn protocol_buffers_are_highlighted() {
        let text = r#"syntax = "proto3";
package shop.v1;
import "google/protobuf/timestamp.proto";
// An order.
message Order {
  string id = 1;
  repeated Item items = 2;
  google.protobuf.Timestamp created = 3;
  map<string, int32> counts = 4;
  enum Status {
    STATUS_UNSPECIFIED = 0;
  }
}
service Orders {
  rpc Get(GetRequest) returns (stream Order);
}
"#;
        scopes(
            "shop.proto",
            text,
            &[
                ("syntax", "syntax", "keyword.other"),
                ("= ", "\"proto3\"", "string.quoted.double"),
                ("package", "package", "keyword.other"),
                ("// An", "order", "comment.line"),
                ("message", "message", "keyword.other.declaration"),
                ("message ", "Order", "entity.name.type"),
                ("  ", "string", "storage.type"),
                ("id = ", "1", "constant.numeric"),
                ("repeated", "repeated", "storage.modifier"),
                ("repeated ", "Item", "support.type"),
                ("2;\n  ", "google.protobuf.Timestamp", "support.type"),
                ("map<string, ", "int32", "storage.type"),
                ("enum ", "Status", "entity.name.type"),
                ("    ", "STATUS_UNSPECIFIED", "constant.other.enum"),
                ("rpc ", "Get", "entity.name.function"),
                (") ", "returns", "keyword.other"),
                ("(", "stream", "keyword.other"),
            ],
        );
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
