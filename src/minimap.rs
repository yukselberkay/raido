//! The minimap: the file at a quarter of its size beside the code, as in VS Code.
//! Each cell is a 2×4 Braille block covering four lines and eight columns, so the
//! code keeps its shape, in the color most of that text has. The part on screen is
//! shaded (the slider), lines changed since Git are marked on the map's left edge,
//! and find matches and errors stand out.

use crate::{
    config::{color, Theme},
    editor::Buffer,
    lsp::Diagnostic,
    markdown::mix,
    quickdiff::Change,
    ui::{change_color, display_width, severity_color},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    Frame,
};
use std::collections::HashMap;
use unicode_width::UnicodeWidthChar;

/// Braille cells across: 80 columns of code.
pub const WIDTH: u16 = 10;
/// The change marks to the map's left, and the map.
pub const SPACE: u16 = WIDTH + 1;
/// Lines in a row of the map: Braille dots are four high.
pub const LINES: usize = 4;
/// Code columns a dot covers. With four lines a row, both ways are a quarter.
const COLUMNS: usize = 4;
const DOTS: usize = WIDTH as usize * 2;
/// The code keeps at least this many columns beside the map; narrower panes go
/// without it.
const MIN_CODE: u16 = 60;
/// Lines whose colors are remembered before starting over.
const REMEMBERED: usize = 20_000;

/// Whether an editor this wide (line numbers included) has room for the map.
pub fn fits(width: u16, gutter: u16) -> bool {
    width >= gutter + MIN_CODE + SPACE
}

/// The first line on the map. A file longer than the map scrolls it with the view,
/// at the pace that brings both ends into reach, as VS Code's does.
pub fn top(lines: usize, scroll: usize, view: usize, rows: u16) -> usize {
    let map = usize::from(rows) * LINES;
    if lines <= map {
        return 0;
    }
    let range = lines.saturating_sub(view).max(1);
    scroll.min(range) * (lines - map) / range
}

/// Where a map was drawn, for clicks and drags on it.
#[derive(Clone, Copy, Debug)]
pub struct Placed {
    pub pane: usize,
    /// The change marks and the map.
    pub area: Rect,
    pub top: usize,
    pub lines: usize,
    pub scroll: usize,
    /// Lines of code on screen.
    pub view: usize,
}

impl Placed {
    /// The first line of row `y`.
    pub fn line(&self, y: u16) -> usize {
        let row = usize::from(y.saturating_sub(self.area.y));
        (self.top + row * LINES).min(self.lines.saturating_sub(1))
    }
    /// Whether row `y` shows part of the screen: the slider.
    pub fn on_slider(&self, y: u16) -> bool {
        let from = self.line(y);
        from < self.scroll + self.view && from + LINES > self.scroll
    }
    /// Lines the view moves for each row the slider is dragged.
    pub fn pace(&self) -> f64 {
        let map = usize::from(self.area.height) * LINES;
        let pace = if self.lines <= map {
            1.0
        } else {
            self.lines.saturating_sub(self.view) as f64
                / map.saturating_sub(self.view).max(1) as f64
        };
        LINES as f64 * pace.max(1.0)
    }
}

/// The dots of each line by its text, remembered after it was highlighted: lines
/// below an edit keep their colors while they're highlighted again.
#[derive(Default)]
pub struct Shades {
    lines: HashMap<u64, [Option<Color>; DOTS]>,
    tab: usize,
}

fn line_key(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The color most of the text in each dot's columns has; `None` for blank ones.
fn dots<'a>(pieces: impl Iterator<Item = (Color, &'a str)>, tab: usize) -> [Option<Color>; DOTS] {
    let mut columns = [None; DOTS * COLUMNS];
    let mut column = 0;
    'line: for (fg, text) in pieces {
        for c in text.chars() {
            let width = if c == '\t' {
                tab - column % tab
            } else {
                UnicodeWidthChar::width(c).unwrap_or(0)
            };
            if !c.is_whitespace() {
                for slot in columns.iter_mut().skip(column).take(width) {
                    *slot = Some(fg);
                }
            }
            column += width;
            if column >= columns.len() {
                break 'line;
            }
        }
    }
    let mut dots = [None; DOTS];
    for (dot, group) in dots.iter_mut().zip(columns.chunks(COLUMNS)) {
        *dot = most(group);
    }
    dots
}

/// The most common color among `colors`, the first of equals.
fn most(colors: &[Option<Color>]) -> Option<Color> {
    let count = |c: &Color| colors.iter().filter(|o| o.as_ref() == Some(c)).count();
    let mut best: Option<(Color, usize)> = None;
    for c in colors.iter().flatten() {
        let n = count(c);
        if best.is_none_or(|(_, m)| n > m) {
            best = Some((*c, n));
        }
    }
    best.map(|(c, _)| c)
}

/// Braille bits for the dot at column `x` (0 or 1) and row `y` (0 to 3).
fn bit(x: usize, y: usize) -> u32 {
    [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]][x][y]
}

/// What the map is made of, besides the text.
pub struct Sources<'a> {
    pub highlight: Option<&'a Vec<Vec<(syntect::highlighting::Style, String)>>>,
    pub shades: Option<&'a mut Shades>,
    pub diagnostics: &'a [Diagnostic],
    pub changes: &'a [Option<Change>],
    pub tab: usize,
}

/// Draw the map of `buffer` in `area` (`SPACE` wide), whose editor shows `view`
/// lines; returns the first line on it.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    buffer: &Buffer,
    view: usize,
    marks: Sources,
    theme: &Theme,
) -> usize {
    let top = top(buffer.lines(), buffer.scroll, view, area.height);
    let tab = marks.tab;
    let mut none = Shades::default();
    let shades = marks.shades.unwrap_or(&mut none);
    if shades.tab != tab || shades.lines.len() > REMEMBERED {
        shades.lines.clear();
        shades.tab = tab;
    }
    let background = color(&theme.background);
    let text = color(&theme.foreground);
    // A faint slider, as in VS Code; the cursor's row in the editor's line color.
    let slider = mix(background, text, 0.1);
    let cursor = color(&theme.selection);
    let end = (buffer.scroll + view).min(buffer.lines());
    for y in 0..area.height {
        let first = top + usize::from(y) * LINES;
        let rows = first..(first + LINES).min(buffer.lines());
        let screen_y = area.y + y;
        let shown = first < end && first + LINES > buffer.scroll;
        let bg = if rows.contains(&buffer.row) {
            cursor
        } else if shown {
            slider
        } else {
            background
        };
        // Changes since Git: the top or bottom half of the mark, or both.
        let mut halves = 0;
        let mut change = None;
        let rank = |c: Change| match c {
            Change::Modified => 2,
            Change::Added => 1,
            Change::Deleted | Change::DeletedAbove => 0,
        };
        for row in rows.clone() {
            if let Some(Some(c)) = marks.changes.get(row) {
                halves |= if row - first < 2 { 1 } else { 2 };
                if change.is_none_or(|old| rank(*c) > rank(old)) {
                    change = Some(*c);
                }
            }
        }
        let glyph = ["", "▘", "▖", "▌"][halves];
        if let (Some(change), false) = (change, glyph.is_empty()) {
            frame.buffer_mut().set_string(
                area.x,
                screen_y,
                glyph,
                Style::default().fg(change_color(change, theme)),
            );
        }
        let raws: Vec<String> = rows.clone().map(|row| buffer.line(row)).collect();
        let lines: Vec<[Option<Color>; DOTS]> = rows
            .clone()
            .zip(&raws)
            .map(|(row, raw)| {
                let key = line_key(raw);
                match marks
                    .highlight
                    .and_then(|lines| lines.get(row))
                    .filter(|line| !line.is_empty())
                {
                    Some(regions) => {
                        let found = dots(
                            regions.iter().map(|(style, piece)| {
                                let fg = style.foreground;
                                (Color::Rgb(fg.r, fg.g, fg.b), piece.as_str())
                            }),
                            tab,
                        );
                        shades.lines.insert(key, found);
                        found
                    }
                    None => shades
                        .lines
                        .get(&key)
                        .copied()
                        .unwrap_or_else(|| dots(std::iter::once((text, raw.as_str())), tab)),
                }
            })
            .collect();
        for cell in 0..WIDTH {
            let mut bits = 0;
            let mut colors = [None; 8];
            for (dy, line) in lines.iter().enumerate() {
                for dx in 0..2 {
                    if let Some(c) = line[usize::from(cell) * 2 + dx] {
                        bits |= bit(dx, dy);
                        colors[dy * 2 + dx] = Some(c);
                    }
                }
            }
            let glyph = char::from_u32(0x2800 + bits).unwrap_or(' ');
            let fg = most(&colors).unwrap_or(text);
            frame.buffer_mut().set_string(
                area.x + 1 + cell,
                screen_y,
                glyph.to_string(),
                Style::default().fg(fg).bg(bg),
            );
        }
        // Find matches and problems: their cells take the mark's color.
        let mut spans: Vec<(usize, usize, Color)> = Vec::new();
        for (row, raw) in rows.zip(&raws) {
            let at = |chars: usize| display_width(raw.chars().take(chars), tab);
            for d in marks
                .diagnostics
                .iter()
                .filter(|d| d.row == row && d.severity <= 2)
            {
                let from = at(d.start);
                spans.push((
                    from,
                    at(d.end).max(from + 1),
                    severity_color(theme, d.severity),
                ));
            }
            if !buffer.search.is_empty() {
                let found = crate::editor::find_all(
                    raw,
                    &buffer.search,
                    !buffer.search_case,
                    buffer.search_word,
                );
                for (byte, end) in found {
                    let from = display_width(raw[..byte].chars(), tab);
                    let to = from + display_width(raw[byte..end].chars(), tab);
                    spans.push((from, to.max(from + 1), color(&theme.warning)));
                }
            }
        }
        for (from, to, mark) in spans {
            let cells = from / (COLUMNS * 2)..(to - 1) / (COLUMNS * 2) + 1;
            for cell in cells.take_while(|c| *c < usize::from(WIDTH)) {
                if let Some(c) = frame
                    .buffer_mut()
                    .cell_mut((area.x + 1 + cell as u16, screen_y))
                {
                    c.set_bg(mark).set_fg(background);
                }
            }
        }
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_dot_takes_the_color_most_of_its_four_columns_have() {
        let (red, blue) = (Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255));
        // "fn" in red, then " xyz" in blue: columns 0 to 3 hold f, n and x, so the
        // first dot is red; y and z make the second blue; the third is blank.
        let line = dots([(red, "fn"), (blue, " xyz   ")].into_iter(), 4);
        assert_eq!(&line[..3], &[Some(red), Some(blue), None]);
        let line = dots([(red, "abcd"), (blue, "e")].into_iter(), 4);
        assert_eq!((line[0], line[1]), (Some(red), Some(blue)));
        // A tab is as wide as it's drawn, and draws nothing.
        let line = dots([(red, "\t\tx")].into_iter(), 4);
        assert_eq!(&line[..3], &[None, None, Some(red)]);
        // Wide characters take two columns; past 80 columns nothing more counts.
        let line = dots([(red, "漢漢漢")].into_iter(), 4);
        assert_eq!(&line[..3], &[Some(red), Some(red), None]);
        let long = "y".repeat(200);
        assert!(dots([(red, long.as_str())].into_iter(), 4)
            .iter()
            .all(Option::is_some));
    }

    #[test]
    fn a_long_file_scrolls_its_map_so_both_ends_are_reachable() {
        // 1000 lines, 40 on screen, a 40-row map (160 lines).
        assert_eq!(top(1000, 0, 40, 40), 0);
        assert_eq!(
            top(1000, 960, 40, 40),
            840,
            "the end of the file at the bottom"
        );
        assert_eq!(top(1000, 480, 40, 40), 420);
        assert_eq!(top(1000, 5000, 40, 40), 840);
        // A file that fits stays put.
        assert_eq!(top(100, 60, 40, 40), 0);
        let map = Placed {
            pane: 0,
            area: Rect::new(50, 2, WIDTH, 40),
            top: 420,
            lines: 1000,
            scroll: 480,
            view: 40,
        };
        assert_eq!(map.line(2), 420);
        assert_eq!(map.line(17), 480);
        assert!(map.on_slider(17) && map.on_slider(26));
        assert!(!map.on_slider(16) && !map.on_slider(27));
        // Dragging the slider a row moves the view the share of the file that row
        // stands for: 960 scrollable lines over 120 rows of slider travel.
        assert_eq!(map.pace(), 32.0);
        let short = Placed {
            lines: 100,
            top: 0,
            ..map
        };
        assert_eq!(short.pace(), 4.0);
    }

    #[test]
    fn braille_bits_follow_the_unicode_dot_order() {
        let all: u32 = (0..2).flat_map(|x| (0..4).map(move |y| bit(x, y))).sum();
        assert_eq!(char::from_u32(0x2800 + all), Some('⣿'));
        assert_eq!(char::from_u32(0x2800 + bit(0, 0) + bit(1, 3)), Some('⢁'));
    }
}
