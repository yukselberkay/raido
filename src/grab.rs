//! Selecting text with the mouse in a terminal, AI, preview, Git or review pane:
//! the drag stays inside the pane it began in, so the copy holds that pane's text
//! and nothing from the panes beside it. Editors select their own text instead (a
//! Visual selection), so they can scroll and edit it.
use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthStr;

pub struct Grab {
    pub pane: usize,
    /// The pane's text area on screen; both ends stay inside it.
    pub area: Rect,
    /// Where the drag began and where it is now, as screen cells.
    pub from: (u16, u16),
    pub to: (u16, u16),
    /// A double click: both ends grow to whole words when next drawn.
    pub words: bool,
    /// The button is up: the selection is copied when next drawn.
    pub done: bool,
    pub copied: bool,
}

/// Characters a double click takes as one word: names, paths and URLs.
fn word_char(symbol: &str) -> bool {
    symbol
        .chars()
        .all(|c| c.is_alphanumeric() || "_-./~:@%+#?&=".contains(c))
        && !symbol.is_empty()
        && symbol != " "
}

impl Grab {
    pub fn new(pane: usize, area: Rect, at: (u16, u16)) -> Self {
        let at = clamp(area, at);
        Self {
            pane,
            area,
            from: at,
            to: at,
            words: false,
            done: false,
            copied: false,
        }
    }
    pub fn drag(&mut self, at: (u16, u16)) {
        self.to = clamp(self.area, at);
    }
    /// The ends in reading order.
    fn ends(&self) -> ((u16, u16), (u16, u16)) {
        let (a, b) = (self.from, self.to);
        if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        }
    }
    /// The selected columns of screen row `y`, as `x0..x1`.
    pub fn columns(&self, y: u16) -> Option<std::ops::Range<u16>> {
        let ((x0, y0), (x1, y1)) = self.ends();
        if y < y0 || y > y1 {
            return None;
        }
        let start = if y == y0 { x0 } else { self.area.x };
        let end = if y == y1 { x1 + 1 } else { self.area.right() };
        Some(start..end)
    }
    /// After a double click, widen both ends to the word under them.
    pub fn grow(&mut self, buffer: &Buffer) {
        if !self.words {
            return;
        }
        self.words = false;
        let symbol = |x: u16, y: u16| buffer.cell((x, y)).map_or("", |c| c.symbol());
        let ((mut x0, y0), (mut x1, y1)) = self.ends();
        if !word_char(symbol(x0, y0)) {
            return;
        }
        while x0 > self.area.x && word_char(symbol(x0 - 1, y0)) {
            x0 -= 1;
        }
        while x1 + 1 < self.area.right() && word_char(symbol(x1 + 1, y1)) {
            x1 += 1;
        }
        self.from = (x0, y0);
        self.to = (x1, y1);
    }
    /// The selected text as drawn. Rows end in newlines without their trailing
    /// blanks, except rows in `wrapped` (by offset from the top of the area): a
    /// terminal line too long for the pane is copied as the one line it is.
    pub fn text(&self, buffer: &Buffer, wrapped: &[bool]) -> String {
        let ((_, y0), (_, y1)) = self.ends();
        let mut out = String::new();
        for y in y0..=y1 {
            let Some(columns) = self.columns(y) else {
                continue;
            };
            let full = columns.end == self.area.right();
            let mut row = String::new();
            let mut x = columns.start;
            while x < columns.end {
                let symbol = buffer.cell((x, y)).map_or(" ", |c| c.symbol());
                row.push_str(if symbol.is_empty() { " " } else { symbol });
                // A wide character's second cell is drawn as a blank: skip it.
                x += symbol.width().max(1) as u16;
            }
            let joined = full && y < y1 && wrapped.get(usize::from(y - self.area.y)) == Some(&true);
            if joined {
                out.push_str(&row);
            } else {
                out.push_str(row.trim_end());
                if y < y1 {
                    out.push('\n');
                }
            }
        }
        // Blank rows the drag ran on into aren't worth copying.
        out.truncate(out.trim_end_matches('\n').len());
        out
    }
}

fn clamp(area: Rect, (x, y): (u16, u16)) -> (u16, u16) {
    (
        x.clamp(area.x, area.right().saturating_sub(1).max(area.x)),
        y.clamp(area.y, area.bottom().saturating_sub(1).max(area.y)),
    )
}

/// What a copy notice says about `text`.
pub fn describe(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => {
            let count = text.chars().count();
            format!("{count} character{}", if count == 1 { "" } else { "s" })
        }
        lines => format!("{lines} lines"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn screen(rows: &[&str], area: Rect) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        // Text to the left and right of the pane: never part of the copy.
        for y in 0..10 {
            buffer.set_string(0, y, "LEFT", Style::default());
            buffer.set_string(area.right(), y, "RIGHT", Style::default());
        }
        for (y, row) in rows.iter().enumerate() {
            buffer.set_string(area.x, area.y + y as u16, row, Style::default());
        }
        buffer
    }

    #[test]
    fn a_drag_copies_only_its_own_pane() {
        let area = Rect::new(5, 1, 20, 6);
        let buffer = screen(&["first line", "second line", "third"], area);
        // Dragged from "line" on the first row out past the pane's right and bottom.
        let mut grab = Grab::new(0, area, (11, 1));
        grab.drag((39, 9));
        assert_eq!(grab.to, (24, 6));
        assert_eq!(grab.text(&buffer, &[]), "line\nsecond line\nthird");
        // Backwards, from "third" up to "line" on the row above.
        let mut grab = Grab::new(0, area, (9, 3));
        grab.drag((12, 2));
        assert_eq!(grab.text(&buffer, &[]), "line\nthird");
        assert_eq!(grab.columns(1), None);
        assert_eq!(grab.columns(2), Some(12..25));
    }

    #[test]
    fn wrapped_terminal_rows_copy_as_one_line_and_wide_characters_once() {
        let area = Rect::new(5, 0, 6, 4);
        let buffer = screen(&["abcdef", "gh", "日本x"], area);
        let mut grab = Grab::new(0, area, (5, 0));
        grab.drag((10, 2));
        assert_eq!(grab.text(&buffer, &[true, false, false]), "abcdefgh\n日本x");
    }

    #[test]
    fn a_double_click_takes_the_whole_word_or_path() {
        let area = Rect::new(5, 0, 30, 2);
        let buffer = screen(&["open src/main.rs now"], area);
        let mut grab = Grab::new(0, area, (14, 0));
        grab.words = true;
        grab.grow(&buffer);
        assert_eq!(grab.text(&buffer, &[]), "src/main.rs");
        // On a blank, nothing more than the blank.
        let mut grab = Grab::new(0, area, (9, 0));
        grab.words = true;
        grab.grow(&buffer);
        assert_eq!(grab.text(&buffer, &[]), "");
        assert_eq!(describe("src/main.rs"), "11 characters");
        assert_eq!(describe("a\nb\nc"), "3 lines");
    }
}
