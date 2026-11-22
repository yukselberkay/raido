//! Spreadsheets, read-only: Excel workbooks (.xlsx, .xlsm, .xlsb, .xls) and
//! OpenDocument ones (.ods) shown as a grid, one sheet at a time. A cell shows the
//! value the file saved (formulas aren't worked out again) as its number format
//! asks: 15%, 1,234.50, € 9.99, 1.23E+04; dates as 2024-03-15. The workbook is
//! read in the background, the sheet you see first.
use calamine::{
    open_workbook_auto, Data, ExcelDateTime, FontWeight, HorizontalAlignment, Range, Reader,
    SheetType, SheetVisible, StyleRange,
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::SystemTime,
};
use unicode_width::UnicodeWidthStr;

/// The files shown as a spreadsheet.
const EXTENSIONS: [&str; 7] = ["xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods"];
/// Larger workbooks aren't read: a sheet's cells take far more memory than the file.
const MAX_FILE: u64 = 64 << 20;
/// Rows read to size the columns.
const SIZED_ROWS: u32 = 2000;
/// A column's width, in characters, is at least and at most this.
const NARROWEST: u16 = 3;
const WIDEST: u16 = 40;

/// Whether `path` is a workbook Raido shows as a spreadsheet.
pub fn is_spreadsheet(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// A workbook in a pane.
pub struct SheetView {
    pub path: PathBuf,
    pub sheets: Vec<Sheet>,
    /// The sheet shown.
    pub current: usize,
    /// Opened by browsing the explorer: the next file browsed takes its place.
    pub peeked: bool,
    /// Why the workbook can't be shown.
    pub error: Option<String>,
    /// `/` and what's typed so far.
    pub typing: Option<String>,
    /// The last search, for n and N.
    pub query: String,
    /// Data rows on screen, recorded at draw time for paging.
    pub page: u32,
    /// Where the grid was drawn, for clicks.
    pub drawn: Drawn,
    /// The file's modification time and size when it was read.
    stamp: Option<(SystemTime, u64)>,
    loading: Option<Receiver<Loaded>>,
    /// Where to start: the sheet, row and column a session saved.
    start: (usize, u32, u32),
}

/// One sheet of the workbook, and where you are in it.
pub struct Sheet {
    pub name: String,
    pub hidden: bool,
    /// The cells: `None` while they're read, an error for a sheet without any.
    pub grid: Option<Result<Grid, String>>,
    /// The selected cell, and the first row and column on screen.
    pub row: u32,
    pub col: u32,
    pub top: u32,
    pub left: u32,
}

/// Screen positions of the grid's columns, rows and sheet tabs.
#[derive(Default)]
pub struct Drawn {
    /// (x, width, column)
    pub columns: Vec<(u16, u16, u32)>,
    /// (y, row)
    pub rows: Vec<(u16, u32)>,
    /// (x, width, sheet), on line `tabs_y`.
    pub tabs: Vec<(u16, u16, usize)>,
    pub tabs_y: Option<u16>,
}

enum Loaded {
    /// The sheets' names, whether each is hidden, and whether it's a chart.
    Names(Vec<(String, bool, bool)>),
    Sheet(usize, Result<Grid, String>),
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
}

/// A cell as the grid shows it.
pub struct Shown {
    pub text: String,
    pub align: Align,
    pub bold: bool,
    pub kind: Kind,
}

/// A sheet's cells, with their formulas and styles.
pub struct Grid {
    values: Range<Data>,
    formulas: Range<String>,
    styles: StyleRange,
    /// Rows and columns from A1 to the last cell used.
    pub rows: u32,
    pub cols: u32,
    /// The first row used: kept on screen as a header while the sheet scrolls.
    pub first: u32,
    /// Each column's width, in characters.
    pub widths: Vec<u16>,
}

impl Grid {
    pub fn new(values: Range<Data>, formulas: Range<String>, styles: StyleRange) -> Self {
        let mut grid = Self {
            rows: 0,
            cols: 0,
            first: 0,
            widths: Vec::new(),
            values,
            formulas,
            styles,
        };
        let ends = [grid.values.end(), non_empty_end(&grid.formulas)];
        for (row, col) in ends.into_iter().flatten() {
            grid.rows = grid.rows.max(row + 1);
            grid.cols = grid.cols.max(col + 1);
        }
        let starts = [
            grid.values.start().filter(|_| !grid.values.is_empty()),
            grid.formulas.start().filter(|_| !grid.formulas.is_empty()),
        ];
        grid.first = starts
            .into_iter()
            .flatten()
            .map(|(row, _)| row)
            .min()
            .unwrap_or(0);
        let last = (grid.first + SIZED_ROWS).min(grid.rows);
        grid.widths = (0..grid.cols)
            .map(|col| {
                let widest = (grid.first..last)
                    .map(|row| grid.shown(row, col).text.width())
                    .max()
                    .unwrap_or(0);
                (widest.min(usize::from(WIDEST)) as u16)
                    .max(NARROWEST)
                    .max(column_name(col).len() as u16)
            })
            .collect();
        grid
    }

    fn value(&self, row: u32, col: u32) -> &Data {
        self.values.get_value((row, col)).unwrap_or(&Data::Empty)
    }

    /// The cell's formula, without its `=`.
    pub fn formula(&self, row: u32, col: u32) -> Option<&str> {
        self.formulas
            .get_value((row, col))
            .map(String::as_str)
            .filter(|f| !f.is_empty())
    }

    fn format_code(&self, row: u32, col: u32) -> Option<&str> {
        let style = self.styles.get((row, col))?;
        Some(style.number_format.as_ref()?.format_code.as_str())
    }

    /// The cell as the grid shows it: its value in its format, on one line.
    pub fn shown(&self, row: u32, col: u32) -> Shown {
        let style = self.styles.get((row, col));
        let bold = style
            .and_then(|s| s.font.as_ref())
            .is_some_and(|f| f.weight == FontWeight::Bold);
        let value = self.value(row, col);
        let (text, kind, natural) = match value {
            Data::Empty => match self.formula(row, col) {
                Some(formula) => (format!("={formula}"), Kind::Formula, Align::Left),
                None => (String::new(), Kind::Value, Align::Left),
            },
            Data::String(s) => (one_line(s), Kind::Value, Align::Left),
            Data::Error(e) => (e.to_string(), Kind::Error, Align::Center),
            Data::Bool(_) => (display(value, None), Kind::Value, Align::Center),
            _ => (
                display(value, self.format_code(row, col)),
                Kind::Value,
                Align::Right,
            ),
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
        Shown {
            text,
            align,
            bold,
            kind,
        }
    }

    /// The cell's text for the clipboard: as shown, but a string keeps its lines.
    pub fn copied(&self, row: u32, col: u32) -> String {
        match self.value(row, col) {
            Data::String(s) => s.clone(),
            _ => self.shown(row, col).text,
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

impl SheetView {
    /// Start reading the workbook at `path`; `start` is the sheet, row and column
    /// to show.
    pub fn open(path: PathBuf, peeked: bool, start: (usize, u32, u32)) -> Self {
        let mut view = Self {
            path,
            sheets: Vec::new(),
            current: 0,
            peeked,
            error: None,
            typing: None,
            query: String::new(),
            page: 20,
            drawn: Drawn::default(),
            stamp: None,
            loading: None,
            start,
        };
        view.load();
        view
    }

    fn load(&mut self) {
        self.error = None;
        self.stamp = stamp(&self.path);
        match std::fs::metadata(&self.path) {
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
            Ok(m) if m.len() > MAX_FILE => {
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
        let first = self.start.0;
        thread::spawn(move || read(&path, first, &send));
        self.loading = Some(receive);
    }

    /// Read the file again (it changed on disk), keeping each sheet's place.
    pub fn reload(&mut self) {
        if let Some(sheet) = self.sheets.get(self.current) {
            self.start = (self.current, sheet.row, sheet.col);
        }
        self.load();
    }

    /// Whether the file changed since it was read.
    pub fn changed_on_disk(&self) -> bool {
        let now = stamp(&self.path);
        now.is_some() && now != self.stamp && self.loading.is_none()
    }

    /// Whether the workbook is still being read.
    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Take what the reader has read; true when there's something new to show.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(loading) = &self.loading {
            match loading.try_recv() {
                Ok(Loaded::Names(names)) => {
                    self.named(names);
                    changed = true;
                }
                Ok(Loaded::Sheet(index, grid)) => {
                    if let Some(sheet) = self.sheets.get_mut(index) {
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
    fn named(&mut self, names: Vec<(String, bool, bool)>) {
        let mut old = std::mem::take(&mut self.sheets);
        let shown = old.get(self.current).map(|s| s.name.clone());
        let reloaded = !old.is_empty();
        self.sheets = names
            .into_iter()
            .map(|(name, hidden, _)| {
                let before = old.iter_mut().find(|s| s.name == name);
                match before {
                    Some(s) => Sheet {
                        name,
                        hidden,
                        grid: s.grid.take(),
                        row: s.row,
                        col: s.col,
                        top: s.top,
                        left: s.left,
                    },
                    None => Sheet {
                        name,
                        hidden,
                        grid: None,
                        row: 0,
                        col: 0,
                        top: 0,
                        left: 0,
                    },
                }
            })
            .collect();
        let last = self.sheets.len().saturating_sub(1);
        self.current = match shown.filter(|_| reloaded) {
            Some(name) => self.sheets.iter().position(|s| s.name == name).unwrap_or(0),
            None => {
                let (sheet, row, col) = self.start;
                let current = first_shown(&self.sheets, sheet.min(last));
                if let Some(s) = self.sheets.get_mut(current) {
                    if current == sheet {
                        s.row = row;
                        s.col = col;
                    }
                }
                current
            }
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

    /// Move the selection by rows and columns, staying on the sheet.
    pub fn move_by(&mut self, rows: i64, cols: i64) {
        if let Some(sheet) = self.sheet_mut() {
            sheet.row = (i64::from(sheet.row) + rows).max(0) as u32;
            sheet.col = (i64::from(sheet.col) + cols).max(0) as u32;
            sheet.clamp();
        }
    }

    /// Select a row or a column (`u32::MAX`: the last one).
    pub fn go(&mut self, row: Option<u32>, col: Option<u32>) {
        if let Some(sheet) = self.sheet_mut() {
            if let Some(row) = row {
                sheet.row = row;
            }
            if let Some(col) = col {
                sheet.col = col;
            }
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
        let row = self.drawn.rows.iter().find(|(at, _)| *at == y).map(|r| r.1);
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
        Some((cell_name(row, col), grid.copied(row, col)))
    }

    /// The selected cell in full, on one line: B7: its text · =its formula.
    pub fn described(&self) -> Option<String> {
        let grid = self.grid()?;
        let (row, col) = self.selected()?;
        let text = one_line(&grid.copied(row, col));
        let formula = grid
            .formula(row, col)
            .map(|f| format!(" · ={f}"))
            .unwrap_or_default();
        Some(format!("{}: {text}{formula}", cell_name(row, col)))
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
        let (rows, cols) = self.extent();
        if rows > 0 {
            self.row = self.row.min(rows - 1);
            self.col = self.col.min(cols.saturating_sub(1));
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

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Read the workbook: its sheets' names, then their cells, sheet `first` (or the
/// first one shown) first. Stops when nobody is waiting any more.
fn read(path: &Path, first: usize, send: &Sender<Loaded>) {
    let mut book = match open_workbook_auto(path) {
        Ok(book) => book,
        Err(error) => {
            let _ = send.send(Loaded::Failed(describe(&error.to_string())));
            return;
        }
    };
    let names: Vec<(String, bool, bool)> = book
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
    if send.send(Loaded::Names(names.clone())).is_err() {
        return;
    }
    let first = first.min(names.len().saturating_sub(1));
    let first = if names.get(first).is_some_and(|n| !n.1) {
        first
    } else {
        names.iter().position(|n| !n.1).unwrap_or(first)
    };
    let order = std::iter::once(first).chain((0..names.len()).filter(|&i| i != first));
    for index in order {
        let Some((name, _, chart)) = names.get(index) else {
            continue;
        };
        let grid = if *chart {
            Err("A chart sheet: Raido shows the cells of the other sheets".to_string())
        } else {
            match book.worksheet_range(name) {
                Ok(values) => {
                    let formulas = book.worksheet_formula(name).unwrap_or_default();
                    let styles = book.worksheet_style(name).unwrap_or_default();
                    Ok(Grid::new(values, formulas, styles))
                }
                Err(error) => Err(format!("Can't read this sheet: {error}")),
            }
        };
        if send.send(Loaded::Sheet(index, grid)).is_err() {
            return;
        }
    }
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
    /// 0.0%, 2 is #,##0.00, 3 a date and 4 bold.
    pub(crate) fn workbook(path: &Path, sheets: &[(&str, bool, &[Cell])]) {
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
                    r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
                ),
            );
        }
        types.push_str("</Types>");
        book.push_str("</sheets></workbook>");
        rels.push_str("</Relationships>");
        put("[Content_Types].xml", types);
        put("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.into());
        put("xl/workbook.xml", book);
        put("xl/_rels/workbook.xml.rels", rels);
        put("xl/styles.xml", r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="0.0%"/></numFmts><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0"/></cellStyleXfs><cellXfs count="5"><xf numFmtId="0" fontId="0" xfId="0"/><xf numFmtId="164" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="4" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="14" fontId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="0" fontId="1" xfId="0" applyFont="1"/></cellXfs></styleSheet>"#.into());
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
        let mut view = SheetView::open(path, false, (0, 0, 0));
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
        let mut view = SheetView::open(path, false, (0, 0, 0));
        settle(&mut view);
        assert!(view.error.is_some());
        let view = SheetView::open(dir.path().join("gone.xlsx"), false, (0, 0, 0));
        assert!(view.error.is_some());
        assert!(is_spreadsheet(Path::new("a/B.XLSX")));
        assert!(is_spreadsheet(Path::new("b.ods")));
        assert!(!is_spreadsheet(Path::new("c.csv")));
    }
}
