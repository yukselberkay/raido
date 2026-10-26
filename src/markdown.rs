use crate::{
    config::{color, Theme},
    editor::Buffer,
    highlight::Syntax,
};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{collections::HashMap, path::Path};
use syntect::{easy::HighlightLines, util::LinesWithEndings};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Readable measure: wide panes center the document instead of stretching prose.
pub const MAX_WIDTH: usize = 100;
const MAX_SOURCE: usize = 4 * 1024 * 1024;
// Inline code keeps its spacing and never wraps at its own spaces.
const NBSP: char = '\u{a0}';

pub fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["md", "markdown", "mdown", "mkd", "mkdn"]
            .iter()
            .any(|x| x.eq_ignore_ascii_case(e))
    })
}

#[derive(Default)]
pub struct Document {
    pub lines: Vec<Line<'static>>,
    /// Rendered line of every heading, for `[` / `]` navigation.
    pub headings: Vec<usize>,
    /// (source line, rendered line) at each block start, both ascending.
    anchors: Vec<(usize, usize)>,
}

impl Document {
    /// First rendered line of the block containing `source_line`.
    pub fn line_for_source(&self, source_line: usize) -> usize {
        match self.anchors.partition_point(|(s, _)| *s <= source_line) {
            0 => 0,
            i => self.anchors[i - 1].1,
        }
    }
    /// Source line of the block shown at rendered `line`.
    pub fn source_for_line(&self, line: usize) -> usize {
        match self.anchors.partition_point(|(_, r)| *r <= line) {
            0 => 0,
            i => self.anchors[i - 1].0,
        }
    }
}

struct Cached {
    revision: u64,
    width: usize,
    document: Document,
}

/// Rendered documents per buffer, rebuilt only when the text or width changes.
#[derive(Default)]
pub struct Previews {
    cache: HashMap<usize, Cached>,
}

impl Previews {
    /// Styles are baked into rendered lines, so a theme change must clear the cache.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
    pub fn get(&self, id: usize) -> Option<&Document> {
        self.cache.get(&id).map(|c| &c.document)
    }
    pub fn document(
        &mut self,
        id: usize,
        buffer: &Buffer,
        width: usize,
        theme: &Theme,
        syntax: &Syntax,
    ) -> &Document {
        let stale = self
            .cache
            .get(&id)
            .is_none_or(|c| c.revision != buffer.revision || c.width != width);
        if stale {
            let document = if buffer.text.len_bytes() > MAX_SOURCE {
                render(
                    "> [!WARNING]\n> File too large to preview. Press **e** to edit the source.",
                    width,
                    theme,
                    syntax,
                )
            } else {
                render(&buffer.text.to_string(), width, theme, syntax)
            };
            self.cache.insert(
                id,
                Cached {
                    revision: buffer.revision,
                    width,
                    document,
                },
            );
        }
        &self.cache[&id].document
    }
}

struct Palette {
    fg: Color,
    muted: Color,
    accent: Color,
    secondary: Color,
    warning: Color,
    border: Color,
    background: Color,
    string: Color,
    function: Color,
    constant: Color,
    danger: Color,
    code_bg: Color,
    inline_bg: Color,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let syntax = theme.syntax_colors.as_ref();
        let pick = |f: fn(&crate::config::SyntaxColors) -> &String, fallback: &String| {
            color(syntax.map(f).unwrap_or(fallback))
        };
        let background = color(&theme.background);
        let fg = color(&theme.foreground);
        Self {
            fg,
            muted: color(&theme.muted),
            accent: color(&theme.accent),
            secondary: color(&theme.secondary),
            warning: color(&theme.warning),
            border: color(&theme.border),
            background,
            string: pick(|s| &s.string, &theme.secondary),
            function: pick(|s| &s.function, &theme.accent),
            constant: pick(|s| &s.constant, &theme.secondary),
            danger: theme
                .ansi
                .as_ref()
                .map(|a| color(&a[9]))
                .unwrap_or_else(|| color(&theme.warning)),
            code_bg: mix(background, fg, 0.07),
            inline_bg: mix(background, fg, 0.13),
        }
    }
}

pub(crate) fn mix(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ => a,
    }
}

enum Container {
    Quote(Style),
    /// List item or footnote; `marker` is printed once, on the item's first line.
    Item {
        indent: usize,
        marker: Option<Vec<Span<'static>>>,
    },
}

#[derive(Default)]
struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header: usize,
}

struct Renderer<'a> {
    syntax: &'a Syntax,
    p: Palette,
    width: usize,
    doc: Document,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    lists: Vec<Option<u64>>,
    links: Vec<(String, usize)>,
    code: Option<(String, String)>,
    html: Option<String>,
    table: Option<Table>,
    gap: bool,
}

pub fn render(source: &str, width: usize, theme: &Theme, syntax: &Syntax) -> Document {
    let mut options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
        | Options::ENABLE_GFM;
    // Front matter only exists at the very top; elsewhere `---` is a rule.
    if source.starts_with("---") {
        options |= Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    }
    let newlines: Vec<usize> = source
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();
    let mut r = Renderer {
        syntax,
        p: Palette::new(theme),
        width: width.max(12),
        doc: Document::default(),
        inline: Vec::new(),
        styles: Vec::new(),
        containers: Vec::new(),
        lists: Vec::new(),
        links: Vec::new(),
        code: None,
        html: None,
        table: None,
        gap: false,
    };
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        let block = matches!(
            &event,
            Event::Start(
                Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::CodeBlock(_)
                    | Tag::Item
                    | Tag::Table(_)
                    | Tag::BlockQuote(_)
                    | Tag::MetadataBlock(_)
            ) | Event::Rule
        );
        r.event(event);
        if block {
            let line = newlines.partition_point(|&n| n < range.start);
            let at = r.doc.lines.len();
            if r.doc
                .anchors
                .last()
                .is_none_or(|&(s, o)| line > s && at >= o)
            {
                r.doc.anchors.push((line, at));
            }
        }
    }
    r.flush();
    while r
        .doc
        .lines
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        r.doc.lines.pop();
    }
    r.doc
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default().fg(self.p.fg), |a, s| a.patch(*s))
    }
    fn prefix(&mut self, consume: bool) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        for c in &mut self.containers {
            match c {
                Container::Quote(style) => out.push(Span::styled("▎ ", *style)),
                Container::Item { indent, marker } => match marker.take_if(|_| consume) {
                    Some(m) => out.extend(m),
                    None => out.push(Span::raw(" ".repeat(*indent))),
                },
            }
        }
        out
    }
    fn avail(&self) -> usize {
        let used: usize = self
            .containers
            .iter()
            .map(|c| match c {
                Container::Quote(_) => 2,
                Container::Item { indent, .. } => *indent,
            })
            .sum();
        self.width.saturating_sub(used).max(8)
    }
    fn emit(&mut self, content: Vec<Span<'static>>) {
        let mut spans = self.prefix(true);
        spans.extend(content);
        self.doc.lines.push(Line::from(spans));
        self.gap = false;
    }
    fn blank(&mut self) {
        if !self.doc.lines.is_empty() {
            let spans = self.prefix(false);
            self.doc.lines.push(Line::from(spans));
        }
        self.gap = false;
    }
    fn begin_block(&mut self) {
        self.flush();
        if self.gap {
            self.blank();
        }
    }
    fn end_block(&mut self) {
        self.flush();
        self.gap = true;
    }
    fn flush(&mut self) {
        if self.inline.iter().all(|s| s.content.trim().is_empty()) {
            self.inline.clear();
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        for line in wrap(spans, self.avail()) {
            self.emit(line);
        }
    }
    fn push_style(&mut self, style: Style) {
        self.styles.push(style);
    }
    fn pop_style(&mut self) {
        self.styles.pop();
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else {
                    let style = self.style();
                    self.inline.push(Span::styled(text.into_string(), style));
                }
            }
            Event::Code(text) => {
                let style = self
                    .style()
                    .fg(self.p.string)
                    .bg(self.p.inline_bg)
                    .remove_modifier(Modifier::UNDERLINED);
                let padded = format!("{NBSP}{}{NBSP}", text.replace(' ', "\u{a0}"));
                self.inline.push(Span::styled(padded, style));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let style = self.style().fg(self.p.constant);
                self.inline.push(Span::styled(text.into_string(), style));
            }
            Event::Html(text) => {
                if let Some(html) = &mut self.html {
                    html.push_str(&text);
                }
            }
            Event::InlineHtml(text) => {
                let tag = text.trim().to_ascii_lowercase();
                if tag.starts_with("<br") {
                    self.inline.push(Span::raw("\n"));
                }
            }
            Event::FootnoteReference(label) => {
                let style = Style::default().fg(self.p.accent);
                self.inline.push(Span::styled(format!("[{label}]"), style));
            }
            Event::SoftBreak => self.inline.push(Span::styled(" ", self.style())),
            Event::HardBreak => self.inline.push(Span::raw("\n")),
            Event::Rule => {
                self.begin_block();
                let rule = "─".repeat(self.avail());
                self.emit(vec![Span::styled(rule, Style::default().fg(self.p.border))]);
                self.gap = true;
            }
            Event::TaskListMarker(checked) => {
                let (symbol, fg) = if checked {
                    ("☑ ", self.p.string)
                } else {
                    ("☐ ", self.p.muted)
                };
                if let Some(Container::Item { indent, marker }) = self.containers.last_mut() {
                    *indent = 2;
                    *marker = Some(vec![Span::styled(symbol, Style::default().fg(fg))]);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                let style = match level {
                    HeadingLevel::H1 => Style::default().fg(self.p.background).bg(self.p.accent),
                    HeadingLevel::H2 => Style::default().fg(self.p.accent),
                    HeadingLevel::H3 => Style::default().fg(self.p.secondary),
                    HeadingLevel::H4 => Style::default().fg(self.p.function),
                    HeadingLevel::H5 => Style::default().fg(self.p.fg),
                    HeadingLevel::H6 => Style::default().fg(self.p.muted),
                };
                self.push_style(style.add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(kind) => {
                self.begin_block();
                let (label, fg) = match kind {
                    Some(BlockQuoteKind::Note) => ("Note", self.p.accent),
                    Some(BlockQuoteKind::Tip) => ("Tip", self.p.string),
                    Some(BlockQuoteKind::Important) => ("Important", self.p.constant),
                    Some(BlockQuoteKind::Warning) => ("Warning", self.p.warning),
                    Some(BlockQuoteKind::Caution) => ("Caution", self.p.danger),
                    None => ("", self.p.border),
                };
                self.containers
                    .push(Container::Quote(Style::default().fg(fg)));
                if label.is_empty() {
                    self.push_style(Style::default().fg(self.p.muted));
                } else {
                    let title = Style::default().fg(fg).add_modifier(Modifier::BOLD);
                    self.emit(vec![Span::styled(label, title)]);
                    self.push_style(Style::default());
                }
            }
            Tag::CodeBlock(kind) => {
                self.begin_block();
                let label = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split([',', ' ', '{'])
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((label, String::new()));
            }
            Tag::MetadataBlock(_) => {
                self.begin_block();
                self.code = Some(("front matter".into(), String::new()));
            }
            Tag::HtmlBlock => self.html = Some(String::new()),
            Tag::List(start) => {
                self.begin_block();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let (text, style) = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let text = format!("{n}. ");
                        *n += 1;
                        (text, Style::default().fg(self.p.accent))
                    }
                    _ => (
                        format!("{} ", ["•", "◦", "▪"][depth % 3]),
                        Style::default().fg(self.p.accent),
                    ),
                };
                self.containers.push(Container::Item {
                    indent: text.width(),
                    marker: Some(vec![Span::styled(text, style)]),
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.begin_block();
                let text = format!("[{label}] ");
                self.containers.push(Container::Item {
                    indent: text.width(),
                    marker: Some(vec![Span::styled(text, Style::default().fg(self.p.accent))]),
                });
                self.push_style(Style::default().fg(self.p.muted));
            }
            Tag::Table(aligns) => {
                self.begin_block();
                self.table = Some(Table {
                    aligns,
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
                self.push_style(
                    Style::default()
                        .fg(self.p.accent)
                        .add_modifier(Modifier::BOLD),
                );
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.into_string(), self.inline.len()));
                self.push_style(
                    Style::default()
                        .fg(self.p.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => {
                let style = Style::default().fg(self.p.secondary);
                self.inline.push(Span::styled("[image: ", style));
                self.push_style(style.add_modifier(Modifier::ITALIC));
            }
            _ => self.push_style(Style::default()),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_block(),
            TagEnd::Heading(level) => {
                let style = self.style();
                self.pop_style();
                let spans = std::mem::take(&mut self.inline);
                self.doc.headings.push(self.doc.lines.len());
                let avail = self.avail();
                if level == HeadingLevel::H1 {
                    for line in wrap(spans, avail.saturating_sub(2)) {
                        let mut pill = vec![Span::styled(" ", style)];
                        pill.extend(line);
                        pill.push(Span::styled(" ", style));
                        self.emit(pill);
                    }
                } else {
                    for line in wrap(spans, avail) {
                        self.emit(line);
                    }
                    if level == HeadingLevel::H2 {
                        let rule =
                            Span::styled("─".repeat(avail), Style::default().fg(self.p.border));
                        self.emit(vec![rule]);
                    }
                }
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.pop_style();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::CodeBlock | TagEnd::MetadataBlock(_) => {
                if let Some((label, text)) = self.code.take() {
                    self.code_block(&label, &text);
                }
                self.gap = true;
            }
            TagEnd::HtmlBlock => {
                let text = self.html.take().map(|h| strip_html(&h)).unwrap_or_default();
                if !text.trim().is_empty() {
                    self.begin_block();
                    let style = Style::default().fg(self.p.muted);
                    self.inline.push(Span::styled(text, style));
                    self.end_block();
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.gap = true;
            }
            TagEnd::Item => {
                self.flush();
                if matches!(
                    self.containers.last(),
                    Some(Container::Item {
                        marker: Some(_),
                        ..
                    })
                ) {
                    self.emit(Vec::new());
                }
                self.containers.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.pop_style();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::TableHead => {
                self.pop_style();
                if let Some(t) = &mut self.table {
                    t.header = t.rows.len();
                }
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline)
                    .into_iter()
                    .map(|s| Span::styled(s.content.replace('\n', " "), s.style))
                    .collect();
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
                self.gap = true;
            }
            TagEnd::Link => {
                self.pop_style();
                if let Some((dest, start)) = self.links.pop() {
                    let text: String = self.inline[start.min(self.inline.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    let shown = dest
                        .trim_start_matches("mailto:")
                        .trim_start_matches("https://")
                        .trim_start_matches("http://");
                    if !dest.is_empty()
                        && !dest.starts_with('#')
                        && text.trim() != dest
                        && text.trim() != shown
                    {
                        let style = Style::default().fg(self.p.muted);
                        self.inline.push(Span::styled(format!(" ({shown})"), style));
                    }
                }
            }
            TagEnd::Image => {
                self.pop_style();
                let style = Style::default().fg(self.p.secondary);
                self.inline.push(Span::styled("]", style));
            }
            _ => self.pop_style(),
        }
    }

    fn code_block(&mut self, label: &str, text: &str) {
        let width = self.avail();
        let base = Style::default().bg(self.p.code_bg).fg(self.p.fg);
        let header = Style::default()
            .bg(self.p.code_bg)
            .fg(self.p.muted)
            .add_modifier(Modifier::ITALIC);
        let top = if label.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(format!(" {label}"), header)]
        };
        self.emit(pad(top, width, base));
        let grammar = Some(label)
            .filter(|l| !l.is_empty())
            .and_then(|l| self.syntax.syntaxes.find_syntax_by_token(l));
        let mut highlighter = grammar.map(|g| HighlightLines::new(g, &self.syntax.theme));
        for line in LinesWithEndings::from(text) {
            let line = line.replace('\t', "    ");
            let mut spans = vec![Span::styled("  ", base)];
            let regions = highlighter
                .as_mut()
                .and_then(|h| h.highlight_line(&line, &self.syntax.syntaxes).ok());
            match regions {
                Some(regions) => {
                    for (style, part) in regions {
                        let fg = style.foreground;
                        let part = part.trim_end_matches(['\n', '\r']);
                        if !part.is_empty() {
                            spans.push(Span::styled(
                                part.to_string(),
                                base.fg(Color::Rgb(fg.r, fg.g, fg.b)),
                            ));
                        }
                    }
                }
                None => spans.push(Span::styled(
                    line.trim_end_matches(['\n', '\r']).to_string(),
                    base,
                )),
            }
            self.emit(pad(truncate(spans, width.saturating_sub(1)), width, base));
        }
        self.emit(pad(Vec::new(), width, base));
    }

    fn render_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![3; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(spans_width(cell));
            }
        }
        let avail = self.avail();
        // Shrink the widest column first until the table fits; cells then truncate.
        while widths.iter().sum::<usize>() + 3 * columns + 1 > avail {
            let (i, &w) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .expect("columns > 0");
            if w <= 3 {
                break;
            }
            widths[i] -= 1;
        }
        let border = Style::default().fg(self.p.border);
        let rule = |l: &str, m: &str, r: &str| {
            let middle: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::styled(format!("{l}{}{r}", middle.join(m)), border)]
        };
        self.emit(rule("╭", "┬", "╮"));
        let empty = Vec::new();
        for (r, row) in table.rows.iter().enumerate() {
            if r > 0 && r == table.header {
                self.emit(rule("├", "┼", "┤"));
            }
            let mut spans = vec![Span::styled("│", border)];
            for (i, width) in widths.iter().enumerate() {
                let cell = truncate(row.get(i).unwrap_or(&empty).clone(), *width);
                let space = width - spans_width(&cell);
                let left = match table.aligns.get(i) {
                    Some(Alignment::Right) => space,
                    Some(Alignment::Center) => space / 2,
                    _ => 0,
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                spans.extend(cell);
                spans.push(Span::raw(" ".repeat(space - left + 1)));
                spans.push(Span::styled("│", border));
            }
            self.emit(spans);
        }
        self.emit(rule("╰", "┴", "╯"));
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn pad(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Vec<Span<'static>> {
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    spans
}

/// Cut spans to `width` columns, marking the cut with an ellipsis.
fn truncate(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= width {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    let limit = width.saturating_sub(1);
    for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > limit {
                out.push(Span::styled(text, span.style));
                if width > 0 {
                    out.push(Span::styled("…", span.style));
                }
                return out;
            }
            used += w;
            text.push(c);
        }
        out.push(Span::styled(text, span.style));
    }
    out
}

/// Greedy word wrap across styled spans. Spaces and tabs separate words; `\n`
/// forces a break; words wider than the line are split by character.
fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    struct Word {
        parts: Vec<(String, Style)>,
        width: usize,
        space: Option<Style>,
        hard: bool,
    }
    let width = width.max(1);
    let mut words: Vec<Word> = Vec::new();
    let mut current: Option<Word> = None;
    let mut space = None;
    let mut hard = false;
    for span in spans {
        for c in span.content.chars() {
            match c {
                '\n' => {
                    words.extend(current.take());
                    hard = true;
                    space = None;
                }
                ' ' | '\t' | '\r' => {
                    words.extend(current.take());
                    space = space.or(Some(span.style));
                }
                _ => {
                    let word = current.get_or_insert_with(|| Word {
                        parts: Vec::new(),
                        width: 0,
                        space: space.take(),
                        hard: std::mem::take(&mut hard),
                    });
                    match word.parts.last_mut() {
                        Some((text, style)) if *style == span.style => text.push(c),
                        _ => word.parts.push((c.to_string(), span.style)),
                    }
                    word.width += c.width().unwrap_or(0);
                }
            }
        }
    }
    words.extend(current);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for word in words {
        if word.hard && used > 0 {
            lines.push(Vec::new());
            used = 0;
        }
        let mut gap = if used > 0 { word.space } else { None };
        if used > 0 && used + usize::from(gap.is_some()) + word.width > width {
            lines.push(Vec::new());
            used = 0;
            gap = None;
        }
        let line = lines.last_mut().expect("at least one line");
        if let Some(style) = gap {
            line.push(Span::styled(" ", style));
            used += 1;
        }
        if used + word.width <= width {
            for (text, style) in word.parts {
                lines
                    .last_mut()
                    .expect("line")
                    .push(Span::styled(text, style));
            }
            used += word.width;
            continue;
        }
        for (text, style) in word.parts {
            for c in text.chars() {
                let w = c.width().unwrap_or(0);
                if used > 0 && used + w > width {
                    lines.push(Vec::new());
                    used = 0;
                }
                lines
                    .last_mut()
                    .expect("line")
                    .push(Span::styled(c.to_string(), style));
                used += w;
            }
        }
    }
    lines
}

/// Plain-text view of an HTML block: comments and tags dropped, images keep their alt text.
fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(body) = rest.strip_prefix("<!--") {
            rest = body.find("-->").map_or("", |end| &body[end + 3..]);
            continue;
        }
        let end = rest.find('>').map_or(rest.len(), |e| e + 1);
        let tag = &rest[..end];
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with("<img") {
            let alt = lower
                .find("alt=")
                .and_then(|i| {
                    let value = &tag[i + 4..];
                    let quote = value.chars().next()?;
                    let value = &value[1..];
                    value.find(quote).map(|e| value[..e].to_string())
                })
                .unwrap_or_default();
            out.push_str(&format!("[image: {alt}] "));
        } else if lower.starts_with("<br") || lower.starts_with("<p") || lower.starts_with("</p") {
            out.push('\n');
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn plain(doc: &Document) -> Vec<String> {
        doc.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }
    fn render_plain(source: &str, width: usize) -> (Document, Vec<String>) {
        let theme = config::load_theme_from("visual-studio-dark", None).unwrap();
        let syntax = Syntax::new(&theme);
        let doc = render(source, width, &theme, &syntax);
        let text = plain(&doc);
        (doc, text)
    }

    #[test]
    fn renders_common_markdown_blocks() {
        let source = "# Raido\n\nSome **bold** and `code` with [a link](https://example.com).\n\n\
## Lists\n\n- one\n  - nested\n- [x] done\n- [ ] todo\n\n1. first\n2. second\n\n\
> quoted\n\n> [!WARNING]\n> careful\n\n```rust\nfn main() {}\n```\n\n---\n\n\
| Name | Size |\n|:-----|-----:|\n| a | 1 |\n";
        let (doc, text) = render_plain(source, 60);
        assert_eq!(text[0], " Raido ");
        assert!(text
            .iter()
            .any(|l| l.contains("bold") && l.contains("(example.com)")));
        assert!(text.iter().any(|l| l.contains("\u{a0}code\u{a0}")));
        assert!(text.iter().any(|l| l == "• one"));
        assert!(text.iter().any(|l| l == "  ◦ nested"));
        assert!(text.iter().any(|l| l == "☑ done"));
        assert!(text.iter().any(|l| l == "☐ todo"));
        assert!(text.iter().any(|l| l == "2. second"));
        assert!(text.iter().any(|l| l == "▎ quoted"));
        assert!(text.iter().any(|l| l == "▎ Warning"));
        assert!(text.iter().any(|l| l.starts_with(" rust")));
        assert!(text.iter().any(|l| l.starts_with("  fn main() {}")));
        assert!(text.iter().any(|l| l.starts_with("╭") && l.ends_with("╮")));
        assert!(text.iter().any(|l| l == "│ a    │    1 │"));
        assert!(text.iter().all(|l| l.width() <= 60), "{text:#?}");
        assert_eq!(doc.headings.len(), 2);
        // Tight list: no blank line between nested items.
        let one = text.iter().position(|l| l == "• one").unwrap();
        assert_eq!(text[one + 1], "  ◦ nested");
    }

    #[test]
    fn wraps_prose_and_keeps_list_indent() {
        let (_, text) = render_plain(
            "- alpha beta gamma delta epsilon zeta eta theta iota kappa\n",
            20,
        );
        assert!(text.len() > 2);
        assert!(text[0].starts_with("• "));
        assert!(text[1..].iter().all(|l| l.starts_with("  ")));
        assert!(text.iter().all(|l| l.width() <= 20));
        let (_, long) = render_plain(&"x".repeat(50), 20);
        assert_eq!(long.len(), 3);
    }

    #[test]
    fn source_anchors_map_both_ways() {
        let source = "# Title\n\nfirst paragraph\n\nsecond paragraph\n";
        let (doc, text) = render_plain(source, 40);
        let line = doc.line_for_source(4);
        assert_eq!(text[line], "second paragraph");
        assert_eq!(doc.source_for_line(line), 4);
        assert_eq!(doc.line_for_source(0), 0);
    }

    #[test]
    fn html_is_reduced_to_text_and_comments_vanish() {
        assert_eq!(
            strip_html("<p align=\"center\"><img src=\"x.png\" alt=\"Logo\"></p>\n<!-- hi -->"),
            "[image: Logo]"
        );
        let (_, text) = render_plain("<!-- hidden -->\n\ntext\n", 40);
        assert_eq!(text, vec!["text"]);
        // `---` after the first line is a rule, never front matter.
        let (_, text) = render_plain("# T\n\n---\nbody\n\n---\n", 20);
        assert!(text.iter().all(|l| !l.contains("front matter")), "{text:?}");
    }

    #[test]
    fn markdown_extensions_are_detected() {
        assert!(is_markdown(Path::new("README.md")));
        assert!(is_markdown(Path::new("notes.MARKDOWN")));
        assert!(!is_markdown(Path::new("main.rs")));
    }
}
