//! Markdown drawn rendered while you edit it, like Obsidian's Live Preview: a
//! note's headings, emphasis, links, lists and to-dos, quotes and callouts, code
//! blocks, tables and rules are drawn as they read, their marks hidden, and only
//! the line the cursor is on (or a selection's lines) shows its Markdown, to edit
//! as text. Each line of the file stays one row on screen, so the cursor,
//! scrolling, line numbers and the Git marks work as they always do.
//!
//! The note is read once per change (`Cache::get`), into how each line looks
//! (`Look`); drawing a line (`draw`) turns that into styled text, and says where
//! each character of the line went, for the mouse, the selection and search marks.
use crate::markdown::Palette;
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag,
    TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

/// Notes larger than this are drawn as they are written.
const MOST: usize = 2 * 1024 * 1024;
/// A non-breaking space: inline code's padding, as in the preview.
const NBSP: &str = "\u{a0}";

/// A color of the theme, picked when the line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Fg,
    Muted,
    Accent,
    Secondary,
    Border,
    Str,
    Function,
    Constant,
    Warning,
    Danger,
    Background,
    /// Behind inline code.
    InlineBg,
}

/// How characters are drawn: colors and attributes, added up where marks overlap
/// (the later mark's colors win).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Paint {
    pub fg: Option<Tone>,
    pub bg: Option<Tone>,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
}

impl Paint {
    fn fg(tone: Tone) -> Self {
        Self {
            fg: Some(tone),
            ..Self::default()
        }
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    fn on(mut self, tone: Tone) -> Self {
        self.bg = Some(tone);
        self
    }
    fn over(self, top: Paint) -> Paint {
        Paint {
            fg: top.fg.or(self.fg),
            bg: top.bg.or(self.bg),
            bold: self.bold || top.bold,
            italic: self.italic || top.italic,
            strike: self.strike || top.strike,
            underline: self.underline || top.underline,
        }
    }
    fn style(self, base: Style, p: &Palette) -> Style {
        let mut style = base;
        if let Some(fg) = self.fg {
            style = style.fg(color(fg, p));
        }
        if let Some(bg) = self.bg {
            style = style.bg(color(bg, p));
        }
        for (on, modifier) in [
            (self.bold, Modifier::BOLD),
            (self.italic, Modifier::ITALIC),
            (self.strike, Modifier::CROSSED_OUT),
            (self.underline, Modifier::UNDERLINED),
        ] {
            if on {
                style = style.add_modifier(modifier);
            }
        }
        style
    }
}

fn color(tone: Tone, p: &Palette) -> Color {
    match tone {
        Tone::Fg => p.fg,
        Tone::Muted => p.muted,
        Tone::Accent => p.accent,
        Tone::Secondary => p.secondary,
        Tone::Border => p.border,
        Tone::Str => p.string,
        Tone::Function => p.function,
        Tone::Constant => p.constant,
        Tone::Warning => p.warning,
        Tone::Danger => p.danger,
        Tone::Background => p.background,
        Tone::InlineBg => p.inline_bg,
    }
}

/// What a mark does to the characters it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// Not drawn: Markdown's own marks (`**`, `#`, a link's address).
    Hide,
    /// Drawn instead of the characters (a bullet, a box, a bar, a label); with no
    /// characters covered, drawn before the one it's at.
    Glyph(String, Paint),
    Paint(Paint),
    /// After the line's text, `char` to the end of the row (a heading's rule).
    Fill(char, Paint),
}

/// A mark on characters `start..end` of a line (in characters, not bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub start: usize,
    pub end: usize,
    pub what: What,
}

/// What a line is part of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Block {
    #[default]
    Text,
    Heading(u8),
    /// The `===` or `---` under a heading's text.
    Underline(u8),
    /// A code block's fence: the opening one with its language.
    Fence(Option<String>),
    Code,
    Rule,
    /// A table's row: its cells (character ranges), of table `table`.
    Row {
        table: usize,
        cells: Vec<(usize, usize)>,
        head: bool,
    },
    /// The `| --- | :-: |` line under a table's head.
    Delimiter(usize),
    FrontMatter,
}

/// How a line is drawn rendered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub block: Block,
    pub marks: Vec<Mark>,
    /// A to-do's box: the column of its `[`, and whether it's checked.
    pub task: Option<(usize, bool)>,
}

/// A table's columns: how wide each is drawn, and how its cells sit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub widths: Vec<usize>,
    pub aligns: Vec<Alignment>,
}

/// A note read for drawing.
#[derive(Debug, Default)]
pub struct Doc {
    revision: u64,
    pub lines: Vec<Look>,
    pub tables: Vec<Table>,
    /// The syntax colors of its code blocks' lines, in each block's language.
    pub code: HashMap<usize, Vec<(syntect::highlighting::Style, String)>>,
}

/// The notes read, by buffer, and those switched from how they're drawn by default.
#[derive(Default)]
pub struct Cache {
    docs: HashMap<usize, Doc>,
    /// Buffers drawn the other way round from `markdown_live` (`Ctrl-b R`).
    switched: HashSet<usize>,
}

impl Cache {
    /// Buffer `id` read as it is now, when it's drawn rendered: a Markdown file,
    /// rendering on (`live`, the setting, unless switched), not too large.
    pub fn get(
        &mut self,
        id: usize,
        buffer: &crate::editor::Buffer,
        live: bool,
        syntax: &crate::highlight::Syntax,
    ) -> Option<&Doc> {
        if !self.rendered(id, buffer, live) {
            return None;
        }
        let fresh = self
            .docs
            .get(&id)
            .is_some_and(|d| d.revision == buffer.revision);
        if !fresh {
            let text = buffer.text.to_string();
            let (lines, tables) = read(&text);
            let code = colored(&text, &lines, syntax);
            self.docs.insert(
                id,
                Doc {
                    revision: buffer.revision,
                    lines,
                    tables,
                    code,
                },
            );
        }
        self.docs.get(&id)
    }
    /// The note as last read, if it's as the buffer is now: for the mouse, between
    /// frames.
    pub fn peek(&self, id: usize, buffer: &crate::editor::Buffer, live: bool) -> Option<&Doc> {
        if !self.rendered(id, buffer, live) {
            return None;
        }
        self.docs.get(&id).filter(|d| d.revision == buffer.revision)
    }
    /// Whether buffer `id` is drawn rendered.
    pub fn rendered(&self, id: usize, buffer: &crate::editor::Buffer, live: bool) -> bool {
        live != self.switched.contains(&id)
            && crate::language::Lang::of(buffer) == crate::language::Lang::Markdown
            && buffer.text.len_bytes() <= MOST
    }
    /// Draw buffer `id` the other way: rendered or as written. True when it's
    /// rendered now.
    pub fn switch(&mut self, id: usize, buffer: &crate::editor::Buffer, live: bool) -> bool {
        if !self.switched.remove(&id) {
            self.switched.insert(id);
        }
        self.rendered(id, buffer, live)
    }
}

/// The syntax colors of the code blocks' lines in `text`, by line: each block in
/// its language's grammar, as the preview colors it.
fn colored(
    text: &str,
    looks: &[Look],
    syntax: &crate::highlight::Syntax,
) -> HashMap<usize, Vec<(syntect::highlighting::Style, String)>> {
    use syntect::easy::HighlightLines;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = HashMap::new();
    let mut row = 0;
    while row < looks.len() {
        let Block::Fence(Some(label)) = &looks[row].block else {
            row += 1;
            continue;
        };
        let first = row + 1;
        let mut last = first;
        while last < looks.len() && looks[last].block == Block::Code {
            last += 1;
        }
        let code = lines[first.min(lines.len())..last.min(lines.len())].join("\n");
        if let Some((grammar, set)) = Some(label.as_str())
            .filter(|l| !l.is_empty())
            .and_then(|l| syntax.by_token_in(l, &code))
        {
            let mut highlighter = HighlightLines::new(grammar, &syntax.theme);
            for (i, line) in lines[first.min(lines.len())..last.min(lines.len())]
                .iter()
                .enumerate()
            {
                let line = format!("{line}\n");
                if let Ok(regions) = highlighter.highlight_line(&line, set) {
                    let regions = regions
                        .into_iter()
                        .map(|(style, part)| (style, part.trim_end_matches('\n').to_string()))
                        .filter(|(_, part)| !part.is_empty())
                        .collect();
                    out.insert(first + i, regions);
                }
            }
        }
        row = last;
    }
    out
}

/// The note's lines, by byte.
struct Lines<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { text, starts }
    }
    fn len(&self) -> usize {
        self.starts.len()
    }
    fn line_of(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    }
    /// Line `line`, without its line break.
    fn text(&self, line: usize) -> &'a str {
        let start = self.starts[line];
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        &self.text[start..end.max(start)]
    }
    /// The line and character column of byte `byte`.
    fn pos(&self, byte: usize) -> (usize, usize) {
        let line = self.line_of(byte);
        let start = self.starts[line];
        let byte = byte.min(start + self.text(line).len());
        (line, self.text[start..byte].chars().count())
    }
    /// The last line `range` covers: its end is often the next line's start.
    fn last_line(&self, range: &Range<usize>) -> usize {
        self.line_of(range.end.saturating_sub(1).max(range.start))
    }
}

/// How the lines of `text` (a Markdown note) are drawn rendered, and its tables.
pub fn read(text: &str) -> (Vec<Look>, Vec<Table>) {
    let lines = Lines::new(text);
    let mut looks = vec![Look::default(); lines.len()];
    let mut tables = Vec::new();
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut marks = Marks {
        lines: &lines,
        looks: &mut looks,
    };
    // Open lists (their depth gives the bullet), the item's bullet to hide when it
    // turns out to be a to-do, open links and images (where their text ends), the
    // quotes open, and the table being read.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut bullet: Option<(usize, usize)> = None;
    let mut links: Vec<(usize, bool, LinkType, usize)> = Vec::new();
    let mut quotes = 0;
    let mut code = 0;
    let mut table: Option<Reading> = None;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        // Where an open link's or image's text has got to.
        if let Some(link) = links.last_mut() {
            if !matches!(event, Event::End(TagEnd::Link | TagEnd::Image)) && range.end > link.3 {
                link.3 = range.end;
            }
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => marks.heading(level, &range),
                Tag::Emphasis => marks.around(&range, '*', 1, Paint::default().italic()),
                Tag::Strong => marks.around(&range, '*', 2, Paint::default().bold()),
                Tag::Strikethrough => {
                    let n = text[range.clone()]
                        .chars()
                        .take_while(|c| *c == '~')
                        .count();
                    let paint = Paint {
                        strike: true,
                        ..Paint::default()
                    };
                    marks.around(&range, '~', n.clamp(1, 2), paint);
                }
                Tag::Link { link_type, .. } => {
                    links.push((range.start, false, link_type, range.start + 1))
                }
                Tag::Image { link_type, .. } => {
                    links.push((range.start, true, link_type, range.start + 2))
                }
                Tag::List(start) => lists.push(start),
                Tag::Item => bullet = marks.item(&range, &lists),
                Tag::BlockQuote(kind) => {
                    if quotes == 0 {
                        marks.quote(&range, kind);
                    }
                    quotes += 1;
                }
                Tag::CodeBlock(kind) => {
                    code += 1;
                    marks.code_block(&range, kind)
                }
                Tag::MetadataBlock(_) => {
                    for line in lines.line_of(range.start)..=lines.last_line(&range) {
                        marks.looks[line].block = Block::FrontMatter;
                    }
                }
                Tag::HtmlBlock => marks.paint(range.start, range.end, Paint::fg(Tone::Muted)),
                Tag::FootnoteDefinition(_) => {
                    let label = text[range.clone()].find("]:").map_or(0, |i| i + 2);
                    marks.paint(range.start, range.start + label, Paint::fg(Tone::Accent));
                }
                Tag::Table(aligns) => {
                    table = Some(Reading {
                        aligns,
                        rows: Vec::new(),
                    })
                }
                Tag::TableHead => {
                    if let Some(t) = &mut table {
                        t.rows.push((lines.line_of(range.start), Vec::new(), true));
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = &mut table {
                        t.rows.push((lines.line_of(range.start), Vec::new(), false));
                    }
                }
                Tag::TableCell => {
                    if let Some((_, cells, _)) = table.as_mut().and_then(|t| t.rows.last_mut()) {
                        cells.push(range.clone());
                    }
                }
                _ => {}
            },
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some((start, image, kind, text_end)) = links.pop() {
                    marks.link(start..range.end, image, kind, text_end);
                }
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::End(TagEnd::BlockQuote(_)) => quotes -= 1,
            Event::End(TagEnd::CodeBlock) => code -= 1,
            Event::End(TagEnd::Table) => {
                if let Some(t) = table.take() {
                    marks.table(t, &mut tables);
                }
            }
            Event::TaskListMarker(checked) => marks.task(&range, checked, bullet.take()),
            Event::Code(_) => marks.code(&range),
            Event::Rule => {
                let line = lines.line_of(range.start);
                marks.looks[line].block = Block::Rule;
            }
            Event::FootnoteReference(_) => {
                marks.paint(range.start, range.end, Paint::fg(Tone::Accent))
            }
            Event::InlineHtml(_) | Event::Html(_) => {
                marks.paint(range.start, range.end, Paint::fg(Tone::Muted))
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                marks.paint(range.start, range.end, Paint::fg(Tone::Constant))
            }
            // An escaped character (`\*`): its text starts after the backslash, which
            // goes. A code block's text is as written.
            Event::Text(_)
                if code == 0
                    && range.start > 0
                    && text.as_bytes()[range.start - 1] == b'\\'
                    && text[range.start..].starts_with(|c: char| c.is_ascii_punctuation()) =>
            {
                marks.hide(range.start - 1, range.start)
            }
            _ => {}
        }
    }
    (looks, tables)
}

/// A table being read: its alignments, and each row's line, cells and whether it's
/// the head.
struct Reading {
    aligns: Vec<Alignment>,
    rows: Vec<(usize, Vec<Range<usize>>, bool)>,
}

/// The marks being put on the note's lines.
struct Marks<'a, 'b> {
    lines: &'a Lines<'b>,
    looks: &'a mut Vec<Look>,
}

impl Marks<'_, '_> {
    /// A mark on bytes `from..to`, split by line.
    fn mark(&mut self, from: usize, to: usize, what: What) {
        if to < from {
            return;
        }
        let (l0, c0) = self.lines.pos(from);
        let (l1, c1) = self.lines.pos(to);
        if l0 == l1 {
            self.looks[l0].marks.push(Mark {
                start: c0,
                end: c1,
                what,
            });
            return;
        }
        let width = |line: usize| self.lines.text(line).chars().count();
        let first = width(l0);
        self.looks[l0].marks.push(Mark {
            start: c0,
            end: first,
            what: what.clone(),
        });
        for line in l0 + 1..l1 {
            let end = width(line);
            self.looks[line].marks.push(Mark {
                start: 0,
                end,
                what: what.clone(),
            });
        }
        self.looks[l1].marks.push(Mark {
            start: 0,
            end: c1,
            what,
        });
    }
    fn hide(&mut self, from: usize, to: usize) {
        self.mark(from, to, What::Hide);
    }
    fn paint(&mut self, from: usize, to: usize, paint: Paint) {
        self.mark(from, to, What::Paint(paint));
    }
    fn glyph(&mut self, from: usize, to: usize, glyph: &str, paint: Paint) {
        self.mark(from, to, What::Glyph(glyph.to_string(), paint));
    }
    /// Emphasis, strong, struck: `n` of the marks at each end hidden, between them
    /// painted.
    fn around(&mut self, range: &Range<usize>, mark: char, n: usize, paint: Paint) {
        let text = &self.lines.text[range.clone()];
        let first = text.chars().next().unwrap_or(mark);
        let n = text.chars().take_while(|c| *c == first).count().min(n);
        if range.len() < 2 * n {
            return;
        }
        self.hide(range.start, range.start + n);
        self.hide(range.end - n, range.end);
        self.paint(range.start + n, range.end - n, paint);
    }
    fn heading(&mut self, level: HeadingLevel, range: &Range<usize>) {
        let n = level as u8;
        let paint = match level {
            HeadingLevel::H1 => Paint::fg(Tone::Background).on(Tone::Accent),
            HeadingLevel::H2 => Paint::fg(Tone::Accent),
            HeadingLevel::H3 => Paint::fg(Tone::Secondary),
            HeadingLevel::H4 => Paint::fg(Tone::Function),
            HeadingLevel::H5 => Paint::fg(Tone::Fg),
            HeadingLevel::H6 => Paint::fg(Tone::Muted),
        }
        .bold();
        let first = self.lines.line_of(range.start);
        let last = self.lines.last_line(range);
        let line = self.lines.text(first);
        let start = self.lines.starts[first];
        let indent = line.len() - line.trim_start().len();
        if line.trim_start().starts_with('#') {
            // `## Title ##`: the hashes and the space after them go, and the closing
            // ones; H1 is a pill like the preview's, H2 has a rule after it.
            let hashes = line[indent..].chars().take_while(|c| *c == '#').count();
            let after = line[indent + hashes..].len() - line[indent + hashes..].trim_start().len();
            let open = indent + hashes + after;
            let body = line[open..].trim_end();
            let closing = body.len() - body.trim_end_matches('#').len();
            let close = if closing > 0 && body[..body.len() - closing].ends_with([' ', '\t']) {
                body[..body.len() - closing].trim_end().len()
            } else {
                body.len()
            };
            let text_end = start + open + close;
            if n == 1 {
                self.glyph(start + indent, start + open, " ", paint);
            } else {
                self.hide(start + indent, start + open);
            }
            self.paint(start + open, text_end, paint);
            self.hide(text_end, start + line.len());
            let at = self.lines.pos(start + line.len()).1;
            let tail = match n {
                1 => What::Glyph(" ".into(), paint),
                2 => What::Fill('─', Paint::fg(Tone::Border)),
                _ => What::Paint(Paint::default()),
            };
            self.looks[first].marks.push(Mark {
                start: at,
                end: at,
                what: tail,
            });
            self.looks[first].block = Block::Heading(n);
            return;
        }
        // A setext heading: its text lines, then the underline.
        for row in first..last {
            let text = self.lines.text(row);
            let begin = self.lines.starts[row];
            self.paint(begin, begin + text.len(), paint);
            self.looks[row].block = Block::Heading(n);
        }
        self.looks[last].block = Block::Underline(n);
    }
    /// A list item's bullet, by depth (`•`, `◦`, `▪`), or its number in the accent
    /// color: the line and mark of a bullet, to hide if it's a to-do.
    fn item(&mut self, range: &Range<usize>, lists: &[Option<u64>]) -> Option<(usize, usize)> {
        let text = &self.lines.text[range.start..];
        let at = range.start + (text.len() - text.trim_start().len());
        let line = self.lines.line_of(at);
        match lists.last() {
            Some(Some(_)) => {
                let digits = self.lines.text[at..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .count();
                self.paint(at, at + digits + 1, Paint::fg(Tone::Accent));
                None
            }
            _ => {
                let depth = lists.len().saturating_sub(1);
                let glyph = ["•", "◦", "▪"][depth % 3];
                self.glyph(at, at + 1, glyph, Paint::fg(Tone::Accent));
                Some((line, self.looks[line].marks.len() - 1))
            }
        }
    }
    /// A to-do's box (`☐`, `☑`) in place of `[ ]`: the bullet before it goes, and
    /// a done one's text is struck out, faint.
    fn task(&mut self, range: &Range<usize>, checked: bool, bullet: Option<(usize, usize)>) {
        let (line, col) = self.lines.pos(range.start);
        if let Some((row, index)) = bullet.filter(|(row, _)| *row == line) {
            let mark = &mut self.looks[row].marks[index];
            mark.what = What::Hide;
            mark.end = col;
        }
        let (glyph, tone) = if checked {
            ("☑", Tone::Str)
        } else {
            ("☐", Tone::Muted)
        };
        self.glyph(range.start, range.end, glyph, Paint::fg(tone));
        if checked {
            let end = self.lines.starts[line] + self.lines.text(line).len();
            let done = Paint {
                fg: Some(Tone::Muted),
                strike: true,
                ..Paint::default()
            };
            self.paint(range.end, end, done);
        }
        self.looks[line].task = Some((col, checked));
    }
    /// A quote's `>`s as bars, faint text; a callout's `[!NOTE]` as its title, its
    /// bars in its color.
    fn quote(&mut self, range: &Range<usize>, kind: Option<BlockQuoteKind>) {
        let (label, tone) = match kind {
            Some(BlockQuoteKind::Note) => ("Note", Tone::Accent),
            Some(BlockQuoteKind::Tip) => ("Tip", Tone::Str),
            Some(BlockQuoteKind::Important) => ("Important", Tone::Constant),
            Some(BlockQuoteKind::Warning) => ("Warning", Tone::Warning),
            Some(BlockQuoteKind::Caution) => ("Caution", Tone::Danger),
            None => ("", Tone::Border),
        };
        let first = self.lines.line_of(range.start);
        for line in first..=self.lines.last_line(range) {
            let text = self.lines.text(line);
            let start = self.lines.starts[line];
            let mut at = text.len() - text.trim_start_matches(' ').len();
            while text[at..].starts_with('>') {
                self.glyph(start + at, start + at + 1, "▎", Paint::fg(tone));
                at += 1;
                if text[at..].starts_with(' ') {
                    at += 1;
                }
                at += text[at..].len() - text[at..].trim_start_matches(' ').len();
            }
            if label.is_empty() {
                self.paint(start + at, start + text.len(), Paint::fg(Tone::Muted));
            } else if line == first && text[at..].starts_with("[!") {
                let end = text[at..].find(']').map_or(text.len(), |i| at + i + 1);
                self.glyph(start + at, start + end, label, Paint::fg(tone).bold());
            }
        }
    }
    fn code_block(&mut self, range: &Range<usize>, kind: CodeBlockKind) {
        let first = self.lines.line_of(range.start);
        let last = self.lines.last_line(range);
        let fenced = match kind {
            CodeBlockKind::Fenced(info) => Some(
                info.split([',', ' ', '{'])
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            ),
            CodeBlockKind::Indented => None,
        };
        let Some(label) = fenced else {
            for line in first..=last {
                self.looks[line].block = Block::Code;
            }
            return;
        };
        self.looks[first].block = Block::Fence(Some(label));
        let fence: String = self
            .lines
            .text(first)
            .trim_start()
            .chars()
            .take(3)
            .collect();
        for line in first + 1..=last {
            self.looks[line].block = Block::Code;
        }
        // Closed: its last line is a fence too (an open one runs to the note's end).
        if last > first
            && self
                .lines
                .text(last)
                .trim_start()
                .starts_with(fence.as_str())
        {
            self.looks[last].block = Block::Fence(None);
        }
    }
    /// `code`: its backticks as the padding of a shaded word.
    fn code(&mut self, range: &Range<usize>) {
        let text = &self.lines.text[range.clone()];
        let n = text.chars().take_while(|c| *c == '`').count();
        if n == 0 || range.len() < 2 * n {
            return;
        }
        // `` `x` ``: the space inside each end goes too, as it does when read.
        let inner = &text[n..text.len() - n];
        let space = usize::from(
            inner.len() > 2
                && inner.starts_with(' ')
                && inner.ends_with(' ')
                && !inner.trim().is_empty(),
        );
        let pad = Paint::default().on(Tone::InlineBg);
        self.glyph(range.start, range.start + 1, NBSP, pad);
        self.hide(range.start + 1, range.start + n + space);
        self.hide(range.end - n - space, range.end - 1);
        self.glyph(range.end - 1, range.end, NBSP, pad);
        self.paint(
            range.start + n + space,
            range.end - n - space,
            Paint::fg(Tone::Str).on(Tone::InlineBg),
        );
    }
    /// A link's text, underlined in the accent color, its address hidden; an image's
    /// as a mark and its description.
    fn link(&mut self, range: Range<usize>, image: bool, kind: LinkType, text_end: usize) {
        let source = &self.lines.text[range.clone()];
        let (paint, open) = if image {
            (Paint::fg(Tone::Secondary).italic(), 2)
        } else {
            (
                Paint {
                    fg: Some(Tone::Accent),
                    underline: true,
                    ..Paint::default()
                },
                1,
            )
        };
        if matches!(kind, LinkType::Autolink | LinkType::Email) {
            if source.starts_with('<') && source.ends_with('>') {
                self.hide(range.start, range.start + 1);
                self.hide(range.end - 1, range.end);
                self.paint(range.start + 1, range.end - 1, paint);
            }
            return;
        }
        if !source.starts_with(if image { "![" } else { "[" }) {
            return;
        }
        // The text ends at its `]`: after the last thing in it, else right away.
        let text_end = self.lines.text[text_end.min(range.end)..range.end]
            .find(']')
            .map_or(range.end, |i| text_end + i);
        if image {
            self.glyph(range.start, range.start + open, "▣ ", paint);
        } else {
            self.hide(range.start, range.start + open);
        }
        self.paint(range.start + open, text_end, paint);
        self.hide(text_end, range.end);
    }
    /// A table read: its rows' cells, and how wide each column is drawn.
    fn table(&mut self, reading: Reading, tables: &mut Vec<Table>) {
        let index = tables.len();
        let columns = reading
            .rows
            .iter()
            .map(|(_, cells, _)| cells.len())
            .chain([reading.aligns.len()])
            .max()
            .unwrap_or(0);
        let mut widths = vec![1; columns];
        let mut head = None;
        for (line, cells, is_head) in &reading.rows {
            let text = self.lines.text(*line);
            let start = self.lines.starts[*line];
            let mut spans = Vec::new();
            for (j, cell) in cells.iter().enumerate() {
                // The cell's text, without the spaces around it.
                let raw = &self.lines.text[cell.clone()];
                let lead = raw.len() - raw.trim_start().len();
                let from = cell.start + lead;
                let to = (cell.start + raw.trim_end().len()).max(from);
                let (from, to) = (
                    self.lines.text[start..from].chars().count(),
                    self.lines.text[start..to].chars().count(),
                );
                widths[j] = widths[j].max(measure(text, &self.looks[*line].marks, from..to));
                spans.push((from, to));
            }
            if *is_head {
                head = Some(*line);
            }
            self.looks[*line].block = Block::Row {
                table: index,
                cells: spans,
                head: *is_head,
            };
        }
        if let Some(line) = head.map(|l| l + 1).filter(|l| *l < self.looks.len()) {
            self.looks[line].block = Block::Delimiter(index);
        }
        tables.push(Table {
            widths,
            aligns: reading.aligns,
        });
    }
}

/// How wide characters `range` of `line` are drawn, with `marks`.
fn measure(line: &str, marks: &[Mark], range: Range<usize>) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut width = 0;
    let mut i = range.start;
    while i < range.end.min(chars.len()) {
        for m in marks.iter().filter(|m| m.start == i && m.end == i) {
            if let What::Glyph(g, _) = &m.what {
                width += text_width(g);
            }
        }
        match marks
            .iter()
            .find(|m| m.start <= i && i < m.end && matches!(m.what, What::Hide | What::Glyph(..)))
        {
            Some(Mark {
                what: What::Glyph(g, _),
                start,
                end,
            }) if *start == i => {
                width += text_width(g);
                i = (*end).max(i + 1);
                continue;
            }
            Some(_) => {}
            None => width += chars[i].width().unwrap_or(0),
        }
        i += 1;
    }
    width
}

fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// A line drawn rendered.
#[derive(Debug, Default)]
pub struct Drawn {
    /// The text, in runs of one style, tabs already spaces.
    pub spans: Vec<(String, Style)>,
    /// The column each character of the line is drawn at (a hidden one: where the
    /// next drawn one is), and one more for its end.
    pub at: Vec<usize>,
    /// Whether each character is drawn: not hidden, nor the rest of a glyph.
    pub shown: Vec<bool>,
    /// The whole row's background: a code block's.
    pub fill: Option<Color>,
    /// A quote's bars (column, bar, style), drawn again on the rows it wraps to.
    pub bars: Vec<(usize, String, Style)>,
}

impl Drawn {
    /// How wide the line is drawn.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|(t, _)| text_width(t)).sum()
    }
    fn push(&mut self, text: &str, style: Style) {
        match self.spans.last_mut() {
            Some((last, s)) if *s == style => last.push_str(text),
            _ => self.spans.push((text.to_string(), style)),
        }
    }
}

/// Line `row` of `doc` (`raw`, as written) drawn rendered, `width` columns wide;
/// `regions` are its syntax colors, for a code block's lines.
pub fn draw(
    doc: &Doc,
    row: usize,
    raw: &str,
    regions: Option<&[(syntect::highlighting::Style, String)]>,
    p: &Palette,
    tab: usize,
    width: usize,
) -> Drawn {
    let empty = Look::default();
    let look = doc.lines.get(row).unwrap_or(&empty);
    let chars: Vec<char> = raw.chars().collect();
    let text = Style::default().fg(p.fg);
    let mut out = Drawn::default();
    let border = Style::default().fg(p.border);
    match &look.block {
        Block::Fence(label) => {
            let code = Style::default().bg(p.code_bg);
            out.fill = Some(p.code_bg);
            if let Some(label) = label.as_deref().filter(|l| !l.is_empty()) {
                out.push(
                    &format!(" {label}"),
                    code.fg(p.muted).add_modifier(Modifier::ITALIC),
                );
            }
            out.at = vec![0; chars.len() + 1];
            out.shown = vec![false; chars.len()];
            return out;
        }
        Block::Rule | Block::Underline(_) => {
            let (glyph, style) = match look.block {
                Block::Underline(1) => ('═', Style::default().fg(p.accent)),
                _ => ('─', border),
            };
            out.push(&glyph.to_string().repeat(width), style);
            out.at = vec![0; chars.len() + 1];
            out.shown = vec![false; chars.len()];
            return out;
        }
        Block::Delimiter(table) => {
            let indent = indent_width(&chars, tab);
            out.push(&" ".repeat(indent), text);
            let widths = doc
                .tables
                .get(*table)
                .map(|t| t.widths.clone())
                .unwrap_or_default();
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            out.push(&format!("├{}┤", parts.join("┼")), border);
            out.at = vec![indent; chars.len() + 1];
            out.shown = vec![false; chars.len()];
            return out;
        }
        Block::Row { table, cells, head } => {
            return row_drawn(doc, *table, cells, *head, look, &chars, p, tab);
        }
        _ => {}
    }
    let code = matches!(look.block, Block::Code);
    if code {
        out.fill = Some(p.code_bg);
    }
    let regions = doc.code.get(&row).map(Vec::as_slice).or(regions);
    // Each character's syntax color, in a code block.
    let colors: Vec<Option<Color>> = match (code, regions) {
        (true, Some(regions)) => regions
            .iter()
            .flat_map(|(style, part)| {
                let fg = style.foreground;
                part.chars()
                    .filter(|c| *c != '\n' && *c != '\r')
                    .map(move |_| Some(Color::Rgb(fg.r, fg.g, fg.b)))
            })
            .collect(),
        _ => Vec::new(),
    };
    let base = |i: usize| {
        let style = match &look.block {
            Block::FrontMatter => Style::default().fg(p.muted),
            Block::Code => Style::default()
                .fg(colors.get(i).copied().flatten().unwrap_or(p.fg))
                .bg(p.code_bg),
            _ => text,
        };
        style
    };
    inline(
        &mut out,
        &chars,
        &look.marks,
        0..chars.len(),
        &base,
        p,
        tab,
        0,
    );
    // What comes after the text: a heading's rule.
    for m in look.marks.iter().filter(|m| m.start >= chars.len()) {
        if let What::Fill(c, paint) = &m.what {
            let used = out.width();
            if width > used + 1 {
                out.push(" ", text);
                out.push(
                    &c.to_string().repeat(width - used - 1),
                    paint.style(text, p),
                );
            }
        }
    }
    out
}

/// Draw characters `range` of a line into `out`, from column `col`.
#[allow(clippy::too_many_arguments)]
fn inline(
    out: &mut Drawn,
    chars: &[char],
    marks: &[Mark],
    range: Range<usize>,
    base: &dyn Fn(usize) -> Style,
    p: &Palette,
    tab: usize,
    col: usize,
) -> usize {
    let mut col = col;
    let tab = tab.max(1);
    if out.at.len() < chars.len() + 1 {
        out.at.resize(chars.len() + 1, col);
        out.shown.resize(chars.len(), false);
    }
    let mut i = range.start;
    let end = range.end.min(chars.len());
    let glyphs_at = |out: &mut Drawn, i: usize, col: &mut usize| {
        for m in marks.iter().filter(|m| m.start == i && m.end == i) {
            if let What::Glyph(g, paint) = &m.what {
                out.push(g, paint.style(base(i), p));
                *col += text_width(g);
            }
        }
    };
    while i < end {
        glyphs_at(out, i, &mut col);
        out.at[i] = col;
        let covering = marks
            .iter()
            .find(|m| m.start <= i && i < m.end && matches!(m.what, What::Hide | What::Glyph(..)));
        match covering {
            Some(Mark {
                what: What::Glyph(g, paint),
                start,
                end: to,
            }) if *start == i => {
                let style = paint_at(marks, i).over(*paint).style(base(i), p);
                if g == "▎" {
                    out.bars.push((col, g.clone(), style));
                }
                out.push(g, style);
                out.shown[i] = true;
                col += text_width(g);
                for j in i + 1..(*to).min(chars.len()) {
                    out.at[j] = col;
                }
                i = (*to).max(i + 1);
                continue;
            }
            Some(_) => {}
            None => {
                let style = paint_at(marks, i).style(base(i), p);
                out.shown[i] = true;
                if chars[i] == '\t' {
                    let spaces = tab - col % tab;
                    out.push(&" ".repeat(spaces), style);
                    col += spaces;
                } else {
                    let mut buf = [0; 4];
                    out.push(chars[i].encode_utf8(&mut buf), style);
                    col += chars[i].width().unwrap_or(0);
                }
            }
        }
        i += 1;
    }
    if end == chars.len() {
        glyphs_at(out, end, &mut col);
    }
    out.at[end] = col;
    col
}

/// The paint of the marks over character `i`, added up in order.
fn paint_at(marks: &[Mark], i: usize) -> Paint {
    marks
        .iter()
        .filter(|m| m.start <= i && i < m.end)
        .fold(Paint::default(), |paint, m| match &m.what {
            What::Paint(top) => paint.over(*top),
            _ => paint,
        })
}

fn indent_width(chars: &[char], tab: usize) -> usize {
    let mut col = 0;
    for c in chars {
        match c {
            ' ' => col += 1,
            '\t' => col += tab.max(1) - col % tab.max(1),
            _ => break,
        }
    }
    col
}

/// A table's row: its cells between bars, each as wide as its column.
#[allow(clippy::too_many_arguments)]
fn row_drawn(
    doc: &Doc,
    table: usize,
    cells: &[(usize, usize)],
    head: bool,
    look: &Look,
    chars: &[char],
    p: &Palette,
    tab: usize,
) -> Drawn {
    let mut out = Drawn {
        at: vec![0; chars.len() + 1],
        shown: vec![false; chars.len()],
        ..Drawn::default()
    };
    let layout = doc.tables.get(table).cloned().unwrap_or_default();
    let border = Style::default().fg(p.border);
    let text = if head {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.fg)
    };
    let indent = indent_width(chars, tab);
    out.push(&" ".repeat(indent), Style::default());
    out.push("│", border);
    let mut col = indent + 1;
    // The characters before the first cell (the bar) are drawn where it is.
    let mut next = 0;
    for (j, width) in layout.widths.iter().enumerate() {
        let (from, to) = cells.get(j).copied().unwrap_or((next, next));
        for i in next..from.min(chars.len()) {
            out.at[i] = col.saturating_sub(1);
        }
        let used = if from < to {
            measure(&chars.iter().collect::<String>(), &look.marks, from..to)
        } else {
            0
        };
        let room = width.saturating_sub(used);
        let (left, right) = match layout.aligns.get(j) {
            Some(Alignment::Right) => (room, 0),
            Some(Alignment::Center) => (room / 2, room - room / 2),
            _ => (0, room),
        };
        out.push(&" ".repeat(1 + left), text);
        col += 1 + left;
        if from < to {
            col = inline(
                &mut out,
                chars,
                &look.marks,
                from..to,
                &|_| text,
                p,
                tab,
                col,
            );
        }
        out.push(&" ".repeat(right + 1), text);
        col += right + 1;
        out.push("│", border);
        col += 1;
        next = to;
    }
    for i in next..chars.len() {
        out.at[i] = col.saturating_sub(1);
    }
    out.at[chars.len()] = col;
    out
}

/// A line laid out in a note's rows, which wrap at words: drawn rendered, or as
/// written (the cursor's).
#[derive(Debug, Default)]
pub struct Laid {
    /// The text, in runs of one style, tabs as spaces.
    pub spans: Vec<(String, Style)>,
    /// The column each character of the line starts at, and one more for its end.
    pub at: Vec<usize>,
    /// Whether each character is drawn.
    pub shown: Vec<bool>,
    /// The column of the line each row starts at; the first is 0.
    pub rows: Vec<usize>,
    /// Where the rows after the first start: under a list item's text.
    pub hang: usize,
    /// A quote's bars, drawn again on the rows after the first.
    pub bars: Vec<(usize, String, Style)>,
    /// The rows' background: a code block's.
    pub fill: Option<Color>,
}

impl Laid {
    /// The row, and the column in it, that column `col` of the line is drawn at.
    pub fn place(&self, col: usize) -> (usize, usize) {
        let row = self.rows.partition_point(|&s| s <= col).saturating_sub(1);
        let lead = if row > 0 { self.hang } else { 0 };
        (row, col - self.rows[row] + lead)
    }
    /// The character drawn at column `x` of row `row`: clicked there. Past a row's
    /// text, its last character; past the line's, its end.
    pub fn char_at(&self, row: usize, x: usize) -> usize {
        let row = row.min(self.rows.len().saturating_sub(1));
        let lead = if row > 0 { self.hang } else { 0 };
        let mut col = self.rows[row] + x.saturating_sub(lead);
        let end = self.at.last().copied().unwrap_or(0);
        match self.rows.get(row + 1) {
            Some(next) => col = col.min(next.saturating_sub(1)),
            None if col >= end => return self.shown.len(),
            None => {}
        }
        let mut found = None;
        for (i, at) in self.at.iter().enumerate().take(self.shown.len()) {
            if self.shown[i] && *at <= col {
                found = Some(i);
            }
        }
        found.unwrap_or(0)
    }
    /// The text of row `row`, as spans.
    pub fn row(&self, row: usize) -> Vec<(String, Style)> {
        let start = self.rows.get(row).copied().unwrap_or(0);
        let end = self.rows.get(row + 1).copied().unwrap_or(usize::MAX);
        let mut out: Vec<(String, Style)> = Vec::new();
        let mut col = 0;
        for (text, style) in &self.spans {
            for c in text.chars() {
                let w = c.width().unwrap_or(0);
                if col >= start && col < end {
                    match out.last_mut() {
                        Some((last, s)) if s == style => last.push(c),
                        _ => out.push((c.to_string(), *style)),
                    }
                }
                col += w;
            }
        }
        out
    }
}

/// Line `row` of `doc` (`raw` as written), laid out in rows `width` columns wide:
/// rendered, or as written with its syntax colors (`regions`, else `plain`).
#[allow(clippy::too_many_arguments)]
pub fn lay(
    doc: &Doc,
    row: usize,
    raw: &str,
    rendered: bool,
    regions: Option<&[(syntect::highlighting::Style, String)]>,
    p: &Palette,
    plain: Style,
    tab: usize,
    width: usize,
) -> Laid {
    let drawn = if rendered {
        draw(doc, row, raw, regions, p, tab, width)
    } else {
        written(
            raw,
            doc.code.get(&row).map(Vec::as_slice).or(regions),
            plain,
            tab,
        )
    };
    let mut hang = drawn.at.get(content_start(raw)).copied().unwrap_or(0);
    if hang + 12 > width {
        hang = 0;
    }
    let mut cells = Vec::new();
    let mut col = 0;
    for (text, _) in &drawn.spans {
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            cells.push((col, w, c == ' ' || c == '\u{a0}'));
            col += w;
        }
    }
    let rows = breaks(&cells, width, hang);
    Laid {
        spans: drawn.spans,
        at: drawn.at,
        shown: drawn.shown,
        rows,
        hang,
        bars: drawn.bars,
        fill: drawn.fill,
    }
}

/// A line as written: its syntax colors, tabs as spaces.
fn written(
    raw: &str,
    regions: Option<&[(syntect::highlighting::Style, String)]>,
    plain: Style,
    tab: usize,
) -> Drawn {
    let styles: Vec<Style> = regions
        .unwrap_or_default()
        .iter()
        .flat_map(|(style, part)| {
            let fg = style.foreground;
            let mut s = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::BOLD)
            {
                s = s.add_modifier(Modifier::BOLD);
            }
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::ITALIC)
            {
                s = s.add_modifier(Modifier::ITALIC);
            }
            part.chars()
                .filter(|c| *c != '\n' && *c != '\r')
                .map(move |_| s)
        })
        .collect();
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Drawn {
        at: vec![0; chars.len() + 1],
        shown: vec![true; chars.len()],
        ..Drawn::default()
    };
    let tab = tab.max(1);
    let mut col = 0;
    for (i, c) in chars.iter().enumerate() {
        out.at[i] = col;
        let style = styles.get(i).copied().unwrap_or(plain);
        if *c == '\t' {
            let spaces = tab - col % tab;
            out.push(&" ".repeat(spaces), style);
            col += spaces;
        } else if !c.is_control() {
            let mut buf = [0; 4];
            out.push(c.encode_utf8(&mut buf), style);
            col += c.width().unwrap_or(0);
        }
    }
    out.at[chars.len()] = col;
    out
}

/// Where a line's text starts, after its indentation, quote marks, list marker and
/// to-do box: the rows it wraps to line up there.
fn content_start(line: &str) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let skip = |mut i: usize| {
        while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
            i += 1;
        }
        i
    };
    let mut i = skip(0);
    while chars.get(i) == Some(&'>') {
        i = skip(i + 1);
    }
    let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if matches!(chars.get(i), Some('-' | '*' | '+')) && chars.get(i + 1) == Some(&' ') {
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
    i.min(chars.len())
}

/// Where a line of drawn `cells` (column, width, whether a space) breaks into rows:
/// after the last space that fits, else where it's full. The first row is `width`
/// wide, the rest `width - hang`.
fn breaks(cells: &[(usize, usize, bool)], width: usize, hang: usize) -> Vec<usize> {
    let mut rows = vec![0];
    let mut start = 0;
    let mut room = width.max(1);
    let mut space: Option<usize> = None;
    for &(col, w, is_space) in cells {
        if col + w > start + room && col > start && !is_space {
            let at = match space {
                Some(end) if end > start && end <= col => end,
                _ => col,
            };
            rows.push(at);
            start = at;
            room = width.saturating_sub(hang).max(1);
            space = None;
            // A word wider than a row is cut where the row is full.
            while col + w > start + room && col > start {
                start += room;
                rows.push(start);
            }
        }
        if is_space {
            space = Some(col + w);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Line `row` of `note` as drawn: its text.
    fn shown(note: &str, row: usize) -> String {
        let (lines, tables) = read(note);
        let doc = Doc {
            revision: 0,
            lines,
            tables,
            code: HashMap::new(),
        };
        let raw = note.split('\n').nth(row).unwrap();
        let p = Palette::new(&crate::config::Theme::default());
        draw(&doc, row, raw, None, &p, 4, 30)
            .spans
            .iter()
            .map(|(t, _)| t.as_str())
            .collect()
    }

    #[test]
    fn the_marks_go_and_what_they_mark_stays() {
        assert_eq!(shown("# Notes", 0), " Notes ");
        assert_eq!(shown("## Plan ##", 0), format!("Plan {}", "─".repeat(25)));
        assert_eq!(shown("### Small", 0), "Small");
        assert_eq!(
            shown("Some **bold**, *it* and ~~old~~.", 0),
            "Some bold, it and old."
        );
        assert_eq!(
            shown("Run `cargo test` now", 0),
            "Run \u{a0}cargo test\u{a0} now"
        );
        assert_eq!(shown("`` a ` b ``", 0), "\u{a0}a ` b\u{a0}");
        assert_eq!(
            shown("See [the docs](https://x.io \"t\") here", 0),
            "See the docs here"
        );
        assert_eq!(shown("<https://x.io>", 0), "https://x.io");
        assert_eq!(shown("![a cat](cat.png)", 0), "▣ a cat");
        assert_eq!(shown("a \\*star\\*", 0), "a *star*");
        assert_eq!(shown("- one\n  - two\n    - three", 2), "    ▪ three");
        assert_eq!(shown("- one\n  - two", 1), "  ◦ two");
        assert_eq!(shown("3. third", 0), "3. third");
        assert_eq!(shown("- [ ] milk\n- [x] eggs", 0), "☐ milk");
        assert_eq!(shown("- [ ] milk\n- [x] eggs", 1), "☑ eggs");
        assert_eq!(shown("> said\n> again", 1), "▎ again");
        assert_eq!(shown("> [!WARNING]\n> careful", 0), "▎ Warning");
        assert_eq!(shown("---", 0), "─".repeat(30));
        assert_eq!(shown("Title\n=====", 1), "═".repeat(30));
        assert_eq!(shown("Title\n=====", 0), "Title");
    }

    #[test]
    fn code_blocks_tables_and_to_dos_are_read() {
        let note =
            "```rust\nfn main() {}\n```\n\n| a | long b |\n|---|--:|\n| **x** | 1 |\n\n- [x] done";
        let (lines, tables) = read(note);
        assert_eq!(lines[0].block, Block::Fence(Some("rust".into())));
        assert_eq!(lines[1].block, Block::Code);
        assert_eq!(lines[2].block, Block::Fence(None));
        assert!(matches!(lines[4].block, Block::Row { head: true, .. }));
        assert_eq!(lines[5].block, Block::Delimiter(0));
        // The bold's marks don't count: "x" is one column wide.
        assert_eq!(tables[0].widths, [1, 6]);
        assert_eq!(lines[8].task, Some((2, true)));
        assert_eq!(shown(note, 0), " rust");
        assert_eq!(shown(note, 4), "│ a │ long b │");
        assert_eq!(shown(note, 5), "├───┼────────┤");
        assert_eq!(shown(note, 6), "│ x │      1 │");
        // An open fence runs to the end.
        let (lines, _) = read("```\ncode");
        assert_eq!(lines[1].block, Block::Code);
    }

    #[test]
    fn long_lines_wrap_at_words_and_under_their_text() {
        let (lines, tables) = read("- a list item with several words");
        let doc = Doc {
            revision: 0,
            lines,
            tables,
            code: HashMap::new(),
        };
        let p = Palette::new(&crate::config::Theme::default());
        let raw = "- a list item with several words";
        let laid = lay(&doc, 0, raw, true, None, &p, Style::default(), 4, 14);
        let rows: Vec<String> = (0..laid.rows.len())
            .map(|r| laid.row(r).iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        // "• a list item " fits 14 columns; the rest goes under "a".
        assert_eq!(rows, ["• a list item ", "with several ", "words"]);
        assert_eq!(laid.hang, 2);
        // Where "words" (character 27) is drawn, and back.
        assert_eq!(laid.place(laid.at[27]), (2, 2));
        assert_eq!(laid.char_at(2, 2), 27);
        // Past a row's end: its last character; past the line's: its end.
        assert_eq!(laid.char_at(0, 30), 13);
        assert_eq!(laid.char_at(2, 30), raw.chars().count());
        // As written, a word too long for a row is cut where it's full.
        let laid = lay(
            &doc,
            0,
            "abcdefghijklmnopqrstuvwxyz",
            false,
            None,
            &p,
            Style::default(),
            4,
            14,
        );
        assert_eq!(laid.rows, [0, 14]);
        assert_eq!(content_start("> - [ ] x"), 8);
        assert_eq!(content_start("   12. x"), 7);
        assert_eq!(content_start("plain"), 0);
    }

    #[test]
    fn a_click_finds_the_character_drawn_there() {
        let (lines, tables) = read("Some **bold** text");
        let doc = Doc {
            revision: 0,
            lines,
            tables,
            code: HashMap::new(),
        };
        let p = Palette::new(&crate::config::Theme::default());
        let raw = "Some **bold** text";
        let laid = lay(&doc, 0, raw, true, None, &p, Style::default(), 4, 40);
        // "Some bold text": column 5 is the b of bold, character 7 of the line.
        assert_eq!(laid.char_at(0, 5), 7);
        assert_eq!(laid.at[7], 5);
        assert!(!laid.shown[5] && laid.shown[7]);
        // Past the end: the line's end.
        assert_eq!(laid.char_at(0, 30), 18);
        assert_eq!(laid.at[18], 14);
    }
}
