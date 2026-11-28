//! Writing notes in Markdown, as in Notion: a `/` menu of blocks (headings, lists,
//! to-dos, quotes, callouts, code blocks, tables, dividers, links, the date). What
//! goes in the note is plain Markdown; these are the rules for writing it, and
//! `App` puts them in the editor's completion menu.

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
    fn fences_and_headings_are_found() {
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
        assert_eq!(
            headings(note),
            [(1, "Top".into()), (2, "Set up & run".into())]
        );
        assert_eq!(anchor("Set up & run"), "set-up--run");
        assert_eq!(
            contents(note),
            "- [Top](#top)\n  - [Set up & run](#set-up--run)\n"
        );
        assert_eq!(civil(20_785), "2026-11-28");
    }
}
