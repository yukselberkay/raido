//! Data shown as a grid, read-only: spreadsheets (Excel's .xlsx, .xlsm, .xlsb and
//! .xls, OpenDocument's .ods), SQLite databases (a tab per table) and CSV or TSV
//! files (from their buffer, so they stay editable as text and the grid follows
//! the edits). A spreadsheet's cell shows the value the file saved (formulas
//! aren't worked out again) as its number format asks: 15%, 1,234.50, € 9.99,
//! 1.23E+04; dates as 2024-03-15. Files are read in the background, the sheet or
//! table you see first.
use crate::editor::Buffer;
use calamine::{
    open_workbook_auto, Data, ExcelDateTime, FontWeight, HorizontalAlignment, Range, Reader,
    SheetType, SheetVisible, StyleRange,
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant, SystemTime},
};
use unicode_width::UnicodeWidthStr;

/// The files shown as a spreadsheet.
const EXTENSIONS: [&str; 7] = ["xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods"];
/// Larger workbooks aren't read: a sheet's cells take far more memory than the file.
const MAX_FILE: u64 = 64 << 20;
/// A database table's rows read; the rest are counted.
const MAX_ROWS: u64 = 100_000;
/// Rows read to size the columns.
const SIZED_ROWS: u32 = 2000;
/// A column's width, in characters, is at least and at most this (unless the
/// workbook sets it).
const NARROWEST: u16 = 3;
const WIDEST: u16 = 40;
/// A row of wrapped text is at most this many lines tall; the cell panel below
/// the grid, and Enter, show the rest.
pub const ROW_LINES: usize = 8;

/// Whether `path` is a workbook Raido shows as a spreadsheet.
pub fn is_spreadsheet(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Whether `path` is a SQLite database: its first bytes say so, whatever its name.
pub fn is_database(path: &Path) -> bool {
    let mut head = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == b"SQLite format 3\0"
}

/// Whether a buffer holds CSV or TSV, shown as a grid.
pub fn is_table(buffer: &Buffer) -> bool {
    buffer
        .path
        .as_deref()
        .and_then(Path::extension)
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["csv", "tsv"].contains(&e.to_ascii_lowercase().as_str()))
}

/// Data in a pane: a file's sheets or tables, or a CSV buffer's rows.
pub struct SheetView {
    pub path: PathBuf,
    pub sheets: Vec<Sheet>,
    /// The sheet shown.
    pub current: usize,
    /// The CSV or TSV buffer shown, for a grid made from text.
    pub buffer: Option<usize>,
    /// A SQLite database, its tables as sheets.
    pub database: bool,
    /// Opened by browsing the explorer: the next file browsed takes its place.
    pub peeked: bool,
    /// Why the file can't be shown.
    pub error: Option<String>,
    /// `/` and what's typed so far.
    pub typing: Option<String>,
    /// The last search, for n and N.
    pub query: String,
    /// Long text wraps inside its column, the row growing (`w` turns it off):
    /// in a workbook, the cells Excel wraps; in a table, any that don't fit.
    pub wrap: bool,
    /// Enter: the selected cell in full, over the grid, scrolled this far.
    pub reading: Option<u16>,
    /// Data rows on screen, recorded at draw time for paging.
    pub page: u32,
    /// Where the grid was drawn, for clicks.
    pub drawn: Drawn,
    /// The file's modification time and size when it was read.
    stamp: Option<(SystemTime, u64)>,
    loading: Option<Receiver<Loaded>>,
    /// Where to start: the sheet, row and column a session saved; without one,
    /// where the workbook was left in Excel.
    start: Option<(usize, u32, u32)>,
    /// The buffer's revision the rows were read from, and when.
    revision: Option<(u64, Instant)>,
    /// The buffer's cursor line the selection last followed.
    synced: Option<usize>,
}

/// One sheet of the workbook (or table of the database), and where you are in it.
pub struct Sheet {
    pub name: String,
    pub hidden: bool,
    /// Said after its name on its tab: "hidden", "view".
    pub note: &'static str,
    /// The cells: `None` while they're read, an error for a sheet without any.
    pub grid: Option<Result<Grid, String>>,
    /// The selected cell, and the first row and column on screen.
    pub row: u32,
    pub col: u32,
    pub top: u32,
    pub left: u32,
    /// You (or a session) chose the selected cell: the one the workbook was saved
    /// on doesn't replace it.
    placed: bool,
}

impl Sheet {
    fn new(name: String, hidden: bool, note: &'static str) -> Self {
        Self {
            name,
            hidden,
            note,
            grid: None,
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            placed: false,
        }
    }
}

/// Screen positions of the grid's columns, rows and sheet tabs.
#[derive(Default)]
pub struct Drawn {
    /// (x, width, column)
    pub columns: Vec<(u16, u16, u32)>,
    /// (y, height, row)
    pub rows: Vec<(u16, u16, u32)>,
    /// (x, width, sheet), on line `tabs_y`.
    pub tabs: Vec<(u16, u16, usize)>,
    pub tabs_y: Option<u16>,
}

/// A sheet's name and how its tab reads.
struct Tab {
    name: String,
    hidden: bool,
    note: &'static str,
}

enum Loaded {
    /// The sheets, and the one to show first (where the workbook was left).
    Names(Vec<Tab>, usize),
    Sheet(usize, Box<Result<Grid, String>>),
    Failed(String),
}

/// How a cell's text sits in its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// What a cell holds, for its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Value,
    /// `#DIV/0!` and the like.
    Error,
    /// A formula the file saved no value for (it was never calculated).
    Formula,
    /// A database's NULL, or a blob (its size shown).
    Missing,
}

/// A cell as the grid shows it.
pub struct Shown {
    pub text: String,
    pub align: Align,
    pub bold: bool,
    pub kind: Kind,
    /// Excel wraps it in its column.
    pub wrap: bool,
    /// A number (or date): never cut short; #### when its column is too narrow.
    pub number: bool,
}

/// A value in a table's row (CSV, a database).
pub enum Field {
    Text(String),
    /// A number, shown as written but lined up on the right.
    Number(String),
    Null,
    /// Binary data: its size.
    Blob(usize),
}

enum Cells {
    /// A spreadsheet's cells, with their formulas and styles.
    Book {
        values: Range<Data>,
        formulas: Range<String>,
        styles: StyleRange,
    },
    Rows(Vec<Vec<Field>>),
}

/// A sheet's cells, or a table's rows.
pub struct Grid {
    cells: Cells,
    /// Rows and columns from A1 to the last cell used.
    pub rows: u32,
    pub cols: u32,
    /// The first row used: kept on screen as a header while the sheet scrolls.
    pub first: u32,
    /// Each column's width, in characters.
    pub widths: Vec<u16>,
    /// The first row holds the columns' names (a database table's): it has no
    /// number, and the rows under it count from 1.
    pub named: bool,
    /// Rows left out: only a large table's first ones are read.
    pub unread: u64,
    /// Each row's first line in the text it came from (CSV), to edit it there.
    lines: Vec<usize>,
    /// Columns kept on screen at the left, and rows at the top, as Excel's frozen
    /// panes keep them; without frozen rows, the first row used stays as a header.
    pub frozen_cols: u32,
    pub frozen_rows: Option<u32>,
    /// Columns hidden in Excel.
    hidden: Vec<bool>,
    /// Merged cells: (first row, first column, last row, last column).
    merges: Vec<(u32, u32, u32, u32)>,
    /// The cell selected when the workbook was saved.
    pub active: Option<(u32, u32)>,
}

impl Grid {
    pub fn new(values: Range<Data>, formulas: Range<String>, styles: StyleRange) -> Self {
        let mut rows = 0;
        let mut cols = 0;
        for (row, col) in [values.end(), non_empty_end(&formulas)]
            .into_iter()
            .flatten()
        {
            rows = rows.max(row + 1);
            cols = cols.max(col + 1);
        }
        let first = [
            values.start().filter(|_| !values.is_empty()),
            formulas.start().filter(|_| !formulas.is_empty()),
        ]
        .into_iter()
        .flatten()
        .map(|(row, _)| row)
        .min()
        .unwrap_or(0);
        Self {
            cells: Cells::Book {
                values,
                formulas,
                styles,
            },
            rows,
            cols,
            first,
            widths: Vec::new(),
            named: false,
            unread: 0,
            lines: Vec::new(),
            frozen_cols: 0,
            frozen_rows: None,
            hidden: Vec::new(),
            merges: Vec::new(),
            active: None,
        }
        .sized()
    }

    /// A table's rows: `named` when the first one names the columns; `lines`, each
    /// row's line in the text it came from.
    pub fn table(rows: Vec<Vec<Field>>, named: bool, unread: u64, lines: Vec<usize>) -> Self {
        let count = rows.len() as u32;
        Self {
            rows: count,
            cols: rows.iter().map(Vec::len).max().unwrap_or(0) as u32,
            cells: Cells::Rows(rows),
            first: 0,
            widths: Vec::new(),
            named,
            unread,
            lines,
            frozen_cols: 0,
            // A table's column names stay on screen; its first record is selected.
            frozen_rows: named.then_some(1),
            hidden: Vec::new(),
            merges: Vec::new(),
            active: (named && count > 1).then_some((1, 0)),
        }
        .sized()
    }

    /// The workbook's own layout: its column widths and hidden columns, frozen
    /// panes, the cell it was saved on, and merged cells.
    fn laid_out(mut self, layout: Option<&Layout>, merges: Vec<(u32, u32, u32, u32)>) -> Self {
        self.merges = merges;
        let Some(layout) = layout else {
            return self;
        };
        for &(first, last, width, hidden) in &layout.columns {
            for col in first..=last.min(self.cols.saturating_sub(1)) {
                if let Some(w) = self.widths.get_mut(col as usize) {
                    *w = (width.floor() as u16).clamp(1, 255);
                }
                if hidden {
                    self.hidden.resize(self.cols as usize, false);
                    self.hidden[col as usize] = true;
                }
            }
        }
        self.frozen_cols = layout.frozen.0.min(self.cols);
        self.frozen_rows = (layout.frozen.1 > 0).then_some(layout.frozen.1);
        self.active = layout.active;
        self
    }

    /// Whether Excel hides this column.
    pub fn is_hidden(&self, col: u32) -> bool {
        self.hidden.get(col as usize).copied().unwrap_or(false)
    }

    /// The column `by` shown columns from `from`, hidden ones skipped, staying on
    /// the sheet.
    pub fn step_col(&self, from: u32, by: i64) -> u32 {
        let mut col = from;
        let mut left = by.unsigned_abs();
        while left > 0 {
            let next = if by < 0 {
                (0..col).rev().find(|&c| !self.is_hidden(c))
            } else {
                (col + 1..self.cols).find(|&c| !self.is_hidden(c))
            };
            match next {
                Some(next) => col = next,
                None => break,
            }
            left -= 1;
        }
        col
    }

    /// The first (or last) column shown.
    pub fn edge_col(&self, last: bool) -> u32 {
        let shown = |&c: &u32| !self.is_hidden(c);
        let found = if last {
            (0..self.cols).rev().find(shown)
        } else {
            (0..self.cols).find(shown)
        };
        found.unwrap_or(0)
    }

    /// The merged cells this one belongs to.
    pub fn merge_at(&self, row: u32, col: u32) -> Option<(u32, u32, u32, u32)> {
        self.merges
            .iter()
            .copied()
            .find(|&(r1, c1, r2, c2)| (r1..=r2).contains(&row) && (c1..=c2).contains(&col))
    }

    /// The rows kept on screen at the top: Excel's frozen rows, else the first
    /// row used (a header).
    pub fn pinned(&self) -> std::ops::Range<u32> {
        match self.frozen_rows {
            Some(rows) => 0..rows.min(self.rows),
            None => self.first..(self.first + 1).min(self.rows),
        }
    }

    /// Whether a cell wraps inside its column when text wraps (`wrap`): text Excel
    /// wraps, or in a table any text; never a number.
    pub fn wraps(&self, row: u32, col: u32, wrap: bool) -> bool {
        wrap && self.shown(row, col).wrap
    }

    /// A cell's text in lines of `width` columns: wrapped (its own line breaks
    /// kept) when it wraps, else one line.
    pub fn wrapped(&self, row: u32, col: u32, width: usize, wrap: bool) -> Vec<String> {
        if self.wraps(row, col, wrap) {
            wrap_text(&self.full(row, col), width)
        } else {
            vec![self.shown(row, col).text]
        }
    }

    /// A cell's whole text, its own line breaks kept: what the cell panel and
    /// Enter show.
    pub fn full(&self, row: u32, col: u32) -> String {
        match self.field(row, col) {
            Some(Field::Null | Field::Blob(_)) => self.shown(row, col).text,
            _ => self.copied(row, col),
        }
    }

    /// Each column as wide as its widest cell (within limits) in the rows looked
    /// at: the first and last ones and some between.
    fn sized(mut self) -> Self {
        let rows: Vec<u32> = if self.rows - self.first <= SIZED_ROWS * 2 {
            (self.first..self.rows).collect()
        } else {
            let step = ((self.rows - self.first) / SIZED_ROWS).max(1);
            (self.first..self.first + SIZED_ROWS)
                .chain((self.first..self.rows).step_by(step as usize))
                .chain(self.rows - SIZED_ROWS..self.rows)
                .collect()
        };
        self.widths = (0..self.cols)
            .map(|col| {
                let widest = rows
                    .iter()
                    .map(|&row| self.shown(row, col).text.width())
                    .max()
                    .unwrap_or(0);
                (widest.min(usize::from(WIDEST)) as u16)
                    .max(NARROWEST)
                    .max(column_name(col).len() as u16)
            })
            .collect();
        self
    }

    fn value(&self, row: u32, col: u32) -> &Data {
        match &self.cells {
            Cells::Book { values, .. } => values.get_value((row, col)).unwrap_or(&Data::Empty),
            Cells::Rows(_) => &Data::Empty,
        }
    }

    fn field(&self, row: u32, col: u32) -> Option<&Field> {
        match &self.cells {
            Cells::Rows(rows) => rows.get(row as usize)?.get(col as usize),
            Cells::Book { .. } => None,
        }
    }

    /// The cell's formula, without its `=`.
    pub fn formula(&self, row: u32, col: u32) -> Option<&str> {
        let Cells::Book { formulas, .. } = &self.cells else {
            return None;
        };
        formulas
            .get_value((row, col))
            .map(String::as_str)
            .filter(|f| !f.is_empty())
    }

    /// The cell as the grid shows it: its value in its format, on one line.
    pub fn shown(&self, row: u32, col: u32) -> Shown {
        let Cells::Book { styles, .. } = &self.cells else {
            let bold = self.named && row == 0;
            let (text, kind, align) = match self.field(row, col) {
                None => (String::new(), Kind::Value, Align::Left),
                Some(Field::Text(s)) => (one_line(s), Kind::Value, Align::Left),
                Some(Field::Number(n)) => (one_line(n), Kind::Value, Align::Right),
                Some(Field::Null) => ("NULL".into(), Kind::Missing, Align::Left),
                Some(Field::Blob(size)) => (
                    format!("blob, {} bytes", thousands(&size.to_string())),
                    Kind::Missing,
                    Align::Left,
                ),
            };
            let number = matches!(self.field(row, col), Some(Field::Number(_)));
            return Shown {
                text,
                align,
                bold,
                kind,
                wrap: !number && kind == Kind::Value,
                number,
            };
        };
        let style = styles.get((row, col));
        let bold = style
            .and_then(|s| s.font.as_ref())
            .is_some_and(|f| f.weight == FontWeight::Bold);
        let value = self.value(row, col);
        let code = style
            .and_then(|s| s.number_format.as_ref())
            .map(|f| f.format_code.as_str());
        let (text, kind, natural) = match value {
            Data::Empty => match self.formula(row, col) {
                Some(formula) => (format!("={formula}"), Kind::Formula, Align::Left),
                None => (String::new(), Kind::Value, Align::Left),
            },
            Data::String(s) => (one_line(s), Kind::Value, Align::Left),
            Data::Error(e) => (e.to_string(), Kind::Error, Align::Center),
            Data::Bool(_) => (display(value, None), Kind::Value, Align::Center),
            _ => (display(value, code), Kind::Value, Align::Right),
        };
        let align = match style
            .and_then(|s| s.alignment.as_ref())
            .map(|a| a.horizontal)
        {
            Some(HorizontalAlignment::Left) => Align::Left,
            Some(HorizontalAlignment::Center | HorizontalAlignment::CenterContinuous) => {
                Align::Center
            }
            Some(HorizontalAlignment::Right) => Align::Right,
            _ => natural,
        };
        let number = matches!(
            value,
            Data::Int(_)
                | Data::Float(_)
                | Data::DateTime(_)
                | Data::DateTimeIso(_)
                | Data::DurationIso(_)
        );
        let wrap = !number
            && style
                .and_then(|s| s.alignment.as_ref())
                .is_some_and(|a| a.wrap_text);
        Shown {
            text,
            align,
            bold,
            kind,
            wrap,
            number,
        }
    }

    /// The cell's text for the clipboard: as shown, but text keeps its lines, and
    /// a NULL or blob is nothing.
    pub fn copied(&self, row: u32, col: u32) -> String {
        match (&self.cells, self.field(row, col)) {
            (Cells::Rows(_), Some(Field::Text(s) | Field::Number(s))) => s.clone(),
            (Cells::Rows(_), _) => String::new(),
            (Cells::Book { .. }, _) => match self.value(row, col) {
                Data::String(s) => s.clone(),
                _ => self.shown(row, col).text,
            },
        }
    }

    /// The number in full (15 digits, as Excel keeps it), when the cell shows it
    /// rounded or in another form.
    pub fn exact(&self, row: u32, col: u32) -> Option<String> {
        let n = match self.value(row, col) {
            Data::Float(n) => *n,
            Data::Int(n) => *n as f64,
            _ => return None,
        };
        let exact = full(n);
        (exact != self.shown(row, col).text).then_some(exact)
    }

    /// A row's number beside it: from 1, the column names' row unnumbered.
    pub fn number(&self, row: u32) -> String {
        match (self.named, row) {
            (true, 0) => String::new(),
            (true, row) => row.to_string(),
            (false, row) => (row + 1).to_string(),
        }
    }

    /// A cell's name: B7, or in a database table, its column and row.
    pub fn label(&self, row: u32, col: u32) -> String {
        if !self.named {
            return cell_name(row, col);
        }
        let column = match self.field(0, col) {
            Some(Field::Text(name)) => one_line(name),
            _ => column_name(col),
        };
        match row {
            0 => format!("column {}", col + 1),
            row => format!("{column} · row {row}"),
        }
    }

    /// How big it is: 1,204 rows · 12 columns; a table read in part says so.
    pub fn size(&self) -> String {
        let rows = u64::from(self.rows) - u64::from(self.named && self.rows > 0);
        let plural = |n: u64, one: &str, many: &str| {
            format!(
                "{} {}",
                thousands(&n.to_string()),
                if n == 1 { one } else { many }
            )
        };
        let rows = if self.unread > 0 {
            format!(
                "first {} of {}",
                thousands(&rows.to_string()),
                plural(rows + self.unread, "row", "rows")
            )
        } else {
            plural(rows, "row", "rows")
        };
        format!(
            "{rows} · {}",
            plural(u64::from(self.cols), "column", "columns")
        )
    }

    /// The line a row starts on, in the text it came from.
    pub fn line(&self, row: u32) -> Option<usize> {
        self.lines.get(row as usize).copied()
    }

    /// The row on line `line` of the text it came from.
    fn row_at(&self, line: usize) -> Option<u32> {
        if self.lines.is_empty() {
            return None;
        }
        let after = self.lines.partition_point(|&l| l <= line);
        Some(after.saturating_sub(1) as u32)
    }
}

/// Where a range's last non-empty cell's row and column end (formulas ranges can
/// carry empty strings).
fn non_empty_end(range: &Range<String>) -> Option<(u32, u32)> {
    let (top, left) = range.start()?;
    range.used_cells().filter(|(_, _, f)| !f.is_empty()).fold(
        None,
        |end: Option<(u32, u32)>, (row, col, _)| {
            let (row, col) = (top + row as u32, left + col as u32);
            Some(end.map_or((row, col), |(r, c)| (r.max(row), c.max(col))))
        },
    )
}

/// A string on one line, without control characters.
pub fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Text in lines of at most `width` columns, broken between words where it can
/// and inside a word too long for a line; its own line breaks are kept.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let word = one_line(word);
            let w = word.width();
            if used > 0 && used + 1 + w <= width {
                line.push(' ');
                line.push_str(&word);
                used += 1 + w;
                continue;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            // A word longer than the line is cut into pieces that fit.
            for c in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        lines.push(line);
    }
    while lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// A column's letters: 0 is A, 25 Z, 26 AA.
pub fn column_name(mut col: u32) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}

/// A cell's name: B7.
pub fn cell_name(row: u32, col: u32) -> String {
    format!("{}{}", column_name(col), row + 1)
}

/// CSV's separator: a tab in .tsv files, else whichever of , ; tab | the first
/// line has most of (Excel writes ; where the decimal point is a comma).
fn separator(path: &Path, text: &str) -> char {
    let tsv = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    if tsv {
        return '\t';
    }
    let mut counts = [(',', 0), (';', 0), ('\t', 0), ('|', 0)];
    let mut quoted = false;
    for c in text.chars() {
        if c == '\n' && !quoted {
            break;
        }
        if c == '"' {
            quoted = !quoted;
        } else if !quoted {
            if let Some(count) = counts.iter_mut().find(|(s, _)| *s == c) {
                count.1 += 1;
            }
        }
    }
    let (best, most) = counts.iter().fold(
        (',', 0),
        |best, &(c, n)| {
            if n > best.1 {
                (c, n)
            } else {
                best
            }
        },
    );
    if most == 0 {
        ','
    } else {
        best
    }
}

/// CSV text's rows (RFC 4180: "quoted" fields may hold the separator, line breaks
/// and "" for a quote), with the line each row starts on.
pub fn parse_table(text: &str, separator: char) -> Grid {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let (mut quoted, mut was_quoted, mut pending) = (false, false, false);
    let (mut line, mut start) = (0, 0);
    let finish = |field: &mut String, row: &mut Vec<Field>| {
        let text = std::mem::take(field);
        row.push(if looks_numeric(&text) {
            Field::Number(text)
        } else {
            Field::Text(text)
        });
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => {
                    if c == '\n' {
                        line += 1;
                    }
                    field.push(c);
                }
            }
            continue;
        }
        pending = true;
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            c if c == separator => {
                finish(&mut field, &mut row);
                was_quoted = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                finish(&mut field, &mut row);
                rows.push(std::mem::take(&mut row));
                lines.push(start);
                line += 1;
                start = line;
                was_quoted = false;
                pending = false;
            }
            c => field.push(c),
        }
    }
    if pending || quoted {
        finish(&mut field, &mut row);
        rows.push(row);
        lines.push(start);
    }
    Grid::table(rows, false, 0, lines)
}

/// Whether CSV text is a number, to line it up on the right: 42, -3.5, 1e6.
fn looks_numeric(text: &str) -> bool {
    let t = text.trim();
    t.chars().any(|c| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c))
        && t.parse::<f64>().is_ok()
}

impl SheetView {
    /// Start reading the workbook or database at `path`; `start` is the sheet,
    /// row and column to show.
    pub fn open(path: PathBuf, peeked: bool, start: Option<(usize, u32, u32)>) -> Self {
        let database = is_database(&path);
        let mut view = Self::empty(path, peeked, start);
        view.database = database;
        view.wrap = !database;
        view.load();
        view
    }

    /// A grid of a CSV or TSV buffer's rows, the row under its cursor selected.
    pub fn from_buffer(buffer: &Buffer, id: usize) -> Self {
        let path = buffer.path.clone().unwrap_or_default();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "CSV".into());
        let mut view = Self::empty(path, false, None);
        view.buffer = Some(id);
        view.sheets.push(Sheet::new(name, false, ""));
        view.follow(buffer);
        view
    }

    fn empty(path: PathBuf, peeked: bool, start: Option<(usize, u32, u32)>) -> Self {
        Self {
            path,
            sheets: Vec::new(),
            current: 0,
            buffer: None,
            database: false,
            peeked,
            error: None,
            typing: None,
            query: String::new(),
            wrap: false,
            reading: None,
            page: 20,
            drawn: Drawn::default(),
            stamp: None,
            loading: None,
            start,
            revision: None,
            synced: None,
        }
    }

    /// Read a CSV buffer's rows again once it has changed (at most a few times a
    /// second while you type), and select the row its cursor moved to.
    pub fn follow(&mut self, buffer: &Buffer) -> bool {
        let mut changed = false;
        let stale = self.revision.is_none_or(|(revision, when)| {
            revision != buffer.revision && when.elapsed() >= Duration::from_millis(150)
        });
        if stale {
            let text = buffer.text.to_string();
            let grid = parse_table(&text, separator(&self.path, &text));
            if let Some(sheet) = self.sheets.first_mut() {
                sheet.grid = Some(Ok(grid));
                sheet.clamp();
            }
            self.revision = Some((buffer.revision, Instant::now()));
            changed = true;
        }
        if self.synced != Some(buffer.row) {
            self.synced = Some(buffer.row);
            let row = self.grid().and_then(|g| g.row_at(buffer.row));
            if let (Some(row), Some(sheet)) = (row, self.sheets.first_mut()) {
                sheet.row = row;
                sheet.clamp();
                changed = true;
            }
        }
        changed
    }

    fn load(&mut self) {
        if self.buffer.is_some() {
            return;
        }
        self.error = None;
        self.stamp = stamp(&self.path, self.database);
        match std::fs::metadata(&self.path) {
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
            Ok(m) if m.len() > MAX_FILE && !self.database => {
                self.error = Some(format!(
                    "it's {} MiB; Raido shows workbooks up to {} MiB",
                    m.len() >> 20,
                    MAX_FILE >> 20
                ));
                return;
            }
            Ok(_) => {}
        }
        let (send, receive) = mpsc::channel();
        let path = self.path.clone();
        let first = self.start.map(|s| s.0);
        let database = self.database;
        thread::spawn(move || {
            if database {
                read_database(&path, first, &send)
            } else {
                read_workbook(&path, first, &send)
            }
        });
        self.loading = Some(receive);
    }

    /// Read the file again (it changed on disk), keeping each sheet's place.
    pub fn reload(&mut self) {
        if let Some(sheet) = self.sheets.get(self.current) {
            self.start = Some((self.current, sheet.row, sheet.col));
        }
        self.load();
    }

    /// Whether the file changed since it was read (a database's write-ahead log
    /// counts).
    pub fn changed_on_disk(&self) -> bool {
        let now = stamp(&self.path, self.database);
        self.buffer.is_none() && now.is_some() && now != self.stamp && self.loading.is_none()
    }

    /// Whether the file is still being read.
    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Take what the reader has read; true when there's something new to show.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(loading) = &self.loading {
            match loading.try_recv() {
                Ok(Loaded::Names(names, first)) => {
                    self.named(names, first);
                    changed = true;
                }
                Ok(Loaded::Sheet(index, grid)) => {
                    let grid = *grid;
                    if let Some(sheet) = self.sheets.get_mut(index) {
                        // A sheet opens on the cell it was saved on.
                        if let (Ok(grid), false) = (&grid, sheet.placed) {
                            if let Some((row, col)) = grid.active {
                                sheet.row = row;
                                sheet.col = col;
                            }
                        }
                        sheet.grid = Some(grid);
                        sheet.clamp();
                    }
                    changed = true;
                }
                Ok(Loaded::Failed(error)) => {
                    self.error = Some(error);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.loading = None,
            }
        }
        changed
    }

    /// The sheets' names are known: the new list, keeping the place (and, until
    /// the new cells come, the cells) of each sheet that was there already.
    fn named(&mut self, names: Vec<Tab>, first: usize) {
        let mut old = std::mem::take(&mut self.sheets);
        let shown = old.get(self.current).map(|s| s.name.clone());
        let reloaded = !old.is_empty();
        self.sheets = names
            .into_iter()
            .map(|tab| {
                let mut sheet = Sheet::new(tab.name, tab.hidden, tab.note);
                if let Some(before) = old.iter_mut().find(|s| s.name == sheet.name) {
                    sheet.grid = before.grid.take();
                    sheet.row = before.row;
                    sheet.col = before.col;
                    sheet.top = before.top;
                    sheet.left = before.left;
                    sheet.placed = true;
                }
                sheet
            })
            .collect();
        let last = self.sheets.len().saturating_sub(1);
        self.current = match shown.filter(|_| reloaded) {
            Some(name) => self.sheets.iter().position(|s| s.name == name).unwrap_or(0),
            None => match self.start {
                Some((sheet, row, col)) => {
                    let current = first_shown(&self.sheets, sheet.min(last));
                    if let Some(s) = self.sheets.get_mut(current) {
                        if current == sheet {
                            s.row = row;
                            s.col = col;
                            s.placed = true;
                        }
                    }
                    current
                }
                None => first.min(last),
            },
        };
    }

    pub fn sheet(&self) -> Option<&Sheet> {
        self.sheets.get(self.current)
    }

    fn sheet_mut(&mut self) -> Option<&mut Sheet> {
        self.sheets.get_mut(self.current)
    }

    /// The shown sheet's cells, once they're read.
    pub fn grid(&self) -> Option<&Grid> {
        self.sheet()?.grid.as_ref()?.as_ref().ok()
    }

    /// The selected cell.
    pub fn selected(&self) -> Option<(u32, u32)> {
        let sheet = self.sheet()?;
        Some((sheet.row, sheet.col))
    }

    /// Move the selection by rows and columns, staying on the sheet (columns
    /// hidden in Excel are skipped).
    pub fn move_by(&mut self, rows: i64, cols: i64) {
        let col = match (self.grid(), self.selected()) {
            (Some(grid), Some((_, col))) => grid.step_col(col, cols),
            _ => 0,
        };
        if let Some(sheet) = self.sheet_mut() {
            sheet.row = (i64::from(sheet.row) + rows).max(0) as u32;
            sheet.col = col;
            sheet.placed = true;
            sheet.clamp();
        }
    }

    /// Select a row or a column: `u32::MAX` is the last one, a column 0 the first
    /// one shown.
    pub fn go(&mut self, row: Option<u32>, col: Option<u32>) {
        let col = match (col, self.grid()) {
            (Some(u32::MAX), Some(grid)) => Some(grid.edge_col(true)),
            (Some(0), Some(grid)) => Some(grid.edge_col(false)),
            (col, _) => col,
        };
        if let Some(sheet) = self.sheet_mut() {
            if let Some(row) = row {
                sheet.row = row;
            }
            if let Some(col) = col {
                sheet.col = col;
            }
            sheet.placed = true;
            sheet.clamp();
        }
    }

    /// The wheel: the view moves and the selection stays on screen.
    pub fn scroll(&mut self, by: i64) {
        let page = self.page.max(1);
        if let Some(sheet) = self.sheet_mut() {
            let rows = sheet.extent().0;
            let last = rows.saturating_sub(1);
            sheet.top = ((i64::from(sheet.top) + by).max(0) as u32).min(last);
            sheet.row = sheet
                .row
                .clamp(sheet.top, (sheet.top + page - 1).min(last).max(sheet.top));
        }
    }

    /// Show the next sheet (or, `by` -1, the one before), round the end.
    pub fn switch(&mut self, by: isize) {
        let count = self.sheets.len();
        if count > 0 {
            self.current = (self.current as isize + by).rem_euclid(count as isize) as usize;
        }
    }

    /// Select the next cell (or, backwards, the one before) whose text has the
    /// query in it, round the end of the sheet. Case counts only when the query
    /// has a capital.
    pub fn find(&mut self, forwards: bool) -> bool {
        let query = self.query.trim().to_string();
        let Some(at) = self.selected().filter(|_| !query.is_empty()) else {
            return false;
        };
        let Some(grid) = self.grid() else {
            return false;
        };
        let exact = query.chars().any(char::is_uppercase);
        let query = if exact { query } else { query.to_lowercase() };
        let matches = |text: String| {
            if exact {
                text.contains(&query)
            } else {
                text.to_lowercase().contains(&query)
            }
        };
        let (mut first, mut before, mut after, mut last) = (None, None, None, None);
        for row in grid.first..grid.rows {
            for col in 0..grid.cols {
                let text = grid.shown(row, col).text;
                if text.is_empty() || !matches(text) {
                    continue;
                }
                let here = (row, col);
                first = first.or(Some(here));
                last = Some(here);
                if here < at {
                    before = Some(here);
                } else if here > at && after.is_none() {
                    after = Some(here);
                    if forwards {
                        break;
                    }
                }
            }
            if forwards && after.is_some() {
                break;
            }
        }
        let found = if forwards {
            after.or(first)
        } else {
            before.or(last)
        };
        match found {
            Some((row, col)) => {
                self.go(Some(row), Some(col));
                true
            }
            None => false,
        }
    }

    /// A click on the grid selects that cell; on a tab, that sheet.
    pub fn click(&mut self, x: u16, y: u16) {
        if self.drawn.tabs_y == Some(y) {
            if let Some(&(_, _, sheet)) = self
                .drawn
                .tabs
                .iter()
                .find(|(at, width, _)| (*at..at + width).contains(&x))
            {
                self.current = sheet;
            }
            return;
        }
        let row = self
            .drawn
            .rows
            .iter()
            .find(|&&(at, height, _)| (at..at + height).contains(&y))
            .map(|r| r.2);
        let col = self
            .drawn
            .columns
            .iter()
            .find(|(at, width, _)| (*at..at + width + 3).contains(&x))
            .map(|c| c.2);
        if let (Some(row), Some(col)) = (row, col) {
            self.go(Some(row), Some(col));
        }
    }

    /// The selected cell's name and its text for the clipboard.
    pub fn copied_cell(&self) -> Option<(String, String)> {
        let grid = self.grid()?;
        let (row, col) = self.selected()?;
        Some((grid.label(row, col), grid.copied(row, col)))
    }

    /// The selected cell's row as tab-separated text, as spreadsheets paste it.
    pub fn copied_row(&self) -> Option<String> {
        let grid = self.grid()?;
        let (row, _) = self.selected()?;
        let mut cells: Vec<String> = (0..grid.cols).map(|col| grid.copied(row, col)).collect();
        while cells.last().is_some_and(String::is_empty) {
            cells.pop();
        }
        Some(cells.join("\t"))
    }
}

impl Sheet {
    /// Rows and columns, once read.
    fn extent(&self) -> (u32, u32) {
        match &self.grid {
            Some(Ok(grid)) => (grid.rows, grid.cols),
            _ => (0, 0),
        }
    }

    fn clamp(&mut self) {
        let Some(Ok(grid)) = &self.grid else {
            return;
        };
        if grid.rows > 0 {
            self.row = self.row.min(grid.rows - 1);
            self.col = self.col.min(grid.cols.saturating_sub(1));
            if grid.is_hidden(self.col) {
                let right = grid.step_col(self.col, 1);
                self.col = if grid.is_hidden(right) {
                    grid.step_col(self.col, -1)
                } else {
                    right
                };
            }
        }
    }
}

/// The sheet to show: `wanted`, unless it's hidden and another one isn't.
fn first_shown(sheets: &[Sheet], wanted: usize) -> usize {
    if sheets.get(wanted).is_some_and(|s| !s.hidden) {
        return wanted;
    }
    sheets.iter().position(|s| !s.hidden).unwrap_or(wanted)
}

/// When a file was last written and how long it is; for a database, its
/// write-ahead log too (where its newest changes wait).
fn stamp(path: &Path, database: bool) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let (mut time, mut len) = (meta.modified().ok()?, meta.len());
    if database {
        let mut log = path.as_os_str().to_owned();
        log.push("-wal");
        if let Ok(meta) = std::fs::metadata(PathBuf::from(log)) {
            len += meta.len();
            time = time.max(meta.modified().unwrap_or(time));
        }
    }
    Some((time, len))
}

/// The sheet to read first: `wanted`, unless it's hidden and another one isn't.
fn first_tab(tabs: &[Tab], wanted: usize) -> usize {
    let wanted = wanted.min(tabs.len().saturating_sub(1));
    if tabs.get(wanted).is_some_and(|t| !t.hidden) {
        return wanted;
    }
    tabs.iter().position(|t| !t.hidden).unwrap_or(wanted)
}

/// Read the workbook: its sheets' names, then their cells, sheet `first` (else the
/// one it was saved on) first. Stops when nobody is waiting any more.
fn read_workbook(path: &Path, first: Option<usize>, send: &Sender<Loaded>) {
    let mut book = match open_workbook_auto(path) {
        Ok(book) => book,
        Err(error) => {
            let _ = send.send(Loaded::Failed(describe(&error.to_string())));
            return;
        }
    };
    let sheets: Vec<(String, bool, bool)> = book
        .sheets_metadata()
        .iter()
        .filter(|s| s.typ != SheetType::Vba)
        .map(|s| {
            (
                s.name.clone(),
                s.visible != SheetVisible::Visible,
                s.typ == SheetType::ChartSheet,
            )
        })
        .collect();
    let tabs: Vec<Tab> = sheets
        .iter()
        .map(|(name, hidden, _)| Tab {
            name: name.clone(),
            hidden: *hidden,
            note: if *hidden { "hidden" } else { "" },
        })
        .collect();
    let (saved_on, layouts) = excel_layouts(path);
    let first = first_tab(&tabs, first.or(saved_on).unwrap_or(0));
    if send.send(Loaded::Names(tabs, first)).is_err() {
        return;
    }
    let order = std::iter::once(first).chain((0..sheets.len()).filter(|&i| i != first));
    for index in order {
        let Some((name, _, chart)) = sheets.get(index) else {
            continue;
        };
        let grid = if *chart {
            Err("A chart sheet: Raido shows the cells of the other sheets".to_string())
        } else {
            match book.worksheet_range(name) {
                Ok(values) => {
                    let formulas = book.worksheet_formula(name).unwrap_or_default();
                    let styles = book.worksheet_style(name).unwrap_or_default();
                    let layout = layouts.iter().find(|(n, _)| n == name).map(|l| &l.1);
                    let merged = merges(&mut book, name);
                    Ok(Grid::new(values, formulas, styles).laid_out(layout, merged))
                }
                Err(error) => Err(format!("Can't read this sheet: {error}")),
            }
        };
        if send.send(Loaded::Sheet(index, Box::new(grid))).is_err() {
            return;
        }
    }
}

/// Read a SQLite database, read-only: its tables and views (as sheets, the column
/// names their first row), then their rows, table `first` first. Only a table's
/// first rows are read; the rest are counted.
fn read_database(path: &Path, first: Option<usize>, send: &Sender<Loaded>) {
    use rusqlite::{Connection, OpenFlags};
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let tables = Connection::open_with_flags(path, flags).and_then(|db| {
        let tables = db
            .prepare(
                "SELECT name, type = 'view' FROM sqlite_schema \
                 WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                 ORDER BY type = 'view', name",
            )?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((db, tables))
    });
    let (db, tables) = match tables {
        Ok(found) => found,
        Err(error) => {
            let _ = send.send(Loaded::Failed(describe(&error.to_string())));
            return;
        }
    };
    let tabs: Vec<Tab> = tables
        .iter()
        .map(|(name, view)| Tab {
            name: name.clone(),
            hidden: false,
            note: if *view { "view" } else { "" },
        })
        .collect();
    let first = first_tab(&tabs, first.unwrap_or(0));
    if send.send(Loaded::Names(tabs, first)).is_err() {
        return;
    }
    let order = std::iter::once(first).chain((0..tables.len()).filter(|&i| i != first));
    for index in order {
        let Some((name, _)) = tables.get(index) else {
            continue;
        };
        let grid = read_table(&db, name).map_err(|e| format!("Can't read this table: {e}"));
        if send.send(Loaded::Sheet(index, Box::new(grid))).is_err() {
            return;
        }
    }
}

/// A table's column names, then its first rows.
fn read_table(db: &rusqlite::Connection, name: &str) -> rusqlite::Result<Grid> {
    use rusqlite::types::ValueRef;
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let total: i64 = db.query_row(&format!("SELECT count(*) FROM {quoted}"), [], |r| r.get(0))?;
    let mut statement = db.prepare(&format!("SELECT * FROM {quoted} LIMIT {MAX_ROWS}"))?;
    let names: Vec<Field> = statement
        .column_names()
        .map(|n| Field::Text(n.to_string()))
        .collect();
    let columns = names.len();
    let mut rows = vec![names];
    let mut query = statement.query([])?;
    while let Some(row) = query.next()? {
        rows.push(
            (0..columns)
                .map(|i| match row.get_ref(i) {
                    Ok(ValueRef::Integer(n)) => Field::Number(n.to_string()),
                    Ok(ValueRef::Real(n)) => Field::Number(format!("{n:?}")),
                    Ok(ValueRef::Text(t)) => Field::Text(String::from_utf8_lossy(t).into_owned()),
                    Ok(ValueRef::Blob(b)) => Field::Blob(b.len()),
                    Ok(ValueRef::Null) | Err(_) => Field::Null,
                })
                .collect(),
        );
    }
    let read = rows.len() as u64 - 1;
    Ok(Grid::table(
        rows,
        true,
        (total.max(0) as u64).saturating_sub(read),
        Vec::new(),
    ))
}

/// A workbook's merged cells: (first row, first column, last row, last column).
fn merges(
    book: &mut calamine::Sheets<std::io::BufReader<std::fs::File>>,
    name: &str,
) -> Vec<(u32, u32, u32, u32)> {
    let found = match book {
        calamine::Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(name).ok(),
        calamine::Sheets::Xls(x) => x.merge_cells_by_sheet_name(name).ok(),
        _ => None,
    };
    found
        .unwrap_or_default()
        .into_iter()
        .map(|d| (d.start.0, d.start.1, d.end.0, d.end.1))
        .collect()
}

/// How a sheet was laid out in Excel, which the cell reader leaves out.
#[derive(Default, Debug, PartialEq)]
struct Layout {
    /// (first column, last column, width in characters, hidden), from 0.
    columns: Vec<(u32, u32, f64, bool)>,
    /// Frozen panes: the columns and rows kept on screen.
    frozen: (u32, u32),
    /// The selected cell.
    active: Option<(u32, u32)>,
}

/// The sheet a workbook was saved on, and each sheet's layout by name.
type Layouts = (Option<usize>, Vec<(String, Layout)>);

/// An .xlsx (or .xlsm) workbook's layout: each sheet's (by name) column widths,
/// frozen panes and selected cell, and the sheet it was saved on. Nothing for the
/// other formats, or a file it can't make out.
fn excel_layouts(path: &Path) -> Layouts {
    let read = || -> Option<Layouts> {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
        let workbook = entry(&mut zip, "xl/workbook.xml", None)?;
        let rels = entry(&mut zip, "xl/_rels/workbook.xml.rels", None).unwrap_or_default();
        let saved_on = tags(&workbook, "workbookView")
            .next()
            .and_then(|t| attr(t, "activeTab"))
            .and_then(|a| a.parse().ok());
        let mut layouts = Vec::new();
        for tag in tags(&workbook, "sheet") {
            let (Some(name), Some(id)) = (attr(tag, "name"), attr(tag, "id")) else {
                continue;
            };
            let target = tags(&rels, "Relationship")
                .find(|r| attr(r, "Id").as_deref() == Some(id.as_str()))
                .and_then(|r| attr(r, "Target"));
            let Some(target) = target else {
                continue;
            };
            let part = match target.strip_prefix('/') {
                Some(absolute) => absolute.to_string(),
                None => format!("xl/{target}"),
            };
            // What comes before the cells is all it needs.
            let xml = entry(&mut zip, &part, Some("<sheetData")).unwrap_or_default();
            layouts.push((name, layout(&xml)));
        }
        Some((saved_on, layouts))
    };
    read().unwrap_or_default()
}

/// A file in the workbook's zip as text, up to `stop` when given.
fn entry(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    stop: Option<&str>,
) -> Option<String> {
    let mut file = zip.by_name(name).ok()?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read]);
        let found = stop.is_some_and(|stop| {
            let from = out.len().saturating_sub(read + stop.len());
            out[from..]
                .windows(stop.len())
                .any(|w| w == stop.as_bytes())
        });
        if found || out.len() > 16 << 20 {
            break;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// A sheet's layout from the start of its XML: `<cols>`, and the first view's
/// frozen pane and selection.
fn layout(xml: &str) -> Layout {
    let mut layout = Layout::default();
    for col in tags(xml, "col") {
        let number = |name| attr(col, name).and_then(|v| v.parse::<u32>().ok());
        let (Some(first), Some(last)) = (number("min"), number("max")) else {
            continue;
        };
        if first == 0 || last < first {
            continue;
        }
        let hidden = attr(col, "hidden").is_some_and(|h| h == "1" || h == "true");
        let width = attr(col, "width").and_then(|w| w.parse::<f64>().ok());
        if width.is_some() || hidden {
            layout
                .columns
                .push((first - 1, last - 1, width.unwrap_or(0.0), hidden));
        }
    }
    let view_end = xml.find("</sheetView>").unwrap_or(xml.len());
    let view = &xml[..view_end];
    let pane = tags(view, "pane").next();
    let active_pane = pane.and_then(|p| attr(p, "activePane"));
    if let Some(pane) = pane {
        let frozen = attr(pane, "state").is_some_and(|s| s.starts_with("frozen"));
        let split = |name| {
            attr(pane, name)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0) as u32
        };
        if frozen {
            layout.frozen = (split("xSplit"), split("ySplit"));
        }
    }
    let selections: Vec<&str> = tags(view, "selection").collect();
    let selection = selections
        .iter()
        .find(|s| attr(s, "pane") == active_pane)
        .or(selections.first());
    layout.active = selection
        .and_then(|s| attr(s, "activeCell"))
        .and_then(|cell| cell_at(&cell));
    layout
}

/// B7 (or $B$7) as (6, 1).
fn cell_at(name: &str) -> Option<(u32, u32)> {
    let name = name.replace('$', "");
    let split = name.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = name.split_at(split);
    if letters.is_empty() {
        return None;
    }
    let mut col: u32 = 0;
    for c in letters.chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    let row: u32 = digits.parse().ok()?;
    Some((row.checked_sub(1)?, col - 1))
}

/// The opening tags called `name` (any namespace prefix), each without its < >.
fn tags<'a>(xml: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    xml.match_indices('<').filter_map(move |(i, _)| {
        let rest = &xml[i + 1..];
        let tag = &rest[..rest.find('>')?];
        let qualified = tag.split(|c: char| c.is_whitespace() || c == '/').next()?;
        let local = qualified.rsplit(':').next()?;
        (local == name && !qualified.is_empty()).then_some(tag)
    })
}

/// An attribute's value in a tag, by its name (any namespace prefix).
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq]
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or("");
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let end = after[1..].find(quote)?;
        let value = &after[1..1 + end];
        if key == name || key.rsplit(':').next() == Some(name) {
            return Some(unescape(value));
        }
        rest = &after[end + 2..];
    }
    None
}

/// XML's &amp; &lt; &gt; &quot; &apos; and &#…; as the characters.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(end) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A reader's error in a few plain words.
fn describe(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("password") || lower.contains("encrypt") {
        "it's password-protected".into()
    } else {
        error.to_string()
    }
}

// ---------------------------------------------------------------------------
// Values as the cell's format shows them.

/// A value as text: numbers in their format (`code`, Excel's number format, when
/// the file has one), dates as 2024-03-15.
pub fn display(value: &Data, code: Option<&str>) -> String {
    match value {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Bool(true) => "TRUE".into(),
        Data::Bool(false) => "FALSE".into(),
        Data::Error(e) => e.to_string(),
        Data::Int(n) => number(*n as f64, code),
        Data::Float(n) => number(*n, code),
        Data::DateTime(dt) => date(dt, code),
        Data::DateTimeIso(s) => iso_date(s),
        Data::DurationIso(s) => iso_duration(s).unwrap_or_else(|| s.clone()),
    }
}

/// A number in Excel format `code`, or as General shows it.
fn number(n: f64, code: Option<&str>) -> String {
    if !n.is_finite() {
        return n.to_string();
    }
    code.filter(|c| !c.eq_ignore_ascii_case("general") && !c.is_empty())
        .and_then(|c| formatted(n, c))
        .unwrap_or_else(|| general(n))
}

/// The digits of `n` (not negative), rounded half away from zero to `decimals`
/// places from its first 15 significant digits, as Excel keeps them: 2.675 to two
/// places is 2.68. The whole part has no leading zeros ("" for zero).
fn fixed(n: f64, decimals: usize) -> (String, String) {
    let text = format!("{:.14e}", n.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mut digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    let mut point = exponent + 1;
    if point < 0 {
        let zeros = (-point) as usize;
        digits.splice(0..0, std::iter::repeat_n(0, zeros));
        point = 0;
    }
    let mut point = point as usize;
    let keep = point + decimals;
    if digits.len() > keep {
        let up = digits[keep] >= 5;
        digits.truncate(keep);
        if up {
            let mut i = keep;
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    point += 1;
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
    } else {
        digits.resize(keep, 0);
    }
    let whole: String = digits[..point]
        .iter()
        .map(|d| char::from(b'0' + d))
        .collect::<String>()
        .trim_start_matches('0')
        .to_string();
    let fraction = digits[point..]
        .iter()
        .map(|d| char::from(b'0' + d))
        .collect();
    (whole, fraction)
}

/// `n` (not negative) as d.ddd and its power of ten, the mantissa rounded to
/// `decimals` places.
fn scientific_parts(n: f64, decimals: usize) -> (String, String, i32) {
    if n == 0.0 {
        return ("0".into(), "0".repeat(decimals), 0);
    }
    let text = format!("{:.14e}", n.abs());
    let exponent: i32 = text
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    let scaled = n.abs() / 10f64.powi(exponent);
    let (whole, fraction) = fixed(scaled, decimals);
    // 9.99 rounded up to 10.0: one more power of ten.
    if whole.len() > 1 {
        let (whole, fraction) = fixed(scaled / 10.0, decimals);
        return (whole, fraction, exponent + 1);
    }
    (whole, fraction, exponent)
}

/// As Excel's General format shows a number: as many digits as fit in 11
/// characters, else in scientific form.
pub fn general(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let sign = if n < 0.0 { "-" } else { "" };
    let a = n.abs();
    let (whole, _) = fixed(a, 0);
    let small = a < 1e-4;
    if whole.len() > 11 || small {
        let fits = fixed(a, 10);
        let plain = trim_fraction(&fits.0, &fits.1);
        // Tiny numbers stay plain while that fits and loses nothing (0.00001).
        if small && plain != "0" && full(a) == plain && plain.len() <= 11 {
            return format!("{sign}{plain}");
        }
        let (whole, fraction, exponent) = scientific_parts(a, 5);
        let fraction = fraction.trim_end_matches('0');
        let point = if fraction.is_empty() { "" } else { "." };
        let e_sign = if exponent < 0 { '-' } else { '+' };
        return format!(
            "{sign}{whole}{point}{fraction}E{e_sign}{:02}",
            exponent.abs()
        );
    }
    let decimals = 10usize.saturating_sub(whole.len().max(1));
    let (whole, fraction) = fixed(a, decimals);
    format!("{sign}{}", trim_fraction(&whole, &fraction))
}

/// The number in full: its 15 significant digits.
fn full(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let sign = if n < 0.0 { "-" } else { "" };
    let a = n.abs();
    let exponent: i32 = format!("{a:e}")
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    if !(-10..16).contains(&exponent) {
        let (whole, fraction, exponent) = scientific_parts(a, 14);
        let fraction = fraction.trim_end_matches('0');
        let point = if fraction.is_empty() { "" } else { "." };
        return format!("{sign}{whole}{point}{fraction}E{exponent:+03}");
    }
    let decimals = (14 - exponent).max(0) as usize;
    let (whole, fraction) = fixed(a, decimals);
    format!("{sign}{}", trim_fraction(&whole, &fraction))
}

/// "12" and "500" as 12.5; "" and "" as 0.
fn trim_fraction(whole: &str, fraction: &str) -> String {
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// Groups of three: 1234567 as 1,234,567.
pub fn thousands(whole: &str) -> String {
    let mut out = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The sections of a format code: positive; negative; zero; text.
fn sections(code: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut bracket, mut escaped) = (0, false, false, false);
    for (i, c) in code.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if !quoted => escaped = true,
            '"' => quoted = !quoted,
            '[' if !quoted => bracket = true,
            ']' if !quoted => bracket = false,
            ';' if !quoted && !bracket => {
                parts.push(&code[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&code[start..]);
    parts
}

/// A piece of a number format's section.
enum Token {
    Text(String),
    /// A digit placeholder (0 # ?), the point, a comma, or E and its sign.
    Digit(char),
    General,
}

/// A number in one of Excel's number formats: decimals, thousands separators,
/// percent, scientific, text and currency around the number, and sections for
/// negatives and zero. `None` for what it doesn't cover (fractions, conditions):
/// the number then shows as General does.
fn formatted(n: f64, code: &str) -> Option<String> {
    let parts = sections(code);
    let (section, value, signed) = if n < 0.0 && parts.len() >= 2 {
        (parts[1], -n, false)
    } else if n == 0.0 && parts.len() >= 3 {
        (parts[2], n, false)
    } else {
        (parts[0], n, true)
    };
    let mut tokens = Vec::new();
    let mut percent = 0;
    let mut chars = section.chars().peekable();
    while let Some(c) = chars.next() {
        let token = match c {
            '"' => Token::Text(chars.by_ref().take_while(|&q| q != '"').collect()),
            '\\' => Token::Text(chars.next().map(String::from).unwrap_or_default()),
            // Room for a character (lining numbers up), and a fill: a space, nothing.
            '_' => {
                chars.next();
                Token::Text(" ".into())
            }
            '*' => {
                chars.next();
                continue;
            }
            '[' => {
                let inside: String = chars.by_ref().take_while(|&b| b != ']').collect();
                if let Some(currency) = inside.strip_prefix('$') {
                    Token::Text(currency.split('-').next().unwrap_or("").to_string())
                } else if inside.starts_with(['<', '>', '='])
                    || inside.to_ascii_lowercase().starts_with(['h', 'm', 's'])
                {
                    return None;
                } else {
                    // A color ([Red], [Color10]) changes nothing here.
                    continue;
                }
            }
            '0' | '#' | '?' | '.' | ',' => Token::Digit(c),
            'E' | 'e' if matches!(chars.peek(), Some('+' | '-')) => {
                tokens.push(Token::Digit('E'));
                Token::Digit(chars.next().unwrap_or('+'))
            }
            '%' => {
                percent += 1;
                Token::Text("%".into())
            }
            'G' | 'g' => {
                let rest: String = chars.clone().take(6).collect();
                if !rest.eq_ignore_ascii_case("eneral") {
                    return None;
                }
                chars.nth(5);
                Token::General
            }
            // Fractions, text (@), and letters Excel would want quoted.
            '/' | '@' => return None,
            c if c.is_alphabetic() => return None,
            c => Token::Text(c.to_string()),
        };
        tokens.push(token);
    }
    let number = |t: &Token| !matches!(t, Token::Text(_));
    let texts = |tokens: &[Token]| -> String {
        tokens
            .iter()
            .filter_map(|t| match t {
                Token::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    };
    let (Some(start), Some(end)) = (
        tokens.iter().position(number),
        tokens.iter().rposition(number),
    ) else {
        // Text only: what the section says (`"-"` for zero).
        return Some(texts(&tokens));
    };
    let before = texts(&tokens[..start]);
    let after = texts(&tokens[end + 1..]);
    let middle = &tokens[start..=end];
    let sign = if signed && n < 0.0 { "-" } else { "" };
    let value = value.abs() * 100f64.powi(percent);
    if middle.iter().any(|t| matches!(t, Token::General)) {
        if middle.len() > 1 {
            return None;
        }
        return Some(format!("{sign}{before}{}{after}", general(value)));
    }
    let mut pattern = String::new();
    for token in middle {
        match token {
            Token::Digit(c) => pattern.push(*c),
            // Text among the digits (000-0000): not covered.
            _ => return None,
        }
    }
    let (mantissa, exponent) = match pattern.split_once('E') {
        Some((m, e)) => (m, Some(e)),
        None => (pattern.as_str(), None),
    };
    // Commas right after the last digit scale by a thousand each.
    let scale = mantissa.len() - mantissa.trim_end_matches(',').len();
    let mantissa = mantissa.trim_end_matches(',');
    let value = value / 1000f64.powi(scale as i32);
    let (whole_part, fraction_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let count = |part: &str, of: &[char]| part.chars().filter(|c| of.contains(c)).count();
    let grouped = whole_part.contains(',');
    let zeros = count(whole_part, &['0']);
    let spaces = count(whole_part, &['?']);
    let most = count(fraction_part, &['0', '#', '?']);
    let least = count(fraction_part, &['0', '?']);
    let (whole, mut fraction, power) = match exponent {
        Some(_) => {
            let (w, f, p) = scientific_parts(value, most);
            (w, f, Some(p))
        }
        None => {
            let (w, f) = fixed(value, most);
            (w, f, None)
        }
    };
    while fraction.len() > least && fraction.ends_with('0') {
        fraction.pop();
    }
    let mut whole = format!("{}{whole}", "0".repeat(zeros.saturating_sub(whole.len())));
    if grouped {
        whole = thousands(&whole);
    }
    let mut body = format!(
        "{}{whole}",
        " ".repeat((zeros + spaces).saturating_sub(whole.len()))
    );
    if !fraction.is_empty() {
        body.push('.');
        body.push_str(&fraction);
    }
    if body.trim().is_empty() {
        body = "0".into();
    }
    // Rounded to nothing, a negative shows no sign.
    let nothing = body.chars().all(|c| matches!(c, '0' | '.' | ',' | ' '));
    let sign = if nothing && power.is_none() { "" } else { sign };
    if let (Some(e), Some(power)) = (exponent, power) {
        let digits = count(e, &['0', '#']);
        let e_sign = match (power < 0, e.starts_with('+')) {
            (true, _) => "-",
            (false, true) => "+",
            (false, false) => "",
        };
        body = format!("{body}E{e_sign}{:0digits$}", power.abs());
    }
    Some(format!("{sign}{before}{body}{after}"))
}

/// A date, a time or both, as the format asks (ISO order: 2024-03-15 14:30); a
/// duration as hours and minutes (37:30:00).
fn date(dt: &ExcelDateTime, code: Option<&str>) -> String {
    let value = dt.as_f64();
    let code = code.map(date_letters).unwrap_or_default();
    let seconds_wanted = code.contains('s');
    if dt.is_duration() {
        let total = (value.abs() * 86_400.0).round() as u64;
        let sign = if value < 0.0 { "-" } else { "" };
        let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
        return if seconds_wanted || code.is_empty() {
            format!("{sign}{h}:{m:02}:{s:02}")
        } else {
            format!("{sign}{h}:{m:02}")
        };
    }
    let (year, month, day, ..) = dt.to_ymd_hms_milli();
    // The time to the nearest second, as Excel shows it (never past midnight).
    let seconds = (value.fract().abs() * 86_400.0).round().min(86_399.0) as u32;
    let (hour, minute, second) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let has_date = code.contains(['y', 'd']) || (code.contains('m') && !code.contains(['h', 's']));
    let has_time = code.contains(['h', 's']);
    let (has_date, has_time) = if code.is_empty() || (!has_date && !has_time) {
        (value.abs() >= 1.0, value.fract() != 0.0)
    } else {
        (has_date, has_time)
    };
    let day_part = format!("{year:04}-{month:02}-{day:02}");
    let time_part = if seconds_wanted || (code.is_empty() && second != 0) {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{hour:02}:{minute:02}")
    };
    match (has_date, has_time) {
        (true, true) => format!("{day_part} {time_part}"),
        (false, true) => time_part,
        _ => day_part,
    }
}

/// A format code's letters that say what it shows, lowercase, without quoted
/// text, colors or currency.
fn date_letters(code: &str) -> String {
    let mut out = String::new();
    let (mut quoted, mut bracket) = (false, false);
    for c in code.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => bracket = true,
            ']' if !quoted => bracket = false,
            c if !quoted && !bracket && c.is_ascii_alphabetic() => out.push(c.to_ascii_lowercase()),
            _ => {}
        }
    }
    out
}

/// OpenDocument's 2024-03-15T14:30:00 as 2024-03-15 14:30:00 (a date alone at
/// midnight).
fn iso_date(s: &str) -> String {
    match s.split_once('T') {
        Some((day, "00:00:00")) => day.to_string(),
        Some((day, time)) => format!("{day} {time}"),
        None => s.to_string(),
    }
}

/// OpenDocument's PT37H30M00S as 37:30:00.
fn iso_duration(s: &str) -> Option<String> {
    let rest = s.strip_prefix('P')?;
    let (days, time) = rest.split_once('T').unwrap_or((rest, ""));
    let days: f64 = match days.strip_suffix('D') {
        Some(d) => d.parse().ok()?,
        None if days.is_empty() => 0.0,
        None => return None,
    };
    let mut seconds = days * 86_400.0;
    let mut number = String::new();
    for c in time.chars() {
        match c {
            'H' | 'M' | 'S' => {
                let n: f64 = number.parse().ok()?;
                seconds += n * match c {
                    'H' => 3600.0,
                    'M' => 60.0,
                    _ => 1.0,
                };
                number.clear();
            }
            c => number.push(c),
        }
    }
    let total = seconds.round() as u64;
    Some(format!(
        "{}:{:02}:{:02}",
        total / 3600,
        total / 60 % 60,
        total % 60
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn numbers_show_as_their_format_asks() {
        let f = |n: f64, code: &str| number(n, Some(code));
        assert_eq!(f(1234.5, "#,##0.00"), "1,234.50");
        assert_eq!(f(-1234.5, "#,##0.00"), "-1,234.50");
        assert_eq!(f(0.155, "0%"), "16%");
        assert_eq!(f(0.1234, "0.0%"), "12.3%");
        assert_eq!(f(2.675, "0.00"), "2.68");
        assert_eq!(f(9.99, "$#,##0.00"), "$9.99");
        assert_eq!(f(1234.5, "[$€-407] #,##0.00"), "€ 1,234.50");
        assert_eq!(f(1234.5, "#,##0.00 [$€-407]"), "1,234.50 €");
        assert_eq!(f(1234.5, "#,##0.00 \"kr\""), "1,234.50 kr");
        assert_eq!(f(-5.0, "0.00;[Red](0.00)"), "(5.00)");
        assert_eq!(f(0.0, "0.00;(0.00);\"-\""), "-");
        assert_eq!(f(12345.0, "0.00E+00"), "1.23E+04");
        assert_eq!(f(0.000123, "0.0E+00"), "1.2E-04");
        assert_eq!(f(7.0, "000"), "007");
        assert_eq!(f(0.5, "#.00"), ".50");
        assert_eq!(f(1.5, "0.##"), "1.5");
        assert_eq!(f(2.0, "0.##"), "2");
        assert_eq!(f(1_234_567.0, "#,##0,\"K\""), "1,235K");
        assert_eq!(f(1234.5, "General"), "1234.5");
        assert_eq!(f(-0.001, "0.00"), "0.00");
        // Accounting: the padding Excel lines numbers up with.
        assert_eq!(
            f(
                1234.5,
                "_($* #,##0.00_);_($* (#,##0.00);_($* \"-\"??_);_(@_)"
            ),
            " $1,234.50 "
        );
        // What it doesn't cover shows as General: fractions, conditions.
        assert_eq!(f(0.5, "# ?/?"), "0.5");
        assert_eq!(f(150.0, "[>100]0;0.00"), "150");
    }

    #[test]
    fn general_fits_eleven_characters_and_keeps_fifteen_digits() {
        assert_eq!(general(0.1 + 0.2), "0.3");
        assert_eq!(general(1.0 / 3.0), "0.333333333");
        assert_eq!(general(1234567.891234), "1234567.891");
        assert_eq!(general(42.0), "42");
        assert_eq!(general(-7.25), "-7.25");
        assert_eq!(general(123456789012345.0), "1.23457E+14");
        assert_eq!(general(0.00001), "0.00001");
        assert_eq!(general(0.0000123456789), "1.23457E-05");
        assert_eq!(general(0.0000123456), "1.23456E-05");
        assert_eq!(full(1.0 / 3.0), "0.333333333333333");
        assert_eq!(full(0.1 + 0.2), "0.3");
        assert_eq!(full(123456789012345.0), "123456789012345");
    }

    #[test]
    fn dates_times_and_durations() {
        use calamine::ExcelDateTimeType::{DateTime, TimeDelta};
        let at = |v: f64| ExcelDateTime::new(v, DateTime, false);
        assert_eq!(date(&at(45366.0), Some("m/d/yyyy")), "2024-03-15");
        assert_eq!(
            date(&at(45366.6042), Some("yyyy-mm-dd h:mm")),
            "2024-03-15 14:30"
        );
        assert_eq!(date(&at(0.6042), Some("h:mm:ss")), "14:30:03");
        assert_eq!(date(&at(45366.0), Some("mmm yyyy")), "2024-03-15");
        assert_eq!(date(&at(45366.5), None), "2024-03-15 12:00");
        let long = ExcelDateTime::new(37.5 / 24.0, TimeDelta, false);
        assert_eq!(date(&long, Some("[h]:mm:ss")), "37:30:00");
        assert_eq!(iso_date("2024-03-15T00:00:00"), "2024-03-15");
        assert_eq!(iso_date("2024-03-15T14:30:00"), "2024-03-15 14:30:00");
        assert_eq!(iso_duration("PT37H30M00S").as_deref(), Some("37:30:00"));
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
        assert_eq!(cell_name(6, 1), "B7");
    }

    /// A cell for `workbook`: (reference, value, style, formula).
    pub(crate) type Cell<'a> = (&'a str, &'a str, u8, &'a str);

    /// A small .xlsx written by hand: each sheet's cells as (reference, value,
    /// style, formula), where a value starting with `'` is text; style 1 is
    /// 0.0%, 2 is #,##0.00, 3 a date, 4 bold and 5 wrapped.
    pub(crate) fn workbook(path: &Path, sheets: &[(&str, bool, &[Cell])]) {
        laid_out_workbook(path, sheets, &[], None);
    }

    /// `workbook`, with XML before each sheet's cells (`<cols>`, `<sheetViews>`)
    /// and the sheet it was saved on.
    pub(crate) fn laid_out_workbook(
        path: &Path,
        sheets: &[(&str, bool, &[Cell])],
        heads: &[&str],
        saved_on: Option<usize>,
    ) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut put = |name: &str, body: String| {
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        let mut types = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#,
        );
        let mut book = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
        );
        let mut rels = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdS" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
        );
        for (i, (name, hidden, cells)) in sheets.iter().enumerate() {
            let n = i + 1;
            types.push_str(&format!(r#"<Override PartName="/xl/worksheets/sheet{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#));
            let state = if *hidden { r#" state="hidden""# } else { "" };
            book.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{n}"{state} r:id="rId{n}"/>"#
            ));
            rels.push_str(&format!(r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#));
            let mut rows: std::collections::BTreeMap<u32, String> = Default::default();
            for (reference, value, style, formula) in cells.iter() {
                let digits = reference.trim_start_matches(|c: char| c.is_ascii_alphabetic());
                let row: u32 = digits.parse().unwrap();
                let f = if formula.is_empty() {
                    String::new()
                } else {
                    format!("<f>{formula}</f>")
                };
                let cell = match value.strip_prefix('\'') {
                    Some(text) => format!(
                        r#"<c r="{reference}" s="{style}" t="inlineStr">{f}<is><t>{text}</t></is></c>"#
                    ),
                    None if value.is_empty() => {
                        format!(r#"<c r="{reference}" s="{style}">{f}</c>"#)
                    }
                    None => format!(r#"<c r="{reference}" s="{style}">{f}<v>{value}</v></c>"#),
                };
                rows.entry(row).or_default().push_str(&cell);
            }
            let data: String = rows
                .iter()
                .map(|(row, cells)| format!(r#"<row r="{row}">{cells}</row>"#))
                .collect();
            put(
                &format!("xl/worksheets/sheet{n}.xml"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{}<sheetData>{data}</sheetData></worksheet>"#,
                    heads.get(i).copied().unwrap_or("")
                ),
            );
        }
        types.push_str("</Types>");
        book.push_str("</sheets></workbook>");
        if let Some(tab) = saved_on {
            book = book.replacen(
                "<sheets>",
                &format!(r#"<bookViews><workbookView activeTab="{tab}"/></bookViews><sheets>"#),
                1,
            );
        }
        rels.push_str("</Relationships>");
        put("[Content_Types].xml", types);
        put("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.into());
        put("xl/workbook.xml", book);
        put("xl/_rels/workbook.xml.rels", rels);
        put("xl/styles.xml", r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="0.0%"/></numFmts><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0"/></cellStyleXfs><cellXfs count="6"><xf numFmtId="0" fontId="0" xfId="0"/><xf numFmtId="164" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="4" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="14" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="0" fontId="1" xfId="0" applyFont="1"/><xf numFmtId="0" fontId="0" xfId="0" applyAlignment="1"><alignment vertical="top" wrapText="1"/></xf></cellXfs></styleSheet>"#.into());
        zip.finish().unwrap();
    }

    /// Read a view to the end.
    pub(crate) fn settle(view: &mut SheetView) {
        for _ in 0..500 {
            view.poll();
            if !view.loading() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the workbook was never read");
    }

    #[test]
    fn a_workbook_is_read_with_its_formats_formulas_and_sheets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.xlsx");
        workbook(
            &path,
            &[
                (
                    "Costs",
                    false,
                    &[
                        ("A1", "'Item", 4, ""),
                        ("B1", "'Cost", 4, ""),
                        ("C1", "'Share", 4, ""),
                        ("D1", "'Due", 4, ""),
                        ("A2", "'Rent", 0, ""),
                        ("B2", "1250.5", 2, ""),
                        ("C2", "0.625", 1, "B2/B4"),
                        ("D2", "45366", 3, ""),
                        ("A3", "'Food", 0, ""),
                        ("B3", "750", 2, ""),
                        ("A4", "'Total", 4, ""),
                        ("B4", "2000.5", 2, "SUM(B2:B3)"),
                        ("C4", "", 0, "B4*2"),
                    ],
                ),
                ("Notes", false, &[("B3", "'later", 0, "")]),
                ("Secret", true, &[("A1", "'x", 0, "")]),
            ],
        );
        let mut view = SheetView::open(path, false, None);
        settle(&mut view);
        assert_eq!(view.error, None);
        let names: Vec<(&str, bool)> = view
            .sheets
            .iter()
            .map(|s| (s.name.as_str(), s.hidden))
            .collect();
        assert_eq!(
            names,
            [("Costs", false), ("Notes", false), ("Secret", true)]
        );
        let grid = view.grid().unwrap();
        assert_eq!((grid.rows, grid.cols, grid.first), (4, 4, 0));
        let shown = |row, col| grid.shown(row, col);
        assert_eq!(shown(0, 0).text, "Item");
        assert!(shown(0, 0).bold);
        assert_eq!(shown(1, 1).text, "1,250.50");
        assert_eq!(shown(1, 1).align, Align::Right);
        assert_eq!(shown(1, 2).text, "62.5%");
        assert_eq!(shown(1, 3).text, "2024-03-15");
        assert_eq!(grid.formula(3, 1), Some("SUM(B2:B3)"));
        assert_eq!(grid.exact(1, 2).as_deref(), Some("0.625"));
        // A formula never calculated shows itself, dimmed.
        assert_eq!(shown(3, 2).text, "=B4*2");
        assert_eq!(shown(3, 2).kind, Kind::Formula);
        assert_eq!(grid.widths[1], "1,250.50".len() as u16);
        // Moving stays on the sheet; search goes round it.
        view.move_by(10, 10);
        assert_eq!(view.selected(), Some((3, 3)));
        view.query = "food".into();
        assert!(view.find(true));
        assert_eq!(view.selected(), Some((2, 0)));
        view.query = "%".into();
        assert!(view.find(false));
        assert_eq!(view.selected(), Some((1, 2)));
        assert_eq!(
            view.copied_row().as_deref(),
            Some("Rent\t1,250.50\t62.5%\t2024-03-15")
        );
        // The next sheet keeps its own place.
        view.switch(1);
        assert_eq!(view.sheet().unwrap().name, "Notes");
        assert_eq!(view.grid().unwrap().first, 2);
        view.switch(-1);
        assert_eq!(view.selected(), Some((1, 2)));
    }

    #[test]
    fn a_broken_or_missing_workbook_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.xlsx");
        std::fs::write(&path, "not a zip").unwrap();
        let mut view = SheetView::open(path, false, None);
        settle(&mut view);
        assert!(view.error.is_some());
        let view = SheetView::open(dir.path().join("gone.xlsx"), false, None);
        assert!(view.error.is_some());
        assert!(is_spreadsheet(Path::new("a/B.XLSX")));
        assert!(is_spreadsheet(Path::new("b.ods")));
        assert!(!is_spreadsheet(Path::new("c.csv")));
    }

    #[test]
    fn excel_layouts_are_read_from_the_sheet_xml() {
        let xml = r#"<worksheet><sheetViews><sheetView workbookViewId="0"><pane xSplit="2" ySplit="1" topLeftCell="C2" activePane="bottomRight" state="frozen"/><selection pane="topRight" activeCell="C1" sqref="C1"/><selection pane="bottomRight" activeCell="$E$7" sqref="E7"/></sheetView></sheetViews><cols><col min="1" max="1" width="11.5" customWidth="1"/><col min="3" max="5" width="64.86"/><col min="6" max="6" hidden="1"/></cols><sheetData>"#;
        assert_eq!(
            layout(xml),
            Layout {
                columns: vec![(0, 0, 11.5, false), (2, 4, 64.86, false), (5, 5, 0.0, true)],
                frozen: (2, 1),
                active: Some((6, 4)),
            }
        );
        // A split that isn't frozen keeps nothing on screen.
        assert_eq!(
            layout(r#"<pane xSplit="900" state="split"/>"#).frozen,
            (0, 0)
        );
        assert_eq!(cell_at("AA10"), Some((9, 26)));
        assert_eq!(cell_at("10"), None);
        assert_eq!(
            attr(r#"sheet name="A &amp; B" r:id="rId3""#, "id").as_deref(),
            Some("rId3")
        );
        assert_eq!(
            attr(r#"sheet name="A &amp; B" sheetId="1""#, "name").as_deref(),
            Some("A & B")
        );
        assert_eq!(
            tags("<x:sheet a='1'/><sheets><sheetView/>", "sheet").count(),
            1
        );
        assert_eq!(
            wrap_text("one two three\nfour", 7),
            ["one two", "three", "four"]
        );
        assert_eq!(wrap_text("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn a_workbook_opens_as_it_was_left_in_excel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coding.xlsx");
        let long = "We live in a country where citizens have become wild game";
        laid_out_workbook(
            &path,
            &[
                ("Instructions", false, &[("A1", "'Read me", 0, "")]),
                (
                    "Coding",
                    false,
                    &[
                        ("A1", "'Sample", 4, ""),
                        ("B1", "'Text", 4, ""),
                        ("C1", "'Code", 4, ""),
                        ("A2", "'S001", 0, ""),
                        ("B2", &format!("'{long}"), 5, ""),
                        ("C2", "1", 0, ""),
                        ("D2", "123456789", 0, ""),
                    ],
                ),
            ],
            &[
                "",
                r#"<sheetViews><sheetView workbookViewId="0"><pane xSplit="2" ySplit="1" topLeftCell="C2" activePane="bottomRight" state="frozen"/><selection pane="bottomRight" activeCell="C2" sqref="C2"/></sheetView></sheetViews><cols><col min="2" max="2" width="20"/><col min="4" max="4" width="4"/></cols>"#,
            ],
            Some(1),
        );
        let mut view = SheetView::open(path, false, None);
        settle(&mut view);
        // The sheet it was saved on, at the cell it was saved on.
        assert_eq!(view.current, 1);
        assert_eq!(view.selected(), Some((1, 2)));
        let grid = view.grid().unwrap();
        assert_eq!(grid.widths[1], 20);
        assert_eq!((grid.frozen_cols, grid.frozen_rows), (2, Some(1)));
        assert_eq!(grid.pinned(), 0..1);
        // Text Excel wraps, wraps; numbers never do.
        assert!(grid.wraps(1, 1, true) && !grid.wraps(1, 1, false));
        assert!(!grid.wraps(1, 2, true));
        assert_eq!(grid.wrapped(1, 1, 20, true).len(), 3);
        assert_eq!(grid.full(1, 1), long);
        assert!(grid.shown(1, 3).number);
    }

    #[test]
    fn csv_rows_keep_their_quotes_and_line_breaks() {
        let text =
            "id;name;note\n1;Ada;\"first; and \"\"best\"\"\"\n2;Grace;\"two\nlines\"\n\n3;Linus;";
        assert_eq!(separator(Path::new("a.csv"), text), ';');
        assert_eq!(separator(Path::new("a.tsv"), text), '\t');
        assert_eq!(separator(Path::new("a.csv"), "a,b\n"), ',');
        let grid = parse_table(text, ';');
        assert_eq!((grid.rows, grid.cols), (5, 3));
        assert_eq!(grid.copied(1, 2), "first; and \"best\"");
        assert_eq!(grid.copied(2, 2), "two\nlines");
        assert_eq!(grid.shown(2, 2).text, "two lines");
        assert_eq!(grid.shown(1, 0).align, Align::Right);
        assert_eq!(grid.shown(1, 1).align, Align::Left);
        // Each row's line in the text, and back: the third row starts on line 2.
        assert_eq!(grid.line(2), Some(2));
        assert_eq!(grid.line(4), Some(5));
        assert_eq!(grid.row_at(3), Some(2));
        assert_eq!(grid.row_at(5), Some(4));
        assert_eq!(parse_table("\u{feff}a,b\r\n1,2\r\n", ',').copied(0, 0), "a");
    }

    #[test]
    fn a_database_shows_its_tables_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.data");
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT, karma REAL, avatar BLOB, bio TEXT);
                 INSERT INTO users VALUES (1, 'ada@example.com', 3.0, x'89504e47', NULL);
                 INSERT INTO users VALUES (2, 'grace@example.com', 12.5, NULL, 'Rear admiral');
                 CREATE TABLE \"order items\"(sku TEXT);
                 CREATE VIEW rich AS SELECT email FROM users WHERE karma > 10;",
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        // Known by its first bytes, whatever its name.
        assert!(is_database(&path));
        let mut view = SheetView::open(path.clone(), false, None);
        settle(&mut view);
        assert_eq!(view.error, None);
        let tabs: Vec<(&str, &str)> = view
            .sheets
            .iter()
            .map(|s| (s.name.as_str(), s.note))
            .collect();
        assert_eq!(tabs, [("order items", ""), ("users", ""), ("rich", "view")]);
        view.switch(1);
        let grid = view.grid().unwrap();
        assert!(grid.named);
        assert_eq!(grid.shown(0, 1).text, "email");
        assert!(grid.shown(0, 1).bold);
        assert_eq!(grid.shown(1, 2).text, "3.0");
        assert_eq!(grid.shown(1, 3).text, "blob, 4 bytes");
        assert_eq!(grid.shown(1, 4).text, "NULL");
        assert_eq!(grid.shown(1, 4).kind, Kind::Missing);
        assert_eq!(grid.copied(1, 4), "");
        assert_eq!(grid.label(2, 1), "email · row 2");
        assert_eq!(grid.number(0), "");
        assert_eq!(grid.number(2), "2");
        assert_eq!(grid.size(), "2 rows · 5 columns");
        // Its first record is selected, under the column names.
        assert_eq!(view.selected(), Some((1, 0)));
        // Read without changing a byte.
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let rows = (0..3).map(|_| vec![Field::Text("n".into())]).collect();
        let part = Grid::table(rows, true, 98, Vec::new());
        assert_eq!(part.size(), "first 2 of 100 rows · 1 column");
    }
}
