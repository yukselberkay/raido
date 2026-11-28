//! Language-aware typing, like VS Code's language configurations: Enter indents
//! after a line that opens a block (`def f():`, `fn main() {`, `if x; then`) and
//! dedents after `return`; Enter between brackets puts the closer on its own
//! line; closers and Python's `else:` / `except:` move back under their opener;
//! brackets and quotes close themselves; Markdown lists continue. The rules are
//! instant and local: language servers answer too slowly for every keystroke and
//! mostly don't offer this, so they keep to completions, diagnostics and format on
//! save.
use crate::editor::Buffer;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Python,
    Rust,
    Go,
    /// C, C++, Objective-C.
    C,
    /// JavaScript and TypeScript.
    Js,
    /// Other curly-brace languages and data: Java, Kotlin, Swift, C#, PHP, CSS,
    /// JSON.
    Brace,
    Yaml,
    /// Docker Compose files: YAML, with Compose's keys to complete.
    Compose,
    /// Dockerfiles and Containerfiles.
    Docker,
    Toml,
    Shell,
    Lua,
    Ruby,
    Markdown,
    Make,
    /// x86, ARM and RISC-V assembly.
    Asm,
    Plain,
}

impl Lang {
    pub fn detect(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::Plain;
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if matches!(name, "Makefile" | "makefile" | "GNUmakefile") {
            return Self::Make;
        }
        if is_dockerfile(path) {
            return Self::Docker;
        }
        if is_compose(path) {
            return Self::Compose;
        }
        if shell(Some(path), "").is_some() {
            return Self::Shell;
        }
        if crate::asm::is_assembly(path) {
            return Self::Asm;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "py" | "pyi" | "pyw" => Self::Python,
            "rs" => Self::Rust,
            "go" => Self::Go,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "m" | "mm" => Self::C,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Self::Js,
            "java" | "kt" | "kts" | "swift" | "cs" | "php" | "css" | "scss" | "less" | "json"
            | "jsonc" | "dart" | "scala" | "zig" => Self::Brace,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "lua" => Self::Lua,
            "rb" | "rake" | "gemspec" => Self::Ruby,
            "md" | "markdown" => Self::Markdown,
            "mk" => Self::Make,
            _ => Self::Plain,
        }
    }
    /// The language of `buffer`: by its file's name, else, for a script without an
    /// extension, by its `#!` line.
    pub fn of(buffer: &Buffer) -> Self {
        let path = buffer.path.as_deref();
        match Self::detect(path) {
            Self::Plain if shell(path, &first_line(buffer)).is_some() => Self::Shell,
            lang => lang,
        }
    }
    fn comment(self) -> Option<&'static str> {
        match self {
            Self::Python
            | Self::Yaml
            | Self::Compose
            | Self::Docker
            | Self::Toml
            | Self::Shell
            | Self::Ruby
            | Self::Make => Some("#"),
            Self::Lua => Some("--"),
            Self::Rust | Self::Go | Self::C | Self::Js | Self::Brace => Some("//"),
            // Which mark an assembly file takes depends on its assembler.
            Self::Markdown | Self::Asm | Self::Plain => None,
        }
    }
    /// Characters that close themselves, as (opener, closer).
    fn pairs(self) -> &'static [(char, char)] {
        match self {
            // ' starts lifetimes in Rust.
            Self::Rust => &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"')],
            Self::Js | Self::Go | Self::Shell | Self::Markdown => &[
                ('(', ')'),
                ('[', ']'),
                ('{', '}'),
                ('"', '"'),
                ('\'', '\''),
                ('`', '`'),
            ],
            _ => &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')],
        }
    }
}

/// A Dockerfile by its name, as Docker and VS Code know them: `Dockerfile`,
/// `Containerfile`, `Dockerfile.dev`, `api.dockerfile` (not `Dockerfile.dockerignore`).
pub fn is_dockerfile(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    !name.ends_with(".dockerignore")
        && ["dockerfile", "containerfile"].iter().any(|kind| {
            name == *kind
                || name.starts_with(&format!("{kind}."))
                || name.ends_with(&format!(".{kind}"))
        })
}

/// A Docker Compose file by its name: `compose.yaml`, `docker-compose.yml`, and the
/// ones beside them (`compose.override.yaml`, `docker-compose.prod.yml`).
pub fn is_compose(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let Some(stem) = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"))
    else {
        return false;
    };
    stem == "compose" || stem.starts_with("compose.") || stem.contains("docker-compose")
}

/// The shell a script is written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// POSIX sh, dash, ash.
    Sh,
    Bash,
    /// ksh, mksh.
    Ksh,
    Zsh,
    Fish,
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Ksh => "ksh",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }
    /// Whether ShellCheck and the Bash language server read it: zsh and fish are
    /// languages of their own to them.
    pub fn checked(self) -> bool {
        !matches!(self, Self::Zsh | Self::Fish)
    }
}

/// The shell `path` is a script for: by its extension (`.sh`, `.bash`, `.zsh`), its
/// name (`.bashrc`, `.zshrc`, `PKGBUILD`), or, without an extension, the `#!` line
/// it starts with (`first_line`). A `#!` line names the shell of a `.sh` file too.
pub fn shell(path: Option<&Path>, first_line: &str) -> Option<Shell> {
    let by_shebang = shebang(first_line);
    let Some(path) = path else {
        return by_shebang;
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let by_name = match name.as_str() {
        ".bashrc" | ".bash_profile" | ".bash_login" | ".bash_logout" | ".bash_aliases"
        | ".bash_functions" | ".envrc" | "pkgbuild" | "apkbuild" => Some(Shell::Bash),
        ".profile" | ".xprofile" | ".xinitrc" | ".xsession" => Some(Shell::Sh),
        ".kshrc" | ".mkshrc" => Some(Shell::Ksh),
        ".zshrc" | ".zshenv" | ".zprofile" | ".zlogin" | ".zlogout" => Some(Shell::Zsh),
        _ => None,
    };
    let extension = path.extension().and_then(|e| e.to_str());
    let by_extension = match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("sh") => Some(Shell::Sh),
        Some("bash" | "bats" | "command") => Some(Shell::Bash),
        Some("ksh" | "mksh") => Some(Shell::Ksh),
        Some("zsh") => Some(Shell::Zsh),
        Some("fish") => Some(Shell::Fish),
        _ => None,
    };
    match by_name.or(by_extension) {
        Some(known) => Some(by_shebang.unwrap_or(known)),
        None if extension.is_none() => by_shebang,
        None => None,
    }
}

/// The shell a `#!` line runs: `#!/bin/bash`, `#!/usr/bin/env bash`,
/// `#!/usr/bin/env -S bash -e`.
fn shebang(line: &str) -> Option<Shell> {
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_string();
    let mut program = base(words.next()?);
    if program == "env" {
        program = base(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
    }
    Some(match program.as_str() {
        "sh" | "dash" | "ash" => Shell::Sh,
        "bash" => Shell::Bash,
        "ksh" | "mksh" | "ksh93" => Shell::Ksh,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        _ => return None,
    })
}

/// The start of `buffer`'s first line, enough for a `#!` line.
pub fn first_line(buffer: &Buffer) -> String {
    buffer.text.line(0).chars().take(200).collect()
}

impl Lang {
    /// The language's keywords and common built-ins, for completion without a
    /// language server.
    pub fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Python => &[
                "False",
                "None",
                "True",
                "and",
                "as",
                "assert",
                "async",
                "await",
                "break",
                "class",
                "continue",
                "def",
                "del",
                "elif",
                "else",
                "except",
                "finally",
                "for",
                "from",
                "global",
                "import",
                "lambda",
                "nonlocal",
                "not",
                "pass",
                "raise",
                "return",
                "try",
                "while",
                "with",
                "yield",
                "print",
                "range",
                "len",
                "self",
                "isinstance",
                "enumerate",
                "dict",
                "list",
                "str",
                "int",
                "float",
                "bool",
                "tuple",
                "set",
                "open",
                "super",
                "__init__",
                "__name__",
                "__main__",
            ],
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "else", "enum",
                "extern", "false", "for", "impl", "in", "let", "loop", "match", "mod", "move",
                "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
                "trait", "true", "type", "unsafe", "use", "where", "while", "Option", "Some",
                "None", "Result", "Ok", "Err", "String", "Vec", "Box", "println", "format",
                "derive", "Default", "Clone", "Debug",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
                "nil",
                "true",
                "false",
                "string",
                "int",
                "int64",
                "float64",
                "bool",
                "byte",
                "error",
                "make",
                "len",
                "append",
                "fmt",
                "Println",
                "Printf",
                "Errorf",
            ],
            Self::C => &[
                "auto",
                "break",
                "case",
                "char",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extern",
                "float",
                "for",
                "goto",
                "if",
                "include",
                "define",
                "ifdef",
                "ifndef",
                "endif",
                "int",
                "long",
                "register",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "typedef",
                "union",
                "unsigned",
                "void",
                "volatile",
                "while",
                "NULL",
                "printf",
                "malloc",
                "free",
                "class",
                "namespace",
                "public",
                "private",
                "protected",
                "template",
                "typename",
                "virtual",
                "nullptr",
                "std",
                "bool",
                "true",
                "false",
            ],
            Self::Js => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "null",
                "return",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
                "console",
                "interface",
                "type",
                "readonly",
                "string",
                "number",
                "boolean",
                "Promise",
                "Array",
                "Object",
            ],
            Self::Brace => &[
                "abstract",
                "boolean",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "else",
                "enum",
                "extends",
                "false",
                "final",
                "finally",
                "for",
                "fun",
                "func",
                "if",
                "implements",
                "import",
                "interface",
                "let",
                "new",
                "null",
                "override",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "static",
                "string",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "val",
                "var",
                "void",
                "while",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "readonly", "return", "then", "until", "while", "echo",
                "printf", "exit", "shift", "source", "select", "declare", "typeset", "unset",
                "trap", "shopt", "getopts", "read", "eval", "exec", "wait", "command", "builtin",
                "break", "continue", "true", "false", "pushd", "popd", "mapfile", "set",
            ],
            Self::Lua => &[
                "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto",
                "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
                "until", "while", "print", "require", "pairs", "ipairs", "table",
            ],
            Self::Ruby => &[
                "alias",
                "and",
                "begin",
                "break",
                "case",
                "class",
                "def",
                "defined?",
                "do",
                "else",
                "elsif",
                "end",
                "ensure",
                "false",
                "for",
                "if",
                "in",
                "module",
                "next",
                "nil",
                "not",
                "or",
                "redo",
                "rescue",
                "retry",
                "return",
                "self",
                "super",
                "then",
                "true",
                "undef",
                "unless",
                "until",
                "when",
                "while",
                "yield",
                "puts",
                "require",
                "attr_accessor",
            ],
            Self::Yaml | Self::Toml => &["true", "false", "null"],
            Self::Docker => &[
                "FROM",
                "AS",
                "RUN",
                "CMD",
                "ENTRYPOINT",
                "SHELL",
                "COPY",
                "ADD",
                "WORKDIR",
                "ENV",
                "ARG",
                "LABEL",
                "EXPOSE",
                "VOLUME",
                "USER",
                "HEALTHCHECK",
                "STOPSIGNAL",
                "ONBUILD",
            ],
            // The Compose file's own keys, and the values that are words.
            Self::Compose => &[
                "services",
                "networks",
                "volumes",
                "configs",
                "secrets",
                "include",
                "name",
                "image",
                "build",
                "context",
                "dockerfile",
                "args",
                "target",
                "command",
                "entrypoint",
                "environment",
                "env_file",
                "ports",
                "expose",
                "depends_on",
                "condition",
                "service_started",
                "service_healthy",
                "service_completed_successfully",
                "restart",
                "unless-stopped",
                "always",
                "on-failure",
                "healthcheck",
                "test",
                "interval",
                "timeout",
                "retries",
                "start_period",
                "container_name",
                "hostname",
                "working_dir",
                "user",
                "labels",
                "profiles",
                "platform",
                "pull_policy",
                "deploy",
                "replicas",
                "resources",
                "limits",
                "reservations",
                "cpus",
                "memory",
                "logging",
                "driver",
                "driver_opts",
                "options",
                "external",
                "extra_hosts",
                "cap_add",
                "cap_drop",
                "privileged",
                "read_only",
                "tmpfs",
                "ulimits",
                "init",
                "tty",
                "stdin_open",
                "stop_signal",
                "stop_grace_period",
                "extends",
                "develop",
                "watch",
                "action",
                "path",
                "sync",
                "rebuild",
                "true",
                "false",
                "null",
            ],
            Self::Markdown | Self::Make | Self::Asm | Self::Plain => &[],
        }
    }
}

/// Completions without a language server: the language's keywords and the words
/// in `texts` (your open files of that language) that start with `prefix` (case
/// ignored), the most used first, keywords before words.
pub fn suggestions(lang: Lang, texts: &[String], prefix: &str) -> Vec<(String, &'static str)> {
    use std::collections::HashMap;
    let lower = prefix.to_lowercase();
    let fits = |word: &str| word != prefix && word.to_lowercase().starts_with(&lower);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for text in texts {
        for word in text
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| w.chars().count() >= 3 && !w.starts_with(|c: char| c.is_ascii_digit()))
        {
            *counts.entry(word).or_default() += 1;
        }
    }
    // The word being typed counts once in the text; it isn't a suggestion.
    if let Some(n) = counts.get_mut(prefix) {
        *n = n.saturating_sub(1);
    }
    let keywords = lang.keywords();
    let mut out: Vec<(String, &'static str)> = keywords
        .iter()
        .filter(|k| fits(k))
        .map(|k| (k.to_string(), "keyword"))
        .collect();
    let mut words: Vec<(&str, usize)> = counts
        .into_iter()
        .filter(|(w, n)| *n > 0 && fits(w) && !keywords.contains(w))
        .collect();
    words.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then(a.0.cmp(b.0))
    });
    out.extend(
        words
            .into_iter()
            .take(40)
            .map(|(w, _)| (w.to_string(), "word")),
    );
    // Exact-case matches first.
    out.sort_by_key(|(w, _)| !w.starts_with(prefix));
    out
}

/// The rules for one buffer: its language and one level of indentation.
pub struct Rules {
    pub lang: Lang,
    /// One indentation level: a tab, or some spaces.
    pub unit: String,
}

impl Rules {
    /// The language from the file name; the indentation from the file itself (tabs,
    /// or the most common step between indented lines), else the language's
    /// convention, else `tab_width` spaces.
    pub fn new(buffer: &Buffer, tab_width: usize) -> Self {
        let lang = Lang::of(buffer);
        let unit = detect_unit(buffer).unwrap_or_else(|| match lang {
            Lang::Go | Lang::Make => "\t".into(),
            Lang::Python => "    ".into(),
            Lang::Yaml | Lang::Compose | Lang::Ruby => "  ".into(),
            _ => " ".repeat(tab_width.clamp(1, 8)),
        });
        Self { lang, unit }
    }
    /// Whether the line (the text before the cursor) opens an indented block.
    pub fn opens_block(&self, before: &str) -> bool {
        let code = self.strip_comment(before);
        let code = code.trim_end();
        if code.ends_with(['(', '[', '{']) {
            return true;
        }
        let words: Vec<&str> = code.split_whitespace().collect();
        let first = words.first().copied().unwrap_or_default();
        let last = words.last().copied().unwrap_or_default();
        match self.lang {
            Lang::Python | Lang::Yaml | Lang::Compose | Lang::Make => code.ends_with(':'),
            Lang::Shell => {
                matches!(last, "then" | "do" | "else")
                    || (first == "case" && last == "in")
                    // A case's pattern on a line of its own: `start)`, `-h|--help)`.
                    || (code.ends_with(')') && !code.contains('(') && code != ")")
            }
            // A label on a line of its own (`main:`, `.loop:`), and a macro.
            Lang::Asm => {
                let code = asm_code(before).trim();
                let lower = code.to_ascii_lowercase();
                let words: Vec<&str> = lower.split_whitespace().collect();
                is_label_line(code)
                    || matches!(
                        words.first().copied().unwrap_or_default(),
                        ".macro" | "%macro" | "%imacro" | ".rept" | ".irp" | ".irpc" | "%rep"
                    )
                    || matches!(words.get(1).copied(), Some("proc" | "macro"))
            }
            Lang::Lua => {
                matches!(last, "then" | "do" | "else" | "repeat")
                    || (code.contains("function") && !code.ends_with("end"))
            }
            Lang::Ruby => {
                let opener = matches!(
                    first,
                    "def"
                        | "class"
                        | "module"
                        | "if"
                        | "unless"
                        | "while"
                        | "until"
                        | "case"
                        | "begin"
                        | "else"
                        | "elsif"
                        | "when"
                        | "rescue"
                        | "ensure"
                        | "for"
                );
                (opener && !code.ends_with("end"))
                    || last == "do"
                    || (code.ends_with('|') && code.contains(" do |"))
            }
            _ => false,
        }
    }
    /// Whether the line ends its block, so the next one goes back a level
    /// (Python's `return`, `pass`, `break`, `continue`, `raise`).
    pub fn dedents_after(&self, before: &str) -> bool {
        let first = before.split_whitespace().next().unwrap_or_default();
        match self.lang {
            Lang::Python => matches!(first, "return" | "pass" | "break" | "continue" | "raise"),
            // A case's pattern ends with `;;` (or `;&`, `;;&`): the next one goes back.
            // Not after a pattern on one line, `a) run ;;`.
            Lang::Shell => {
                let code = self.strip_comment(before).trim_end();
                let one_line = match (code.find(')'), code.find('(')) {
                    (Some(close), Some(open)) => close < open,
                    (close, _) => close.is_some(),
                };
                (code.ends_with(";;") || code.ends_with(";&")) && !one_line
            }
            _ => false,
        }
    }
    fn closer(&self, open: char) -> Option<char> {
        self.lang
            .pairs()
            .iter()
            .find(|(o, _)| *o == open)
            .map(|(_, c)| *c)
    }
    fn is_closer(&self, c: char) -> bool {
        self.lang.pairs().iter().any(|(_, close)| *close == c)
    }
    /// `line` without a trailing comment (quotes are respected).
    fn strip_comment<'a>(&self, line: &'a str) -> &'a str {
        let Some(marker) = self.lang.comment() else {
            return line;
        };
        let mut quote = None;
        let mut escaped = false;
        for (at, c) in line.char_indices() {
            match quote {
                Some(q) => {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == q {
                        quote = None;
                    }
                }
                None if c == '"' || c == '\'' || c == '`' => quote = Some(c),
                // A shell comment starts a word: `${#list[@]}` and `a#b` aren't ones.
                None if line[at..].starts_with(marker)
                    && (self.lang != Lang::Shell
                        || line[..at]
                            .chars()
                            .next_back()
                            .is_none_or(char::is_whitespace)) =>
                {
                    return &line[..at]
                }
                None => {}
            }
        }
        line
    }
}

/// Tabs if most indented lines start with one; otherwise the most common change in
/// indentation between neighboring lines (2, 4 or 8 spaces).
fn detect_unit(buffer: &Buffer) -> Option<String> {
    let (mut tabs, mut spaces) = (0, 0);
    let mut steps = [0usize; 9];
    let mut previous = 0;
    for line in buffer.text.lines().take(2000) {
        let line: String = line.chars().take(200).collect();
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let width = line.chars().take_while(|c| *c == ' ').count();
        if width > 0 {
            spaces += 1;
        }
        let step = width.abs_diff(previous);
        if (2..=8).contains(&step) {
            steps[step] += 1;
        }
        previous = width;
    }
    if tabs > spaces && tabs > 0 {
        return Some("\t".into());
    }
    let (step, count) = [2, 4, 8]
        .iter()
        .map(|&s| (s, steps[s]))
        .max_by_key(|&(s, n)| (n, std::cmp::Reverse(s)))?;
    (count > 0).then(|| " ".repeat(step))
}

/// What the cursor line looks like: its indentation, and the text before and after
/// the cursor.
struct Line {
    start: usize,
    indent: String,
    before: String,
    after: String,
}

fn line(b: &Buffer) -> Line {
    let text = b.line(b.row);
    let text = text.trim_end_matches(['\n', '\r']);
    let before: String = text.chars().take(b.col).collect();
    let after: String = text.chars().skip(b.col).collect();
    let indent: String = text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    Line {
        start: b.text.line_to_char(b.row),
        indent,
        before,
        after,
    }
}

fn eol(b: &Buffer) -> &'static str {
    if b.crlf {
        "\r\n"
    } else {
        "\n"
    }
}

/// `indent` with one level taken off its end.
fn outdent(indent: &str, unit: &str) -> String {
    if let Some(rest) = indent.strip_suffix(unit) {
        return rest.to_string();
    }
    if let Some(rest) = indent.strip_suffix('\t') {
        return rest.to_string();
    }
    let spaces = indent.chars().rev().take_while(|c| *c == ' ').count();
    let cut = spaces.min(unit.len().max(1));
    indent[..indent.len() - cut].to_string()
}

/// The indentation for a new line below one whose text before the cursor is
/// `before`.
fn next_indent(rules: &Rules, current: &str, before: &str) -> String {
    if rules.opens_block(before) {
        format!("{current}{}", rules.unit)
    } else if rules.dedents_after(before.trim_start()) {
        outdent(current, &rules.unit)
    } else {
        current.to_string()
    }
}

/// The indentation for a new line below row `row` of a Dockerfile or shell script,
/// whose text before the cursor is `before`: an instruction or command that goes on
/// (its line ends in a backslash) indents the lines it goes on over by one level, and
/// the line after its last one goes back to where it starts.
fn continued_indent(b: &Buffer, row: usize, rules: &Rules, current: &str, before: &str) -> String {
    let goes_on = |text: &str| text.trim_end().ends_with('\\');
    let mut first = row;
    while first > 0 && goes_on(&b.line(first - 1)) {
        first -= 1;
    }
    if goes_on(before) {
        if first == row {
            format!("{current}{}", rules.unit)
        } else {
            current.to_string()
        }
    } else if first < row {
        b.line(first)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    } else {
        next_indent(rules, current, before)
    }
}

/// A Markdown list item: (indent + marker to repeat, whether the item is empty).
fn list_item(before: &str) -> Option<(String, bool)> {
    let trimmed = before.trim_start();
    let indent = &before[..before.len() - trimmed.len()];
    let (marker, rest) = if let Some(rest) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|m| trimmed.strip_prefix(m).map(|r| (m.to_string(), r)))
    {
        rest
    } else {
        let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
        let rest = trimmed[digits.len()..]
            .strip_prefix(". ")
            .or_else(|| trimmed[digits.len()..].strip_prefix(") "))?;
        let n: u64 = digits.parse().ok()?;
        let dot = &trimmed[digits.len()..digits.len() + 1];
        (format!("{}{dot} ", n + 1), rest)
    };
    let (task, rest) = match rest.get(..4) {
        Some("[ ] " | "[x] " | "[X] ") => ("[ ] ", &rest[4..]),
        _ => ("", rest),
    };
    Some((format!("{indent}{marker}{task}"), rest.trim().is_empty()))
}

/// Enter in insert mode.
pub fn enter(b: &mut Buffer, rules: &Rules) {
    if rules.lang == Lang::Shell {
        shell_dedent(b);
    }
    let Line {
        start,
        indent,
        before,
        after,
    } = line(b);
    let newline = eol(b);
    // A code block's opening fence with none to close it: Enter makes the closing
    // one too, the cursor on the line between, as Notion makes a code block.
    if rules.lang == Lang::Markdown && after.trim().is_empty() {
        let text = b.text.to_string();
        let lines: Vec<&str> = text.split('\n').collect();
        if crate::notes::unclosed(&lines, b.row) {
            let fence: String = before
                .trim_start()
                .chars()
                .take_while(|c| *c == '`' || *c == '~')
                .collect();
            let at = b.index();
            b.insert(&format!("{newline}{indent}{newline}{indent}{fence}"));
            b.set_index(at + newline.chars().count() + indent.chars().count());
            return;
        }
    }
    // Markdown lists go on; Enter on an empty item ends the list.
    if rules.lang == Lang::Markdown && after.trim().is_empty() {
        if let Some((marker, empty)) = list_item(&before) {
            if empty {
                let end = start + before.chars().count();
                b.replace(start, end, "");
                return;
            }
            b.insert(&format!("{newline}{marker}"));
            return;
        }
    }
    // Whitespace right after the cursor would only push the new line's text out.
    let blanks = after
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    if blanks > 0 {
        let i = b.index();
        b.replace(i, i + blanks, "");
        b.set_index(i);
    }
    let after = &after[after.len() - after.trim_start_matches([' ', '\t']).len()..];
    // On a line of nothing but indentation, drop it rather than leave it behind.
    let indent = if before.trim().is_empty() && after.is_empty() && !before.is_empty() {
        let end = start + before.chars().count();
        b.replace(start, end, "");
        before.clone()
    } else {
        indent.chars().take(before.chars().count()).collect()
    };
    let inner = match rules.lang {
        Lang::Docker | Lang::Shell => continued_indent(b, b.row, rules, &indent, &before),
        _ => next_indent(rules, &indent, &before),
    };
    // Between a bracket pair: the closer gets a line of its own.
    let last = before.trim_end().chars().last();
    let first = after.chars().next();
    if let (Some(open), Some(close)) = (last, first) {
        if rules.closer(open) == Some(close) && open != close {
            let at = b.index();
            b.insert(&format!("{newline}{inner}{newline}{indent}"));
            b.set_index(at + newline.chars().count() + inner.chars().count());
            return;
        }
    }
    b.insert(&format!("{newline}{inner}"));
}

/// `o` / `O` in normal mode: a new line below (indented as Enter at the line end
/// would) or above.
pub fn open_line(b: &mut Buffer, rules: &Rules, above: bool) {
    let row = b.row;
    let text = b.line(row);
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    b.open_line(above);
    if above {
        return;
    }
    let indent: String = text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let wanted = match rules.lang {
        Lang::Docker | Lang::Shell => continued_indent(b, row, rules, &indent, &text),
        _ => next_indent(rules, &indent, &text),
    };
    if wanted != indent {
        let start = b.text.line_to_char(b.row);
        b.replace(start, start + indent.chars().count(), &wanted);
    }
}

/// How a language comments a line out: a mark to the line's end, or a pair around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comment {
    Line(&'static str),
    Block(&'static str, &'static str),
}

/// How the file in `buffer` comments a line out, if its language has comments.
pub fn comment_marks(buffer: &Buffer) -> Option<Comment> {
    if let Some(asm) = crate::asm::of(buffer) {
        let mark = crate::asm::comment(asm, &crate::asm::sample(buffer));
        return Some(Comment::Line(mark));
    }
    let path = buffer.path.as_deref();
    let name = path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = path
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let by_extension = match ext.as_str() {
        "css" => Some(Comment::Block("/*", "*/")),
        "html" | "htm" | "xhtml" | "xml" | "svg" | "plist" | "md" | "markdown" | "mdx" => {
            Some(Comment::Block("<!--", "-->"))
        }
        "ml" | "mli" => Some(Comment::Block("(*", "*)")),
        "sql" | "hs" | "elm" | "ada" => Some(Comment::Line("--")),
        "ini" => Some(Comment::Line(";")),
        "clj" | "cljs" | "cljc" | "edn" | "el" | "lisp" | "scm" | "rkt" => Some(Comment::Line(";")),
        "erl" | "hrl" | "tex" | "sty" => Some(Comment::Line("%")),
        "vim" => Some(Comment::Line("\"")),
        "bat" | "cmd" => Some(Comment::Line("REM")),
        "proto" | "scss" | "less" | "jsonc" => Some(Comment::Line("//")),
        "ps1" | "psm1" | "psd1" | "env" | "tf" | "tfvars" | "hcl" | "nomad" | "r" | "pl" | "pm"
        | "cmake" | "conf" | "cfg" | "cnf" | "nix" | "ex" | "exs" | "jl" | "tcl" | "properties"
        | "service" | "socket" | "timer" | "desktop" | "gitignore" | "dockerignore" => {
            Some(Comment::Line("#"))
        }
        _ => None,
    };
    let by_name = (name.starts_with(".env")
        || name.ends_with("ignore")
        || matches!(
            name.as_str(),
            ".editorconfig" | ".gitconfig" | ".gitmodules" | ".gitattributes" | ".npmrc"
        ))
    .then_some(Comment::Line("#"));
    by_extension
        .or(by_name)
        .or_else(|| Lang::of(buffer).comment().map(Comment::Line))
}

/// Comment out lines `first..=last` of `b`, or, when every one of them that isn't
/// blank already is, comment them back in, as VS Code's Ctrl-/ does: the mark goes at
/// the lines' shared indentation, blank lines stay as they are, and it's one undo
/// step. The cursor stays on its text. False when there was nothing to do.
pub fn toggle_comment(b: &mut Buffer, first: usize, last: usize, comment: Comment) -> bool {
    let last = last.min(b.lines().saturating_sub(1));
    if first > last {
        return false;
    }
    let text = |row: usize| -> String { b.line(row).trim_end_matches(['\n', '\r']).to_string() };
    let lines: Vec<String> = (first..=last).map(text).collect();
    let indent = |l: &str| l.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let filled: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if filled.is_empty() {
        return false;
    }
    let commented = |l: &str| {
        let body = l.trim();
        match comment {
            Comment::Line(mark) => body.starts_with(mark),
            Comment::Block(open, close) => {
                body.len() >= open.len() + close.len()
                    && body.starts_with(open)
                    && body.ends_with(close)
            }
        }
    };
    let uncomment = filled.iter().all(|l| commented(l));
    let at = filled.iter().map(|l| indent(l)).min().unwrap_or(0);
    let changed: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.clone();
            }
            let lead: String = l.chars().take(indent(l)).collect();
            let body = &l[lead.len()..];
            match (comment, uncomment) {
                (Comment::Line(mark), true) => {
                    let rest = &body[mark.len()..];
                    format!("{lead}{}", rest.strip_prefix(' ').unwrap_or(rest))
                }
                (Comment::Block(open, close), true) => {
                    let inner = body.trim_end();
                    let inner = &inner[open.len()..inner.len() - close.len()];
                    let inner = inner.strip_prefix(' ').unwrap_or(inner);
                    format!("{lead}{}", inner.strip_suffix(' ').unwrap_or(inner))
                }
                (Comment::Line(mark), false) => {
                    let (head, tail): (String, String) =
                        (l.chars().take(at).collect(), l.chars().skip(at).collect());
                    format!("{head}{mark} {tail}")
                }
                (Comment::Block(open, close), false) => {
                    format!("{lead}{open} {} {close}", body.trim_end())
                }
            }
        })
        .collect();
    let (row, col) = (b.row, b.col);
    let start = b.text.line_to_char(first);
    let end = start
        + lines.iter().map(|l| l.chars().count()).sum::<usize>()
        + (last - first) * eol(b).chars().count();
    b.end_group();
    b.replace(start, end, &changed.join(eol(b)));
    b.end_group();
    // The cursor stays on its character: after the text a mark went in front of.
    b.row = row;
    if (first..=last).contains(&row) {
        let (old, new) = (&lines[row - first], &changed[row - first]);
        let delta = new.chars().count() as isize - old.chars().count() as isize;
        if col >= at.min(indent(old)) {
            b.col = col.saturating_add_signed(delta);
        }
    }
    b.clamp();
    true
}

/// Options for typing.
#[derive(Clone, Copy)]
pub struct Typing {
    pub auto_pairs: bool,
    pub smart_indent: bool,
}

/// A character typed in insert mode.
pub fn type_char(b: &mut Buffer, rules: &Rules, c: char, typing: Typing) {
    let i = b.index();
    let next = (i < b.text.len_chars()).then(|| b.text.char(i));
    let prev = (i > 0).then(|| b.text.char(i - 1));
    // Typing the closer that's already there steps over it.
    if typing.auto_pairs && rules.is_closer(c) && next == Some(c) {
        b.set_index(i + 1);
        return;
    }
    // Strings and comments that run to a closing mark close as you open them, so the
    // rest of the file doesn't turn into string or comment color while you write:
    // Python's third quote makes """|""", and /* makes /*| */.
    if typing.auto_pairs && next.is_none_or(char::is_whitespace) {
        let back = |n: usize| (i >= n).then(|| b.text.char(i - n));
        if rules.lang == Lang::Python
            && matches!(c, '"' | '\'')
            && back(1) == Some(c)
            && back(2) == Some(c)
            && back(3) != Some(c)
        {
            b.insert(&c.to_string().repeat(4));
            b.set_index(b.index() - 3);
            return;
        }
        // A Markdown fence's third backtick doesn't pair: ``` opens a code block,
        // and Enter closes it (see `enter`).
        let fence = back(1) == Some('`') && back(2) == Some('`') && {
            let here = line(b);
            here.before.trim_end_matches('`').trim().is_empty()
                && here.before.trim_start().chars().count() == 2
        };
        if rules.lang == Lang::Markdown && c == '`' && fence {
            b.insert("`");
            return;
        }
        let block_comments = matches!(
            rules.lang,
            Lang::C | Lang::Rust | Lang::Go | Lang::Js | Lang::Brace
        );
        if block_comments && c == '*' && prev == Some('/') {
            b.insert("* */");
            b.set_index(b.index() - 3);
            return;
        }
    }
    // A closing bracket on a blank line goes back under its opener.
    if typing.smart_indent && matches!(c, ')' | ']' | '}') {
        let here = line(b);
        if here.before.trim().is_empty() && !here.before.is_empty() {
            let target = opener_indent(b, c).unwrap_or_else(|| outdent(&here.indent, &rules.unit));
            let end = here.start + here.before.chars().count();
            b.replace(here.start, end, &target);
        }
    }
    if typing.auto_pairs {
        if let Some(close) = rules.closer(c) {
            let quote = c == close;
            let word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            let free_after = next.is_none_or(|n| n.is_whitespace() || ")]};,:.>".contains(n));
            // Quotes don't close after a word (don't), except Python's f"", r'', b"".
            let prefix = rules.lang == Lang::Python
                && prev.is_some_and(|p| "fFrRbBuU".contains(p))
                && !word((i > 1).then(|| b.text.char(i - 2)));
            let free_before = !quote || !word(prev) || prefix;
            if free_after && free_before && !(quote && word(next)) {
                b.insert(&format!("{c}{close}"));
                b.set_index(b.index() - 1);
                return;
            }
        }
    }
    b.insert(&c.to_string());
    // Python: `else:`, `elif x:`, `except E:`, `finally:` go back under their if/try.
    if typing.smart_indent && rules.lang == Lang::Python && c == ':' {
        python_dedent(b);
    }
    // Assembly: a label typed on an indented line goes back to where labels start.
    if typing.smart_indent && rules.lang == Lang::Asm && c == ':' {
        label_dedent(b);
    }
    // Shell: `fi`, `done`, `esac`, `else`, `elif` once the word is typed out.
    if typing.smart_indent && rules.lang == Lang::Shell && " ;&|<>".contains(c) {
        let here = line(b);
        let word = here.before.trim_start().strip_suffix(c).unwrap_or_default();
        if matches!(word, "fi" | "done" | "esac" | "else" | "elif") {
            shell_dedent(b);
        }
    }
}

/// An assembly line without its comment, roughly: `;`, `//`, `/*`, and `#` and
/// `@` at the start or after a space (not ARM's `#1` or Apple's `msg@PAGE`).
fn asm_code(line: &str) -> &str {
    let mut end = line.len();
    let mut after_space = true;
    for (at, c) in line.char_indices() {
        let comment = match c {
            ';' => true,
            '/' => line[at + 1..].starts_with(['/', '*']),
            '#' | '@' => after_space,
            _ => false,
        };
        if comment {
            end = at;
            break;
        }
        after_space = c.is_whitespace();
    }
    &line[..end]
}

/// An assembly line that's only a label: `main:`, `.L2:`, `1:`, NASM's `%%skip:`.
fn is_label_line(code: &str) -> bool {
    code.strip_suffix(':').is_some_and(|name| {
        !name.is_empty()
            && name
                .trim_start_matches("%%")
                .chars()
                .all(|c| c.is_alphanumeric() || "_.$?@".contains(c))
    })
}

/// Assembly: a label typed on an indented line of its own goes back to where the
/// labels above it start (the left edge, unless they're indented), out of the
/// instructions it follows.
fn label_dedent(b: &mut Buffer) {
    let here = line(b);
    if here.indent.is_empty() || !here.after.trim().is_empty() || !is_label_line(here.before.trim())
    {
        return;
    }
    let width = |s: &str| s.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let target: String = (0..b.row)
        .rev()
        .map(|row| b.line(row))
        .find(|text| is_label_line(asm_code(text).trim()))
        .map(|text| text.chars().take(width(&text)).collect())
        .unwrap_or_default();
    let ours = here.indent.chars().count();
    let theirs = target.chars().count();
    if theirs >= ours {
        return;
    }
    let col = b.col;
    b.replace(here.start, here.start + ours, &target);
    b.set_index(here.start + col - ours + theirs);
}

/// Shell: a line starting with `fi`, `else` or `elif`, `done` or `esac` goes back
/// under the `if`, loop or `case` it belongs to, found by matching blocks up from it
/// (one opened and closed on a line, `if x; then y; fi`, counts as neither).
fn shell_dedent(b: &mut Buffer) {
    let here = line(b);
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| c.is_whitespace() || ";&|()<>".contains(c))
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect()
    };
    let first = words(&here.before).into_iter().next().unwrap_or_default();
    let (openers, closer): (&[&str], &str) = match first.as_str() {
        "fi" | "else" | "elif" => (&["if"], "fi"),
        "done" => (&["for", "while", "until", "select"], "done"),
        "esac" => (&["case"], "esac"),
        _ => return,
    };
    let mut depth = 0usize;
    for row in (0..b.row).rev() {
        let text = b.line(row);
        let line_words = words(&text);
        let Some(word) = line_words.first() else {
            continue;
        };
        let opens = openers.contains(&word.as_str());
        if opens && line_words.iter().any(|w| w == closer) {
            continue;
        }
        if word == closer {
            depth += 1;
        } else if opens {
            if depth > 0 {
                depth -= 1;
                continue;
            }
            let target: String = text
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            if target != here.indent {
                let (ours, theirs) = (here.indent.chars().count(), target.chars().count());
                let col = b.col;
                b.replace(here.start, here.start + ours, &target);
                b.set_index(here.start + (col + theirs).saturating_sub(ours));
            }
            return;
        }
    }
}

/// The indentation of the line holding the bracket that `close` would match.
fn opener_indent(b: &Buffer, close: char) -> Option<String> {
    let open = match close {
        ')' => '(',
        ']' => '[',
        _ => '{',
    };
    let mut depth = 0usize;
    let mut i = b.index();
    let stop = i.saturating_sub(200_000);
    while i > stop {
        i -= 1;
        let c = b.text.char(i);
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                let row = b.text.char_to_line(i);
                let text = b.line(row);
                return Some(
                    text.chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .collect(),
                );
            }
            depth -= 1;
        }
    }
    None
}

fn python_dedent(b: &mut Buffer) {
    let here = line(b);
    let code = here.before.trim();
    let first = code
        .split(|c: char| c.is_whitespace() || c == ':')
        .next()
        .unwrap_or_default();
    let openers: &[&str] = match first {
        "else" => &["if", "elif", "for", "while", "try", "except"],
        "elif" => &["if", "elif"],
        "except" => &["try", "except"],
        "finally" => &["try", "except", "else"],
        _ => return,
    };
    let width = |s: &str| s.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let ours = width(&here.indent);
    // The nearest line above that's less indented: if it's the matching if / try,
    // line up with it.
    for row in (0..b.row).rev() {
        let text = b.line(row);
        if text.trim().is_empty() {
            continue;
        }
        let theirs = width(&text);
        if theirs < ours {
            let word = text
                .trim_start()
                .split(|c: char| c.is_whitespace() || c == ':')
                .next()
                .unwrap_or_default()
                .to_string();
            if openers.contains(&word.as_str()) {
                let target: String = text.chars().take(theirs).collect();
                let col = b.col;
                b.replace(
                    here.start,
                    here.start + here.indent.chars().count(),
                    &target,
                );
                b.set_index(here.start + col - ours + theirs);
            }
            return;
        }
    }
}

/// Backspace in insert mode: an empty pair goes as one, and in indentation made of
/// spaces it goes back a whole level.
pub fn backspace(b: &mut Buffer, rules: &Rules, typing: Typing) {
    let i = b.index();
    if i == 0 {
        return;
    }
    let prev = b.text.char(i - 1);
    let next = (i < b.text.len_chars()).then(|| b.text.char(i));
    if typing.auto_pairs && next.is_some() && rules.closer(prev) == next {
        b.replace(i - 1, i + 1, "");
        return;
    }
    if typing.smart_indent && rules.unit != "\t" {
        let here = line(b);
        if !here.before.is_empty() && here.before.chars().all(|c| c == ' ') {
            let width = rules.unit.len();
            let cut = match here.before.len() % width {
                0 => width,
                n => n,
            };
            b.replace(i - cut, i, "");
            return;
        }
    }
    b.backspace();
}

/// Tab in insert mode: one level of indentation.
pub fn tab(b: &mut Buffer, rules: &Rules) {
    if rules.unit == "\t" {
        b.insert("\t");
        return;
    }
    // Spaces up to the next indentation stop.
    let width = rules.unit.len();
    let col = line(b).before.chars().count();
    b.insert(&" ".repeat(width - col % width));
}

/// Shift-Tab in insert mode: the line goes back one level.
pub fn dedent(b: &mut Buffer, rules: &Rules) {
    let here = line(b);
    if here.indent.is_empty() {
        return;
    }
    let target = outdent(&here.indent, &rules.unit);
    let removed = here.indent.chars().count() - target.chars().count();
    let col = b.col;
    b.replace(
        here.start,
        here.start + here.indent.chars().count(),
        &target,
    );
    b.set_index(here.start + col.saturating_sub(removed));
}

/// Typing an opener over a selection wraps it: `(`, `[`, `{` and quotes.
pub fn surround(rules: &Rules, c: char, selected: &str) -> Option<String> {
    let close = rules.closer(c)?;
    Some(format!("{c}{selected}{close}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(name: &str, text: &str) -> Buffer {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        let mut b = Buffer::open(&path).unwrap();
        std::mem::forget(dir);
        b.row = b.lines().saturating_sub(1);
        b.col = b.line_len(b.row);
        b
    }
    const ON: Typing = Typing {
        auto_pairs: true,
        smart_indent: true,
    };
    fn typed(b: &mut Buffer, keys: &str) {
        let rules = Rules::new(b, 4);
        for c in keys.chars() {
            match c {
                '\n' => enter(b, &rules),
                '\u{8}' => backspace(b, &rules, ON),
                c => type_char(b, &rules, c, ON),
            }
        }
    }

    #[test]
    fn python_indents_blocks_and_dedents_after_return() {
        let mut b = buffer("a.py", "");
        typed(&mut b, "def testfunc():\nif x:  # check\nreturn 1\n");
        assert_eq!(
            b.text.to_string(),
            "def testfunc():\n    if x:  # check\n        return 1\n    "
        );
        // else: goes back under its if, wherever the cursor was.
        let mut b = buffer("b.py", "");
        typed(&mut b, "def f():\nif x:\npass\nelse:\nfoo()");
        assert_eq!(
            b.text.to_string(),
            "def f():\n    if x:\n        pass\n    else:\n        foo()"
        );
        let mut b = buffer("c.py", "");
        typed(&mut b, "try:\nfoo()\nexcept E:");
        assert_eq!(b.text.to_string(), "try:\n    foo()\nexcept E:");
    }

    #[test]
    fn braces_open_a_line_between_and_close_themselves() {
        let mut b = buffer("main.rs", "");
        typed(&mut b, "fn main() {\nlet v = vec![1];");
        assert_eq!(
            b.text.to_string(),
            "fn main() {\n    let v = vec![1];\n}",
            "{{ closed itself, Enter put }} on its own line, ] and ) stepped over"
        );
        // Go: tabs; a } typed on a blank line goes back under its opener.
        let mut b = buffer(
            "main.go",
            "package main\n\nfunc main() {\n\tif x {\n\t\tfoo()\n\t\t",
        );
        b.row = 5;
        b.col = 2;
        typed(&mut b, "}");
        assert!(
            b.text.to_string().ends_with("\t\tfoo()\n\t}"),
            "{:?}",
            b.text.to_string()
        );
        // Backspace between an empty pair takes both.
        let mut b = buffer("a.js", "");
        typed(&mut b, "call(\u{8}x");
        assert_eq!(b.text.to_string(), "callx");
    }

    #[test]
    fn docstrings_and_block_comments_close_as_they_open() {
        let mut b = buffer("a.py", "");
        typed(&mut b, "\"\"\"Docs.");
        assert_eq!(b.text.to_string(), "\"\"\"Docs.\"\"\"");
        // Typing the closing quotes steps over them.
        typed(&mut b, "\"\"\"\n");
        assert_eq!(b.text.to_string(), "\"\"\"Docs.\"\"\"\n");
        let mut b = buffer("a.py", "");
        typed(&mut b, "x = ''");
        assert_eq!(
            b.text.to_string(),
            "x = ''",
            "an empty string stays two quotes"
        );
        let mut b = buffer("a.c", "");
        typed(&mut b, "/* note");
        assert_eq!(b.text.to_string(), "/* note */");
        let mut b = buffer("a.rs", "");
        typed(&mut b, "/** Docs");
        assert_eq!(b.text.to_string(), "/** Docs */");
        let mut b = buffer("a.py", "");
        typed(&mut b, "x = 2 * 3 / 4");
        assert_eq!(b.text.to_string(), "x = 2 * 3 / 4");
    }

    #[test]
    fn quotes_close_only_where_a_string_can_start() {
        let mut b = buffer("a.py", "");
        typed(&mut b, "x = \"hi\" + f'{n}' + don't");
        assert_eq!(b.text.to_string(), "x = \"hi\" + f'{n}' + don't");
        let mut b = buffer("a.rs", "");
        typed(&mut b, "fn f<'a>(s: &'a str)");
        assert_eq!(
            b.text.to_string(),
            "fn f<'a>(s: &'a str)",
            "no pairs for lifetimes"
        );
        // Before a word, nothing closes.
        let mut b = buffer("a.c", "word");
        b.col = 0;
        typed(&mut b, "(");
        assert_eq!(b.text.to_string(), "(word");
    }

    #[test]
    fn shell_lua_ruby_yaml_and_markdown_rules() {
        let mut b = buffer("a.sh", "");
        typed(&mut b, "if true; then\necho hi");
        assert_eq!(b.text.to_string(), "if true; then\n    echo hi");
        let mut b = buffer("a.yaml", "");
        typed(&mut b, "jobs:\nbuild:");
        assert_eq!(b.text.to_string(), "jobs:\n  build:");
        let mut b = buffer("a.rb", "");
        typed(&mut b, "items.each do |i|\nputs i");
        assert_eq!(b.text.to_string(), "items.each do |i|\n  puts i");
        let mut b = buffer("a.md", "");
        // Enter on an empty item ends the list (the marker goes, the line stays).
        typed(&mut b, "- one\ntwo\n\n\n1. a\nb\n\n- [x] done\n");
        assert_eq!(
            b.text.to_string(),
            "- one\n- two\n\n1. a\n2. b\n- [x] done\n- [ ] "
        );
    }

    #[test]
    fn dockerfiles_and_compose_files_are_known_by_name() {
        for name in [
            "Dockerfile",
            "dockerfile",
            "Containerfile",
            "Dockerfile.dev",
            "api.Dockerfile",
            "web.containerfile",
        ] {
            assert_eq!(Lang::detect(Some(Path::new(name))), Lang::Docker, "{name}");
        }
        for name in [
            "compose.yaml",
            "compose.yml",
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.override.yaml",
            "docker-compose.prod.yml",
        ] {
            assert_eq!(Lang::detect(Some(Path::new(name))), Lang::Compose, "{name}");
        }
        for (name, lang) in [
            ("Dockerfile.dockerignore", Lang::Plain),
            ("values.yaml", Lang::Yaml),
            ("composer.json", Lang::Brace),
            ("compose.md", Lang::Markdown),
        ] {
            assert_eq!(Lang::detect(Some(Path::new(name))), lang, "{name}");
        }
    }

    #[test]
    fn dockerfile_instructions_indent_the_lines_they_go_on_over() {
        let mut b = buffer("Dockerfile", "");
        typed(
            &mut b,
            "FROM alpine\nRUN apk add \\\ncurl \\\ngit\nCMD [\n\"sh\"",
        );
        assert_eq!(
            b.text.to_string(),
            "FROM alpine\nRUN apk add \\\n    curl \\\n    git\nCMD [\n    \"sh\"\n]"
        );
        // `o` too: below an instruction that goes on, and below its last line.
        let mut b = buffer("Dockerfile", "RUN a \\\n    b\n");
        let rules = Rules::new(&b, 4);
        b.row = 0;
        open_line(&mut b, &rules, false);
        assert_eq!(b.text.to_string(), "RUN a \\\n    \n    b\n");
        let mut b = buffer("Dockerfile", "RUN a \\\n    b\n");
        b.row = 1;
        open_line(&mut b, &rules, false);
        assert_eq!(b.text.to_string(), "RUN a \\\n    b\n\n");
        // A comment line is a comment.
        let rules = Rules::new(&buffer("Dockerfile", ""), 4);
        assert_eq!(rules.lang.comment(), Some("#"));
    }

    #[test]
    fn compose_files_indent_as_yaml_and_complete_compose_keys() {
        let mut b = buffer("compose.yaml", "");
        typed(&mut b, "services:\nweb:\nimage: nginx");
        assert_eq!(b.text.to_string(), "services:\n  web:\n    image: nginx");
        let found = suggestions(Lang::Compose, &[], "dep");
        assert_eq!(found.first().map(|(w, _)| w.as_str()), Some("depends_on"));
        let found = suggestions(Lang::Docker, &[], "ent");
        assert_eq!(found.first().map(|(w, _)| w.as_str()), Some("ENTRYPOINT"));
    }

    #[test]
    fn shell_scripts_are_known_by_name_and_by_their_first_line() {
        let shell_of = |name: &str, first: &str| shell(Some(Path::new(name)), first);
        for (name, first, expected) in [
            ("deploy.sh", "", Some(Shell::Sh)),
            ("deploy.sh", "#!/usr/bin/env bash", Some(Shell::Bash)),
            ("a.bash", "", Some(Shell::Bash)),
            ("t.bats", "", Some(Shell::Bash)),
            ("a.ksh", "", Some(Shell::Ksh)),
            ("a.zsh", "", Some(Shell::Zsh)),
            ("config.fish", "", Some(Shell::Fish)),
            (".bashrc", "", Some(Shell::Bash)),
            (".zshrc", "", Some(Shell::Zsh)),
            (".profile", "", Some(Shell::Sh)),
            ("PKGBUILD", "", Some(Shell::Bash)),
            ("deploy", "#!/bin/bash", Some(Shell::Bash)),
            ("deploy", "#! /bin/sh -e", Some(Shell::Sh)),
            ("deploy", "#!/usr/bin/env -S bash -eu", Some(Shell::Bash)),
            ("deploy", "#!/usr/bin/env zsh", Some(Shell::Zsh)),
            ("deploy", "#!/bin/dash", Some(Shell::Sh)),
            ("deploy", "#!/usr/bin/env python3", None),
            ("deploy", "", None),
            ("notes.txt", "#!/bin/bash", None),
        ] {
            assert_eq!(shell_of(name, first), expected, "{name} {first}");
        }
        assert_eq!(shell(None, "#!/bin/bash\n"), Some(Shell::Bash));
        // A script without an extension types as shell too.
        let b = buffer("deploy", "#!/usr/bin/env bash\necho hi\n");
        assert_eq!(Lang::of(&b), Lang::Shell);
        assert_eq!(Lang::of(&buffer("notes", "hello\n")), Lang::Plain);
        assert_eq!(Lang::detect(Some(Path::new(".zshrc"))), Lang::Shell);
    }

    #[test]
    fn shell_blocks_indent_and_their_closers_line_up() {
        let typed_in = |name: &str, keys: &str| {
            let mut b = buffer(name, "");
            typed(&mut b, keys);
            b.text.to_string()
        };
        // if / else / fi, on Enter.
        assert_eq!(
            typed_in("a.sh", "if test -f x; then\necho a\nelse\necho b\nfi\n"),
            "if test -f x; then\n    echo a\nelse\n    echo b\nfi\n"
        );
        // elif once its word is typed out; nested blocks match their own opener.
        assert_eq!(
            typed_in("a.sh", "if a; then\nx\nelif b; then\ny\nfi\n"),
            "if a; then\n    x\nelif b; then\n    y\nfi\n"
        );
        assert_eq!(
            typed_in(
                "a.sh",
                "for f in *; do\nif test -d x; then\ncontinue\nfi\ndone\n"
            ),
            "for f in *; do\n    if test -d x; then\n        continue\n    fi\ndone\n"
        );
        // A loop's `do` on its own line, and `done <` closing it as it's typed.
        assert_eq!(
            typed_in("a.sh", "while read -r l\ndo\necho\ndone <"),
            "while read -r l\ndo\n    echo\ndone <"
        );
        // A one-line block between them counts as neither.
        assert_eq!(
            typed_in("a.sh", "if a; then\nif b; then c; fi\nfi\n"),
            "if a; then\n    if b; then c; fi\nfi\n"
        );
        // case: patterns indent their commands, ;; goes back to the patterns.
        assert_eq!(
            typed_in("a.sh", "case \"$1\" in\nstart)\nrun\n;;\n*)\nusage\n;;\nesac\n"),
            "case \"$1\" in\n    start)\n        run\n        ;;\n    *)\n        usage\n        ;;\nesac\n"
        );
        // A pattern and its commands on one line stay with the patterns.
        assert_eq!(
            typed_in("a.sh", "case x in\na) run ;;\nb)"),
            "case x in\n    a) run ;;\n    b)"
        );
        // `file` isn't `fi`.
        assert_eq!(
            typed_in("a.sh", "if x; then\nfile=1"),
            "if x; then\n    file=1"
        );
        // A command that goes on over lines ending in a backslash.
        assert_eq!(
            typed_in("a.sh", "apt-get install \\\ncurl \\\ngit\necho ok"),
            "apt-get install \\\n    curl \\\n    git\necho ok"
        );
        // Without an extension, by its #! line.
        let mut b = buffer("deploy", "#!/bin/sh\n");
        typed(&mut b, "if x; then\ny");
        assert_eq!(b.text.to_string(), "#!/bin/sh\nif x; then\n    y");
        // `#` starts a comment only as a word of its own.
        let rules = Rules::new(&buffer("a.sh", ""), 4);
        assert!(rules.opens_block("if [ ${#list[@]} -gt 0 ]; then"));
        assert!(rules.opens_block("if x; then # check"));
    }

    #[test]
    fn assembly_indents_under_labels_and_comments_out_as_its_assembler_does() {
        // Enter after a label indents; a label typed on an indented line goes back to
        // where the labels above it start.
        let mut b = buffer("start.s", "");
        typed(
            &mut b,
            "_start:\nmovq $60, %rax\nsyscall\nexit:  # done\nret",
        );
        assert_eq!(
            b.text.to_string(),
            "_start:\n    movq $60, %rax\n    syscall\nexit:  # done\n    ret"
        );
        let mut b = buffer("loop.asm", "    .L1:\n        dec ecx\n        ");
        typed(&mut b, ".L2:");
        assert_eq!(b.text.to_string(), "    .L1:\n        dec ecx\n    .L2:");
        // Not a label with an instruction after it, nor a segment in an operand.
        let rules = Rules::new(&buffer("a.s", ""), 4);
        assert!(rules.opens_block("main:"));
        assert!(rules.opens_block("1:"));
        assert!(rules.opens_block(".loop:  ; again"));
        assert!(rules.opens_block("%macro print 1"));
        assert!(!rules.opens_block("main: ret"));
        assert!(!rules.opens_block("    mov ax, es:"));
        assert_eq!(Lang::detect(Some(Path::new("crt0.S"))), Lang::Asm);
        // Ctrl-/ comments a line out with the mark its assembler reads.
        for (name, text, commented) in [
            (
                "boot.asm",
                "section .text\n    mov eax, 1\n",
                "section .text\n    ; mov eax, 1\n",
            ),
            (
                "a.s",
                "_start:\n    addi a0, zero, 1\n",
                "_start:\n    # addi a0, zero, 1\n",
            ),
            (
                "b.s",
                "_start:\n    mov x0, #1\n    ret\n",
                "_start:\n    // mov x0, #1\n    ret\n",
            ),
        ] {
            let mut b = buffer(name, text);
            let marks = comment_marks(&b).unwrap();
            toggle_comment(&mut b, 1, 1, marks);
            assert_eq!(b.text.to_string(), commented, "{name}");
        }
    }

    #[test]
    fn lines_are_commented_out_and_back_in() {
        let toggled = |name: &str, text: &str, first: usize, last: usize| {
            let mut b = buffer(name, text);
            let marks = comment_marks(&b).unwrap();
            toggle_comment(&mut b, first, last, marks);
            b.text.to_string()
        };
        // At the lines' shared indentation; blank lines left alone.
        let rust = "fn main() {\n    let a = 1;\n\n        a\n}\n";
        let out = toggled("a.rs", rust, 1, 3);
        assert_eq!(out, "fn main() {\n    // let a = 1;\n\n    //     a\n}\n");
        // And back, with the space after the mark.
        let mut b = buffer("a.rs", &out);
        toggle_comment(&mut b, 1, 3, Comment::Line("//"));
        assert_eq!(b.text.to_string(), rust);
        // Some lines commented and some not: all get commented.
        assert_eq!(toggled("a.py", "# x\ny\n", 0, 1), "# # x\n# y\n");
        assert_eq!(toggled("a.py", "#x\n# y\n", 0, 1), "x\ny\n");
        // Each language its own mark; a pair where there's no line comment.
        assert_eq!(toggled("a.lua", "x = 1\n", 0, 0), "-- x = 1\n");
        assert_eq!(toggled("a.sql", "select 1\n", 0, 0), "-- select 1\n");
        assert_eq!(toggled("Dockerfile", "RUN x\n", 0, 0), "# RUN x\n");
        assert_eq!(
            toggled("deploy", "#!/bin/sh\nrm -rf x\n", 1, 1),
            "#!/bin/sh\n# rm -rf x\n"
        );
        assert_eq!(
            toggled("a.html", "  <p>hi</p>\n", 0, 0),
            "  <!-- <p>hi</p> -->\n"
        );
        assert_eq!(
            toggled("a.html", "  <!-- <p>hi</p> -->\n", 0, 0),
            "  <p>hi</p>\n"
        );
        assert_eq!(
            toggled("a.css", "a { color: red; }\n", 0, 0),
            "/* a { color: red; } */\n"
        );
        assert_eq!(toggled("a.ini", "key = 1\n", 0, 0), "; key = 1\n");
        assert_eq!(toggled(".env.local", "KEY=1\n", 0, 0), "# KEY=1\n");
        // Windows line breaks stay.
        assert_eq!(toggled("a.rs", "a\r\nb\r\n", 0, 1), "// a\r\n// b\r\n");
        // The cursor stays on its character.
        let mut b = buffer("a.rs", "    let x = 1;\n");
        (b.row, b.col) = (0, 8);
        toggle_comment(&mut b, 0, 0, Comment::Line("//"));
        assert_eq!((b.row, b.col), (0, 11));
        assert_eq!(b.line(0).chars().nth(11), Some('x'));
        // Undone at once.
        b.undo(false);
        assert_eq!(b.text.to_string(), "    let x = 1;\n");
        // Plain text has no comments.
        assert_eq!(comment_marks(&buffer("notes.txt", "x\n")), None);
    }

    #[test]
    fn suggestions_come_from_keywords_and_your_words() {
        let texts =
            vec!["total_count = 1\ntotal_count += 2\ntotals = []\ntoken = 0\nto".to_string()];
        let found = suggestions(Lang::Python, &texts, "to");
        let words: Vec<&str> = found.iter().map(|(w, _)| w.as_str()).collect();
        assert_eq!(
            words,
            vec!["total_count", "token", "totals"],
            "most used, then shortest"
        );
        let found = suggestions(Lang::Python, &[], "ret");
        assert_eq!(found, vec![("return".to_string(), "keyword")]);
        // Case is ignored, but exact-case matches come first.
        let found = suggestions(Lang::Python, &["Total total".into()], "tot");
        assert_eq!(found[0].0, "total");
        assert!(suggestions(Lang::Plain, &[], "zz").is_empty());
    }

    #[test]
    fn indentation_comes_from_the_file() {
        let b = buffer("x.js", "a {\n  b {\n    c\n  }\n}\n");
        assert_eq!(Rules::new(&b, 4).unit, "  ");
        let b = buffer("x.py", "def f():\n\tpass\n");
        assert_eq!(Rules::new(&b, 4).unit, "\t");
        let b = buffer("x.go", "");
        assert_eq!(Rules::new(&b, 4).unit, "\t");
        // Tab goes to the next stop; Shift-Tab back a level.
        let mut b = buffer("x.py", "  ");
        let rules = Rules::new(&b, 4);
        tab(&mut b, &rules);
        assert_eq!(b.text.to_string(), "    ");
        dedent(&mut b, &rules);
        assert_eq!(b.text.to_string(), "");
    }
}
