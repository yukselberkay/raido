//! Writing notes in Markdown, as in Notion: a `/` menu of blocks (headings, lists,
//! to-dos, quotes, callouts, code blocks, tables, dividers, links, the date), and
//! suggestions while you write: a code block's language after its fence, your
//! notes and their headings after a link's `](`, a table's next cell with Tab.
//! What goes in the note is plain Markdown; these are the rules for writing it,
//! and `App` puts them in the editor's completion menu and its keys.
use std::path::Path;
use unicode_width::UnicodeWidthStr;

/// Where the cursor goes in a block's template.
const CURSOR: char = '‸';

/// What a block of the `/` menu writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// The line becomes this kind of line: its marker (`# `, `- [ ] `), or none.
    Line(&'static str),
    /// Lines of their own, `‸` where the cursor goes.
    Lines(&'static str),
    /// Text where the `/` was, `‸` where the cursor goes.
    Inline(&'static str),
    /// Today's date where the `/` was.
    Date,
    /// A list of links to the note's headings.
    Contents,
}

/// A block of the `/` menu.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub label: &'static str,
    /// What it writes, shown beside it.
    pub detail: &'static str,
    /// Other words it's found by.
    pub words: &'static str,
    pub insert: Insert,
}

/// The `/` menu, in the order it's shown.
pub const BLOCKS: &[Block] = &[
    Block {
        label: "Text",
        detail: "plain text",
        words: "paragraph plain",
        insert: Insert::Line(""),
    },
    Block {
        label: "Heading 1",
        detail: "# big heading",
        words: "h1 title",
        insert: Insert::Line("# "),
    },
    Block {
        label: "Heading 2",
        detail: "## section",
        words: "h2 section subtitle",
        insert: Insert::Line("## "),
    },
    Block {
        label: "Heading 3",
        detail: "### small heading",
        words: "h3",
        insert: Insert::Line("### "),
    },
    Block {
        label: "Bulleted list",
        detail: "- item",
        words: "bullet ul unordered list",
        insert: Insert::Line("- "),
    },
    Block {
        label: "Numbered list",
        detail: "1. item",
        words: "number ol ordered list",
        insert: Insert::Line("1. "),
    },
    Block {
        label: "To-do list",
        detail: "- [ ] task",
        words: "todo task checkbox check",
        insert: Insert::Line("- [ ] "),
    },
    Block {
        label: "Quote",
        detail: "> quote",
        words: "blockquote",
        insert: Insert::Line("> "),
    },
    Block {
        label: "Code block",
        detail: "``` language",
        words: "snippet fence pre",
        insert: Insert::Lines("```‸\n\n```"),
    },
    Block {
        label: "Table",
        detail: "| a | b |",
        words: "grid columns",
        insert: Insert::Lines("| ‸ |  |\n| --- | --- |\n|  |  |"),
    },
    Block {
        label: "Callout · Note",
        detail: "> [!NOTE]",
        words: "callout info alert",
        insert: Insert::Lines("> [!NOTE]\n> ‸"),
    },
    Block {
        label: "Callout · Tip",
        detail: "> [!TIP]",
        words: "callout hint alert",
        insert: Insert::Lines("> [!TIP]\n> ‸"),
    },
    Block {
        label: "Callout · Important",
        detail: "> [!IMPORTANT]",
        words: "callout alert",
        insert: Insert::Lines("> [!IMPORTANT]\n> ‸"),
    },
    Block {
        label: "Callout · Warning",
        detail: "> [!WARNING]",
        words: "callout alert",
        insert: Insert::Lines("> [!WARNING]\n> ‸"),
    },
    Block {
        label: "Callout · Caution",
        detail: "> [!CAUTION]",
        words: "callout danger alert",
        insert: Insert::Lines("> [!CAUTION]\n> ‸"),
    },
    Block {
        label: "Divider",
        detail: "---",
        words: "rule hr line separator",
        insert: Insert::Lines("---\n‸"),
    },
    Block {
        label: "Toggle",
        detail: "<details>",
        words: "details collapse fold spoiler",
        insert: Insert::Lines("<details>\n<summary>‸</summary>\n\n</details>"),
    },
    Block {
        label: "Math block",
        detail: "$$ … $$",
        words: "equation latex formula",
        insert: Insert::Lines("$$\n‸\n$$"),
    },
    Block {
        label: "Link",
        detail: "[text](url)",
        words: "url",
        insert: Insert::Inline("[‸]()"),
    },
    Block {
        label: "Image",
        detail: "![alt](path)",
        words: "img picture photo",
        insert: Insert::Inline("![‸]()"),
    },
    Block {
        label: "Date",
        detail: "today",
        words: "today now",
        insert: Insert::Date,
    },
    Block {
        label: "Table of contents",
        detail: "links to the headings",
        words: "toc contents outline",
        insert: Insert::Contents,
    },
];

/// The block of the `/` menu labelled `label`.
pub fn block(label: &str) -> Option<&'static Block> {
    BLOCKS.iter().find(|b| b.label == label)
}

/// Whether a `/` typed at column `slash` of `line` opens the menu: at the start of
/// the line's text (after its indentation and any list or quote marker) or after
/// a space, and not in inline code.
pub fn opens_menu(line: &str, slash: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    if chars.get(slash) != Some(&'/') {
        return false;
    }
    let before: String = chars[..slash].iter().collect();
    let ticks = before.matches('`').count();
    let after_space = slash == 0 || chars[slash - 1].is_whitespace();
    after_space && ticks.is_multiple_of(2)
}

/// An edit to a line: characters `from..to` of it become `text`, the cursor
/// `cursor` characters into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub from: usize,
    pub to: usize,
    pub text: String,
    pub cursor: usize,
}

/// The line's indentation and the marks that start it (`- `, `1. `, `> `, `## `,
/// `- [ ] `): their length in characters, and how much of it is indentation.
fn lead(line: &str) -> (usize, usize) {
    let chars: Vec<char> = line.chars().collect();
    let indent = chars
        .iter()
        .take_while(|c| **c == ' ' || **c == '\t')
        .count();
    let mut i = indent;
    let skip = |mut i: usize| {
        while chars.get(i) == Some(&' ') {
            i += 1;
        }
        i
    };
    while chars.get(i) == Some(&'>') {
        i = skip(i + 1);
    }
    let hashes = chars[i..].iter().take_while(|c| **c == '#').count();
    let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=6).contains(&hashes) && chars.get(i + hashes) == Some(&' ') {
        i = skip(i + hashes);
    } else if matches!(chars.get(i), Some('-' | '*' | '+')) && chars.get(i + 1) == Some(&' ') {
        i = skip(i + 1);
    } else if digits > 0
        && matches!(chars.get(i + digits), Some('.' | ')'))
        && chars.get(i + digits + 1) == Some(&' ')
    {
        i = skip(i + digits + 1);
    }
    let rest: String = chars[i.min(chars.len())..].iter().take(4).collect();
    if matches!(rest.as_str(), "[ ] " | "[x] " | "[X] ") {
        i = skip(i + 3);
    }
    (i.min(chars.len()), indent)
}

/// What picking `block` writes, for a `/` at column `slash` of `line` and the
/// cursor at `col` (after what was typed to find it). As in Notion, a line with
/// nothing else on it becomes the block; one with text keeps it, and the block
/// goes on the lines after it. `today` is the date, `contents` the note's table
/// of contents.
pub fn apply(
    block: &Block,
    line: &str,
    slash: usize,
    col: usize,
    today: &str,
    contents: &str,
) -> Edit {
    let chars: Vec<char> = line.chars().collect();
    let col = col.clamp(slash, chars.len());
    let before: String = chars[..slash].iter().collect();
    let after: String = chars[col..].iter().collect();
    // The line without the `/` and what was typed after it.
    let kept = format!("{before}{after}");
    let (lead_len, indent_len) = lead(&kept);
    let indent: String = kept.chars().take(indent_len).collect();
    let text: String = kept.chars().skip(lead_len).collect();
    let empty = text.trim().is_empty();
    let whole = |text: String| {
        let cursor = text
            .find(CURSOR)
            .map_or(text.chars().count(), |b| text[..b].chars().count());
        Edit {
            from: 0,
            to: chars.len(),
            text: text.replace(CURSOR, ""),
            cursor,
        }
    };
    let lines = |template: &str| {
        template
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("{indent}{l}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    match block.insert {
        Insert::Line(marker) if empty => whole(format!("{indent}{marker}{CURSOR}{}", text.trim())),
        Insert::Line(marker) => whole(format!("{}\n{indent}{marker}{CURSOR}", kept.trim_end())),
        Insert::Lines(template) if empty => whole(lines(template)),
        Insert::Lines(template) => whole(format!("{}\n{}", kept.trim_end(), lines(template))),
        // The cursor on a line of its own after the list.
        Insert::Contents if empty => whole(format!("{}\n{CURSOR}", lines(contents))),
        Insert::Contents => whole(format!(
            "{}\n{}\n{CURSOR}",
            kept.trim_end(),
            lines(contents)
        )),
        Insert::Inline(template) => {
            let cursor = template
                .find(CURSOR)
                .map_or(0, |b| template[..b].chars().count());
            Edit {
                from: slash,
                to: col,
                text: template.replace(CURSOR, ""),
                cursor,
            }
        }
        Insert::Date => Edit {
            from: slash,
            to: col,
            text: today.to_string(),
            cursor: today.chars().count(),
        },
    }
}

/// Today's date, as `2026-11-28`: the local one where the system says it.
pub fn today() -> String {
    #[cfg(unix)]
    if let Ok(out) = std::process::Command::new("date").arg("+%Y-%m-%d").output() {
        let date = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && date.len() == 10 {
            return date;
        }
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    civil(seconds.div_euclid(86_400))
}

/// Days since 1970 as a date (Howard Hinnant's algorithm).
fn civil(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The note's headings, outside its code blocks: (level, text).
pub fn headings(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        if let Some(open) = fence_mark(line) {
            match fence {
                None => fence = Some(open),
                Some((c, n)) if open.0 == c && open.1 >= n && closes(line) => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let t = line.trim_start();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            let title = t[hashes..].trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                out.push((hashes, title.to_string()));
            }
        }
    }
    out
}

/// The anchor GitHub gives a heading: `## Set up & run` → `set-up--run`.
pub fn anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// A table of contents for `text`: a list of links to its headings, nested by level.
pub fn contents(text: &str) -> String {
    let headings = headings(text);
    let top = headings.iter().map(|h| h.0).min().unwrap_or(1);
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    let mut out = String::new();
    for (level, title) in headings {
        let base = anchor(&title);
        let n = seen.entry(base.clone()).or_default();
        let link = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        let indent = "  ".repeat(level - top);
        out.push_str(&format!("{indent}- [{title}](#{link})\n"));
    }
    out
}

/// A fence line's mark: its character and how many (``` or ~~~, at least 3).
fn fence_mark(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|d| *d == c).count();
    (n >= 3).then_some((c, n))
}

/// Whether a fence line can close a block: nothing after its mark.
fn closes(line: &str) -> bool {
    let t = line.trim();
    t.trim_start_matches(['`', '~']).trim().is_empty()
}

/// The code block line `row` of `lines` is in (its fences too): the row it opens on
/// and its language. `None` outside one.
pub fn fence_at<'a>(lines: impl Iterator<Item = &'a str>, row: usize) -> Option<(usize, String)> {
    let mut open: Option<(usize, String, char, usize)> = None;
    for (i, line) in lines.enumerate().take(row + 1) {
        let Some((c, n)) = fence_mark(line) else {
            continue;
        };
        match &open {
            None => {
                let label = line.trim_start()[n..]
                    .split([',', ' ', '{'])
                    .next()
                    .unwrap_or_default()
                    .to_string();
                open = Some((i, label, c, n));
            }
            Some((_, _, oc, on)) if c == *oc && n >= *on && closes(line) => {
                if i == row {
                    break;
                }
                open = None;
            }
            _ => {}
        }
    }
    open.map(|(row, label, ..)| (row, label))
}

/// Whether the fence line `row` of `lines` opens a code block that isn't closed.
pub fn unclosed(lines: &[&str], row: usize) -> bool {
    let Some((c, n)) = lines.get(row).and_then(|l| fence_mark(l)) else {
        return false;
    };
    if fence_at(lines.iter().copied(), row).map(|(open, _)| open) != Some(row) {
        return false;
    }
    !lines[row + 1..]
        .iter()
        .any(|l| fence_mark(l).is_some_and(|(d, m)| d == c && m >= n) && closes(l))
}

/// Code block languages, as their fences name them, with what they are: the ones
/// this note uses first.
pub fn languages(used: &[String]) -> Vec<(String, &'static str)> {
    const LANGUAGES: &[(&str, &str)] = &[
        ("python", "Python"),
        ("rust", "Rust"),
        ("javascript", "JavaScript"),
        ("typescript", "TypeScript"),
        ("tsx", "TypeScript JSX"),
        ("go", "Go"),
        ("bash", "Bash"),
        ("sh", "Shell"),
        ("c", "C"),
        ("cpp", "C++"),
        ("java", "Java"),
        ("kotlin", "Kotlin"),
        ("swift", "Swift"),
        ("csharp", "C#"),
        ("ruby", "Ruby"),
        ("php", "PHP"),
        ("lua", "Lua"),
        ("sql", "SQL"),
        ("json", "JSON"),
        ("yaml", "YAML"),
        ("toml", "TOML"),
        ("html", "HTML"),
        ("css", "CSS"),
        ("xml", "XML"),
        ("markdown", "Markdown"),
        ("diff", "Diff"),
        ("dockerfile", "Dockerfile"),
        ("makefile", "Makefile"),
        ("powershell", "PowerShell"),
        ("haskell", "Haskell"),
        ("elixir", "Elixir"),
        ("scala", "Scala"),
        ("dart", "Dart"),
        ("zig", "Zig"),
        ("r", "R"),
        ("asm", "Assembly"),
        ("nasm", "Assembly (NASM)"),
        ("ini", "INI"),
        ("text", "Plain text"),
    ];
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for language in used {
        if language.is_empty() || out.iter().any(|(l, _)| l == language) {
            continue;
        }
        let what = LANGUAGES
            .iter()
            .find(|(l, _)| l == language)
            .map_or("in this note", |(_, w)| w);
        out.push((language.clone(), what));
    }
    for (language, what) in LANGUAGES {
        if !out.iter().any(|(l, _)| l == language) {
            out.push((language.to_string(), what));
        }
    }
    out
}

/// The languages of the code blocks in `text`, in order.
pub fn used_languages(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut row = 0;
    while row < lines.len() {
        if fence_mark(lines[row]).is_some() {
            if let Some((open, label)) = fence_at(lines.iter().copied(), row) {
                if open == row && !label.is_empty() && !out.contains(&label) {
                    out.push(label);
                }
            }
        }
        row += 1;
    }
    out
}

/// The language a code block's fence names, as Raido knows languages (its keywords
/// to suggest): `python`, `py` → Python.
pub fn language_of(label: &str) -> crate::language::Lang {
    let file = match label.to_ascii_lowercase().as_str() {
        "python" | "python3" | "py" => "x.py",
        "rust" | "rs" => "x.rs",
        "go" | "golang" => "x.go",
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "node" => "x.ts",
        "c" | "h" | "cpp" | "c++" | "cc" | "cxx" | "hpp" | "objc" => "x.c",
        "java" | "kotlin" | "kt" | "swift" | "csharp" | "cs" | "c#" | "php" | "css" | "scss"
        | "json" | "jsonc" | "dart" | "scala" | "zig" => "x.json",
        "bash" | "sh" | "shell" | "zsh" | "fish" | "console" | "shellscript" => "x.sh",
        "yaml" | "yml" => "x.yaml",
        "toml" => "x.toml",
        "lua" => "x.lua",
        "ruby" | "rb" => "x.rb",
        "dockerfile" | "docker" => "Dockerfile",
        "makefile" | "make" => "Makefile",
        _ => return crate::language::Lang::Plain,
    };
    crate::language::Lang::detect(Some(Path::new(file)))
}

/// Files a note links to, from its folder (`note_dir`, in the project at `root`):
/// the project's `files` (relative to `root`) as the note writes them, Markdown
/// first, then images, then the rest.
pub fn link_targets(root: &Path, note_dir: &Path, files: &[String]) -> Vec<(String, &'static str)> {
    let note = note_dir.strip_prefix(root).unwrap_or(Path::new(""));
    let rank = |f: &str| {
        let ext = Path::new(f)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => (0, "note"),
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => (1, "image"),
            _ => (2, "file"),
        }
    };
    let mut out: Vec<(usize, String, &'static str)> = files
        .iter()
        .map(|f| {
            let (rank, what) = rank(f);
            (rank, relative(note, Path::new(f)), what)
        })
        .collect();
    out.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(&b.1))
    });
    out.into_iter()
        .map(|(_, path, what)| (path, what))
        .collect()
}

/// `to` (from the project's root) as a path from folder `from`, with `/`s.
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// The lines of the table line `row` of `lines` is in: rows that start with `|`,
/// the second of them its `| --- |` line.
pub fn table_at(lines: &[&str], row: usize) -> Option<(usize, usize)> {
    let is_row = |r: usize| {
        lines
            .get(r)
            .is_some_and(|l| l.trim_start().starts_with('|'))
    };
    if !is_row(row) {
        return None;
    }
    let mut first = row;
    while first > 0 && is_row(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while is_row(last + 1) {
        last += 1;
    }
    let delimiter = lines.get(first + 1).is_some_and(|l| is_delimiter(l));
    (delimiter && last > first).then_some((first, last))
}

fn is_delimiter(line: &str) -> bool {
    let cells = cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            !c.trim_matches(':').is_empty() && c.trim_matches(':').chars().all(|d| d == '-')
        })
}

/// A table row's cells, without the bars at its ends; `\|` stays in its cell.
fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t
        .strip_suffix('|')
        .filter(|r| !r.ends_with('\\'))
        .unwrap_or(t);
    let mut out = vec![String::new()];
    let mut escaped = false;
    for c in t.chars() {
        if c == '|' && !escaped {
            out.push(String::new());
        } else if let Some(cell) = out.last_mut() {
            cell.push(c);
        }
        escaped = c == '\\' && !escaped;
    }
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

/// A table's lines lined up: every column as wide as its widest cell, the
/// `| --- |` line keeping each column's alignment.
pub fn format_table(lines: &[&str]) -> Vec<String> {
    let indent: String = lines
        .first()
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = lines.iter().map(|l| cells(l)).collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let aligns: Vec<(bool, bool)> = (0..columns)
        .map(|j| {
            let c = rows
                .get(1)
                .and_then(|r| r.get(j))
                .map_or("", |c| c.as_str());
            (c.starts_with(':'), c.ends_with(':') && c.len() > 1)
        })
        .collect();
    let widths: Vec<usize> = (0..columns)
        .map(|j| {
            rows.iter()
                .enumerate()
                .filter(|(i, _)| *i != 1)
                .filter_map(|(_, r)| r.get(j))
                .map(|c| c.width())
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let parts: Vec<String> = (0..columns)
                .map(|j| {
                    let w = widths[j];
                    if i == 1 {
                        let (left, right) = aligns[j];
                        let dashes = w - usize::from(left) - usize::from(right);
                        return format!(
                            "{}{}{}",
                            if left { ":" } else { "" },
                            "-".repeat(dashes),
                            if right { ":" } else { "" }
                        );
                    }
                    let cell = row.get(j).map_or("", |c| c.as_str());
                    let pad = w.saturating_sub(cell.width());
                    match aligns[j] {
                        (_, true) if !aligns[j].0 => format!("{}{cell}", " ".repeat(pad)),
                        (true, true) => {
                            format!("{}{cell}{}", " ".repeat(pad / 2), " ".repeat(pad - pad / 2))
                        }
                        _ => format!("{cell}{}", " ".repeat(pad)),
                    }
                })
                .collect();
            format!("{indent}| {} |", parts.join(" | "))
        })
        .collect()
}

/// How many cells a table row has.
pub fn columns(line: &str) -> usize {
    cells(line).len()
}

/// Which cell of a table row column `col` is in (from 0).
pub fn cell_of(line: &str, col: usize) -> usize {
    let before: String = line.chars().take(col).collect();
    let t = before.trim_start();
    let bars = t.matches('|').count() - t.matches("\\|").count();
    bars.saturating_sub(usize::from(t.starts_with('|')))
}

/// Where the text of cell `cell` of a lined-up table row starts.
pub fn cell_start(line: &str, cell: usize) -> usize {
    let mut seen = 0;
    let mut escaped = false;
    for (i, c) in line.chars().enumerate() {
        if c == '|' && !escaped {
            if seen == cell {
                return i + 2;
            }
            seen += 1;
        }
        escaped = c == '\\' && !escaped;
    }
    line.chars().count()
}

/// A new, empty row for a table whose rows look like `line`.
pub fn empty_row(line: &str) -> String {
    let indent: String = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let n = cells(line).len().max(1);
    format!("{indent}|{}", "  |".repeat(n))
}

/// A list item's line: how far Tab nests it (the width of its marker, as the
/// item under it lines up with its text).
pub fn nests_by(line: &str) -> Option<usize> {
    let t = line.trim_start();
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return Some(2);
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    (digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")))
        .then_some(digits + 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(label: &str, line: &str, slash: usize, col: usize) -> Edit {
        apply(
            block(label).unwrap(),
            line,
            slash,
            col,
            "2026-11-28",
            "- [A](#a)\n",
        )
    }

    #[test]
    fn a_block_turns_an_empty_line_into_it_or_goes_after_the_text() {
        // An empty line (or one with only its marker) becomes the block.
        assert_eq!(
            pick("Heading 2", "/h2", 0, 3),
            Edit {
                from: 0,
                to: 3,
                text: "## ".into(),
                cursor: 3
            }
        );
        assert_eq!(pick("To-do list", "  - /todo", 4, 9).text, "  - [ ] ");
        assert_eq!(pick("Bulleted list", "## /b", 3, 5).text, "- ");
        // A line with text keeps it; the block starts the line after.
        let edit = pick("Heading 1", "Ideas /h1", 6, 9);
        assert_eq!((edit.text.as_str(), edit.cursor), ("Ideas\n# ", 8));
        // Lines of their own, indented as the line was, the cursor where it goes.
        let edit = pick("Code block", "  /code", 2, 7);
        assert_eq!((edit.text.as_str(), edit.cursor), ("  ```\n\n  ```", 5));
        let edit = pick("Table", "/table", 0, 6);
        assert_eq!(edit.text, "|  |  |\n| --- | --- |\n|  |  |");
        assert_eq!(edit.cursor, 2);
        assert_eq!(pick("Callout · Tip", "/tip", 0, 4).text, "> [!TIP]\n> ");
        assert_eq!(pick("Divider", "/div", 0, 4).cursor, 4);
        // Inline ones go where the `/` was.
        let edit = pick("Date", "Met on /date for lunch", 7, 12);
        assert_eq!(
            (edit.from, edit.to, edit.text.as_str(), edit.cursor),
            (7, 12, "2026-11-28", 10)
        );
        let edit = pick("Link", "see /link", 4, 9);
        assert_eq!((edit.text.as_str(), edit.cursor), ("[]()", 1));
        assert_eq!(pick("Table of contents", "/toc", 0, 4).text, "- [A](#a)\n");
        // A `/` opens the menu after a space or at the start, not in a path or code.
        assert!(opens_menu("/", 0) && opens_menu("- /", 2) && opens_menu("a /", 2));
        assert!(!opens_menu("src/", 3) && !opens_menu("`a /", 3));
    }

    #[test]
    fn fences_languages_headings_and_links_are_found() {
        let note =
            "# Top\n\n```python\nx = 1\n```\n\n## Set up & run\n\n```\n# not a heading\n```\n";
        let lines: Vec<&str> = note.lines().collect();
        assert_eq!(
            fence_at(lines.iter().copied(), 3),
            Some((2, "python".into()))
        );
        assert_eq!(
            fence_at(lines.iter().copied(), 4),
            Some((2, "python".into()))
        );
        assert_eq!(fence_at(lines.iter().copied(), 6), None);
        assert_eq!(fence_at(lines.iter().copied(), 9), Some((8, String::new())));
        assert!(!unclosed(&lines, 2));
        assert!(unclosed(&["```rust", "fn x() {}"], 0));
        assert_eq!(used_languages(note), ["python"]);
        assert_eq!(languages(&["zig".into()])[0], ("zig".to_string(), "Zig"));
        assert_eq!(language_of("py"), crate::language::Lang::Python);
        assert_eq!(
            headings(note),
            [(1, "Top".into()), (2, "Set up & run".into())]
        );
        assert_eq!(anchor("Set up & run"), "set-up--run");
        assert_eq!(
            contents(note),
            "- [Top](#top)\n  - [Set up & run](#set-up--run)\n"
        );
        // Files from the note's folder: notes first, then images, then the rest.
        let files = [
            "src/main.rs".to_string(),
            "docs/a.md".into(),
            "docs/img/x.png".into(),
            "README.md".into(),
        ];
        let targets: Vec<String> = link_targets(Path::new("/p"), Path::new("/p/docs"), &files)
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(
            targets,
            ["a.md", "../README.md", "img/x.png", "../src/main.rs"]
        );
        assert_eq!(civil(20_785), "2026-11-28");
    }

    #[test]
    fn tables_line_up_and_tab_finds_the_cells() {
        let table = ["| a | long name |", "|:-|--:|", "| **bold** | 1 |"];
        assert_eq!(table_at(&table, 2), Some((0, 2)));
        assert_eq!(table_at(&["| a |", "text"], 0), None);
        assert_eq!(
            format_table(&table),
            [
                "| a        | long name |",
                "| :------- | --------: |",
                "| **bold** |         1 |",
            ]
        );
        let row = "| a        | long name |";
        assert_eq!((cell_of(row, 2), cell_of(row, 13)), (0, 1));
        assert_eq!((cell_start(row, 0), cell_start(row, 1)), (2, 13));
        assert_eq!(empty_row(row), "|  |  |");
        assert_eq!(
            (nests_by("- a"), nests_by("12. b"), nests_by("text")),
            (Some(2), Some(4), None)
        );
    }
}
