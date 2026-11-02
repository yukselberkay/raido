//! What a change did to a file, read from the code itself without any AI: the
//! functions, types and sections it added, changed or removed, found with the
//! language's grammar (the one Raido highlights with), and the comment written
//! above each new one, which agents nearly always leave. The review shows it
//! under every file and sums it up at the end.
use crate::review::{Hunk, Kind};
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{Mutex, OnceLock},
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

/// Larger files aren't read for definitions.
const MAX_LINES: usize = 20_000;
/// How much of a definition's comment is kept.
const DOC: usize = 110;

/// A function, type, section… defined in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// 0-based: the line it starts on and the last line inside it.
    pub line: usize,
    pub end: usize,
    /// What the grammar calls it: function, struct, class, section…
    pub kind: String,
    /// With what it's in: `Grab::text`, `Parser.parse`.
    pub name: String,
    /// The first sentence of its comment.
    pub doc: Option<String>,
    /// A test, by its name or the attribute above it.
    pub test: bool,
}

/// What a change did to one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// Definitions that are new, tests left out.
    pub added: Vec<Definition>,
    /// Definitions whose insides changed.
    pub changed: Vec<String>,
    /// Definitions that are gone.
    pub removed: Vec<String>,
    /// Import lines added.
    pub imports: Vec<String>,
    /// Tests added.
    pub tests: usize,
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The kinds of `entity.name.*` that are definitions worth naming; the ones that
/// hold others come first.
const CONTAINERS: [&str; 9] = [
    "impl",
    "class",
    "struct",
    "trait",
    "interface",
    "module",
    "namespace",
    "enum",
    "union",
];
const KINDS: [&str; 4] = ["function", "type", "section", "table"];

/// Every definition in `text`, by the grammar for a file named `name`.
pub fn definitions(name: &str, text: &str) -> Vec<Definition> {
    let set = syntaxes();
    let Some(grammar) = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| set.find_syntax_by_extension(e))
    else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() > MAX_LINES {
        return Vec::new();
    }
    let comment = Scope::new("comment").expect("scope");
    let kinds: Vec<(Scope, &str)> = CONTAINERS
        .iter()
        .chain(&KINDS)
        .map(|kind| {
            (
                Scope::new(&format!("entity.name.{kind}")).expect("scope"),
                *kind,
            )
        })
        .collect();
    let mut parse = ParseState::new(grammar);
    let mut stack = ScopeStack::new();
    // Per line: the first definition named on it, and whether it's only a comment.
    let mut found: Vec<Option<(&str, String)>> = Vec::with_capacity(lines.len());
    let mut remarks = Vec::with_capacity(lines.len());
    for line in &lines {
        let ops = if line.len() > 16_384 {
            Vec::new()
        } else {
            parse.parse_line(line, set).unwrap_or_default()
        };
        let (mut name, mut kind, mut naming) = (String::new(), "", true);
        let (mut remark, mut code) = (false, false);
        let mut at = 0;
        let mut piece = |stack: &ScopeStack, text: &str| {
            if text.trim().is_empty() {
                return;
            }
            let scopes = stack.as_slice();
            if scopes.iter().any(|s| comment.is_prefix_of(*s)) {
                remark = true;
            } else {
                code = true;
            }
            let entity = scopes
                .iter()
                .rev()
                .find_map(|s| kinds.iter().find(|(k, _)| k.is_prefix_of(*s)));
            match entity {
                Some((_, k)) if naming && (kind.is_empty() || kind == *k) => {
                    kind = *k;
                    name.push_str(text.trim());
                }
                _ if !name.is_empty() => naming = false,
                _ => {}
            }
        };
        for (offset, op) in ops {
            let offset = offset.min(line.len());
            if offset > at {
                piece(&stack, &line[at..offset]);
                at = offset;
            }
            let _ = stack.apply(&op);
        }
        piece(&stack, &line[at..]);
        found.push((!name.is_empty()).then_some((kind, name)));
        remarks.push(remark && !code);
    }
    let indent = |line: &str| {
        line.chars()
            .take_while(|c| c.is_whitespace() && *c != '\n')
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum::<usize>()
    };
    let blank = |line: &str| line.trim().is_empty();
    let separator = match name.rsplit('.').next() {
        Some("rs" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "h") => "::",
        _ => ".",
    };
    let mut out: Vec<Definition> = Vec::new();
    for (line, entry) in found.iter().enumerate() {
        let Some((kind, name)) = entry else {
            continue;
        };
        // It lasts while lines are indented deeper, through a brace on its own
        // level, and ends at the closing one.
        let depth = indent(lines[line]);
        let mut end = line;
        for (k, text) in lines.iter().enumerate().skip(line + 1) {
            if blank(text) {
                continue;
            }
            let trimmed = text.trim();
            if indent(text) > depth || (indent(text) == depth && trimmed.starts_with('{')) {
                end = k;
            } else {
                if indent(text) == depth
                    && (trimmed.starts_with('}') || trimmed.starts_with(')') || trimmed == "end")
                {
                    end = k;
                }
                break;
            }
        }
        // A helper inside a function (a closure, a nested def) is that function's
        // business.
        let around = out.iter().rev().find(|d| d.line < line && line <= d.end);
        if around.is_some_and(|d| d.kind == "function") {
            continue;
        }
        // What it's in: the nearest container around it.
        let within = out
            .iter()
            .rev()
            .find(|d| d.line < line && line <= d.end && CONTAINERS.contains(&d.kind.as_str()));
        let full = match within {
            Some(outer) if *kind != "section" => format!("{}{separator}{name}", outer.name),
            _ => name.clone(),
        };
        // Its comment: the comment lines just above (past attributes and
        // decorators), or a docstring just below.
        let mut above = Vec::new();
        let mut attributes = Vec::new();
        let mut k = line;
        while k > 0 {
            k -= 1;
            let text = lines[k].trim();
            if remarks[k] {
                above.push(text);
            } else if above.is_empty() && (text.starts_with("#[") || text.starts_with('@')) {
                attributes.push(text);
            } else {
                break;
            }
        }
        above.reverse();
        if above.is_empty() && remarks.get(line + 1) == Some(&true) {
            above.push(lines[line + 1].trim());
        }
        let last = full.rsplit(separator).next().unwrap_or(&full);
        let test = *kind == "function"
            && (attributes.iter().any(|a| a.to_lowercase().contains("test"))
                || last.starts_with("test")
                || last.starts_with("Test")
                || full.contains(&format!("tests{separator}")));
        out.push(Definition {
            line,
            end,
            kind: kind.to_string(),
            name: full,
            doc: summary(&above),
            test,
        });
    }
    out
}

/// The first sentence of comment lines, without their markers.
fn summary(lines: &[&str]) -> Option<String> {
    let mut text = String::new();
    for line in lines {
        let mut line = line.trim();
        for marker in [
            "///", "//!", "//", "/**", "/*", "*/", "#", "--", "\"\"\"", "'''", "*",
        ] {
            line = line.trim_start_matches(marker).trim();
        }
        for marker in ["*/", "\"\"\"", "'''"] {
            line = line.trim_end_matches(marker).trim();
        }
        if line.is_empty() {
            if text.is_empty() {
                continue;
            }
            break;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line);
        if text.contains(". ") || text.ends_with('.') {
            break;
        }
    }
    if let Some(end) = text.find(". ") {
        text.truncate(end + 1);
    }
    if text.chars().count() > DOC {
        text = text.chars().take(DOC - 1).collect::<String>() + "…";
    }
    (!text.is_empty()).then_some(text)
}

/// Lines that bring in code from elsewhere, in most languages.
fn import(line: &str) -> bool {
    let line = line.trim_start();
    [
        "use ", "pub use ", "import ", "from ", "#include", "using ", "require ",
    ]
    .iter()
    .any(|p| line.starts_with(p))
        || line.contains("= require(")
}

/// What the change from `old` to `new` (with these hunks) did to file `name`.
pub fn outline(name: &str, old: &str, new: &str, hunks: &[Hunk]) -> Outline {
    static CACHE: OnceLock<Mutex<HashMap<u64, Outline>>> = OnceLock::new();
    let key = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (name, old, new).hash(&mut hasher);
        hasher.finish()
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return found;
    }
    let before = definitions(name, old);
    let after = definitions(name, new);
    let id = |d: &Definition| (d.kind.clone(), d.name.clone());
    let had: HashSet<_> = before.iter().map(id).collect();
    let has: HashSet<_> = after.iter().map(id).collect();
    let mut out = Outline::default();
    let mut seen = HashSet::new();
    for d in &after {
        if !had.contains(&id(d)) && seen.insert(id(d)) {
            if d.test {
                out.tests += 1;
            } else {
                out.added.push(d.clone());
            }
        }
    }
    let mut seen = HashSet::new();
    for d in &before {
        if !has.contains(&id(d)) && !d.test && seen.insert(id(d)) {
            out.removed.push(format!("{} {}", d.kind, d.name));
        }
    }
    // The innermost definition around each changed line, if it's still there.
    let inside = |defs: &[Definition], line: usize| {
        defs.iter()
            .filter(|d| d.line <= line && line <= d.end)
            .max_by_key(|d| d.line)
            .map(id)
    };
    let mut seen = HashSet::new();
    for line in hunks.iter().flat_map(|h| &h.lines) {
        let around = match line.kind {
            Kind::Added => line.new.and_then(|n| inside(&after, n - 1)),
            Kind::Removed => line.old.and_then(|n| inside(&before, n - 1)),
            Kind::Context => None,
        };
        if let Some(around) = around.filter(|a| had.contains(a) && has.contains(a)) {
            if seen.insert(around.clone()) {
                out.changed.push(around.1);
            }
        }
        if line.kind == Kind::Added && import(&line.text) {
            out.imports
                .push(line.text.trim().trim_end_matches(';').to_string());
        }
    }
    if let Ok(mut cache) = cache.lock() {
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key, out.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_definitions_with_their_comments_and_what_holds_them() {
        let text = "\
use std::fs;

/// Opens the door. Then more.
#[inline]
pub fn open(door: &str) -> bool {
    true
}

struct Grab {
    x: u16,
}

impl Grab {
    // Where the drag is now.
    fn at(&self) -> u16 {
        self.x
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opens() {}
}
";
        let defs = definitions("x.rs", text);
        let names: Vec<_> = defs
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("function", "open"),
                ("struct", "Grab"),
                ("impl", "Grab"),
                ("function", "Grab::at"),
                ("module", "tests"),
                ("function", "tests::opens"),
            ]
        );
        assert_eq!(defs[0].doc.as_deref(), Some("Opens the door."));
        assert_eq!((defs[0].line, defs[0].end), (4, 6));
        assert_eq!(defs[3].doc.as_deref(), Some("Where the drag is now."));
        assert!(defs[5].test && !defs[0].test);
    }

    #[test]
    fn helpers_inside_a_function_are_left_out() {
        let text = "fn find() {\n    let count = |c: char| c;\n    fn inner() {}\n}\n";
        let names: Vec<_> = definitions("x.rs", text)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["find"]);
    }

    #[test]
    fn python_docstrings_and_go_comments_count_too() {
        let python = "class Door:\n    def open(self):\n        \"\"\"Swing it wide.\"\"\"\n        return 1\n";
        let defs = definitions("door.py", python);
        assert_eq!(defs[1].name, "Door.open");
        assert_eq!(defs[1].doc.as_deref(), Some("Swing it wide."));
        let go = "package door\n\n// Open swings the door.\nfunc Open() int {\n\treturn 1\n}\n";
        let defs = definitions("door.go", go);
        assert_eq!(defs[0].name, "Open");
        assert_eq!(defs[0].doc.as_deref(), Some("Open swings the door."));
        assert!(definitions("notes.txt", "fn x() {}\n").is_empty());
    }

    #[test]
    fn an_outline_says_what_was_added_changed_and_removed() {
        let old = "fn keep() {\n    1;\n}\n\nfn gone() {}\n";
        let new = "use std::io;\n\nfn keep() {\n    2;\n}\n\n/// Says hi.\nfn hello() {}\n\n#[test]\nfn hello_works() {}\n";
        let (before, after): (Vec<_>, Vec<_>) = (
            old.split_inclusive('\n').collect(),
            new.split_inclusive('\n').collect(),
        );
        let hunks = crate::review::hunks(&before, &after);
        let outline = outline("a.rs", old, new, &hunks);
        assert_eq!(outline.added.len(), 1);
        assert_eq!(outline.added[0].name, "hello");
        assert_eq!(outline.added[0].doc.as_deref(), Some("Says hi."));
        assert_eq!(outline.changed, ["keep"]);
        assert_eq!(outline.removed, ["function gone"]);
        assert_eq!(outline.imports, ["use std::io"]);
        assert_eq!(outline.tests, 1);
    }
}
