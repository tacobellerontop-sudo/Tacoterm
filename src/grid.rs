//! The terminal grid: a rows×cols screen of cells, scrollback, an alternate
//! screen, and the selection.
//!
//! The VT parser writes through this; the renderer reads from it. No
//! rendering code lives here — just the buffer and cursor bookkeeping.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

/// A color a cell can carry. Resolved to RGB by the renderer from the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    /// Terminal default (theme foreground/background).
    #[default]
    Default,
    /// An indexed color: 0-15 are the theme's ANSI colors, 16-255 the
    /// xterm 256-color cube and grayscale ramp.
    Indexed(u8),
    /// Truecolor from SGR 38;2 / 48;2.
    Rgb(u8, u8, u8),
}

/// Text attributes, packed as bitflags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr(pub u16);

impl Attr {
    pub const BOLD: u16 = 1 << 0;
    pub const UNDERLINE: u16 = 1 << 1;
    pub const INVERSE: u16 = 1 << 2;
    pub const ITALIC: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    pub const STRIKE: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    /// First half of a double-width character.
    pub const WIDE: u16 = 1 << 8;
    /// Second half of a double-width character: draws nothing itself.
    pub const WIDE_SPACER: u16 = 1 << 9;

    pub fn set(&mut self, flag: u16, on: bool) {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    pub fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attr: Attr::default(),
        }
    }
}

impl Cell {
    fn with_pen(c: char, pen: &Pen) -> Self {
        Cell {
            c,
            fg: pen.fg,
            bg: pen.bg,
            attr: pen.attr,
        }
    }

    /// An erased cell: blank, but keeping the pen's background (xterm's
    /// "background color erase", which ConPTY relies on).
    fn blank(pen: &Pen) -> Self {
        Cell {
            bg: pen.bg,
            ..Cell::default()
        }
    }
}

/// The current drawing state the VT parser writes with.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
}

/// One line of cells. `wrapped` means the line continues on the next one
/// (it was auto-wrapped), so copying joins them without a newline.
#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    fn new(cols: usize, fill: Cell) -> Self {
        Row {
            cells: vec![fill; cols],
            wrapped: false,
        }
    }
}

/// Terminal modes set with `CSI ? n h` / `CSI ? n l`.
#[derive(Clone, Copy, Debug)]
pub struct Modes {
    /// DECAWM (?7): wrap at the right margin.
    pub autowrap: bool,
    /// DECTCEM (?25): show the cursor.
    pub cursor_visible: bool,
    /// DECCKM (?1): arrow keys send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// ?2004: wrap pastes in `ESC [200~` … `ESC [201~`.
    pub bracketed_paste: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            autowrap: true,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    x: usize,
    y: usize,
    pen: Pen,
}

/// A position in the whole history (scrollback + screen). `line` counts
/// from the first line the terminal ever produced, so it stays put while
/// output scrolls and the view moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

impl Selection {
    /// (start, end), start first, both inclusive.
    pub fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    screen: Vec<Row>,
    /// Lines scrolled off the top, oldest first.
    scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    /// Lines evicted from the front of `scrollback` so far; keeps
    /// [`Point::line`] stable.
    lines_dropped: usize,
    /// The primary screen and its cursor, parked while the alternate screen
    /// (vim, less, htop…) is up.
    parked_primary: Option<(Vec<Row>, SavedCursor)>,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub pen: Pen,
    /// Set when a printable landed in the last column: the *next* printable
    /// wraps. (The classic deferred-wrap.)
    pub wrap_pending: bool,
    /// Scroll region (DECSTBM), inclusive rows.
    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<SavedCursor>,
    pub modes: Modes,
    /// How many lines the view is scrolled back into history. 0 = live.
    pub display_offset: usize,
    pub selection: Option<Selection>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Grid {
            cols,
            rows,
            screen: vec![Row::new(cols, Cell::default()); rows],
            scrollback: VecDeque::new(),
            scrollback_limit,
            lines_dropped: 0,
            parked_primary: None,
            cursor_x: 0,
            cursor_y: 0,
            pen: Pen::default(),
            wrap_pending: false,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            saved_cursor: None,
            modes: Modes::default(),
            display_offset: 0,
            selection: None,
        }
    }

    pub fn is_alt_screen(&self) -> bool {
        self.parked_primary.is_some()
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// A cell of the live screen.
    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.screen[y].cells[x]
    }

    /// The row shown at viewport line `y`, taking the scrolled-back view into
    /// account. Scrollback rows keep the width they had, so they may be
    /// shorter or longer than `cols`.
    pub fn visible_row(&self, y: usize) -> &Row {
        if y < self.display_offset {
            &self.scrollback[self.scrollback.len() - self.display_offset + y]
        } else {
            &self.screen[y - self.display_offset]
        }
    }

    fn blank(&self) -> Cell {
        Cell::blank(&self.pen)
    }

    fn blank_row(&self) -> Row {
        Row::new(self.cols, self.blank())
    }

    // ---- printing -------------------------------------------------------

    /// Print one character at the cursor, honoring deferred wrap and
    /// double-width characters.
    pub fn put_char(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // Combining marks and other zero-width characters: dropped for
            // now (they would need cells holding more than one char).
            return;
        }
        if self.wrap_pending && self.modes.autowrap {
            self.screen[self.cursor_y].wrapped = true;
            self.cursor_x = 0;
            self.line_feed();
        }
        self.wrap_pending = false;
        if width == 2 && self.cursor_x + 1 >= self.cols {
            if self.cols < 2 {
                return;
            }
            // A wide char that doesn't fit: pad and wrap first.
            if self.modes.autowrap {
                let y = self.cursor_y;
                self.clear_wide_at(self.cursor_x, y);
                self.screen[y].cells[self.cursor_x] = self.blank();
                self.screen[y].wrapped = true;
                self.cursor_x = 0;
                self.line_feed();
            } else {
                self.cursor_x = self.cols - 2;
            }
        }
        let (x, y) = (self.cursor_x, self.cursor_y);
        self.clear_wide_at(x, y);
        let mut cell = Cell::with_pen(c, &self.pen);
        if width == 2 {
            self.clear_wide_at(x + 1, y);
            cell.attr.set(Attr::WIDE, true);
            let mut spacer = Cell::with_pen(' ', &self.pen);
            spacer.attr.set(Attr::WIDE_SPACER, true);
            self.screen[y].cells[x + 1] = spacer;
        }
        self.screen[y].cells[x] = cell;
        let next = x + width;
        if next >= self.cols {
            self.cursor_x = self.cols - 1;
            self.wrap_pending = self.modes.autowrap;
        } else {
            self.cursor_x = next;
        }
    }

    /// If (x, y) is half of a wide character, blank the other half so no
    /// orphaned halves are left behind when it gets overwritten.
    fn clear_wide_at(&mut self, x: usize, y: usize) {
        if x >= self.cols {
            return;
        }
        let attr = self.screen[y].cells[x].attr;
        if attr.has(Attr::WIDE) && x + 1 < self.cols {
            self.screen[y].cells[x + 1] = Cell::default();
        } else if attr.has(Attr::WIDE_SPACER) && x > 0 {
            self.screen[y].cells[x - 1] = Cell::default();
        }
    }

    // ---- cursor ---------------------------------------------------------

    pub fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_y == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_y + 1 < self.rows {
            self.cursor_y += 1;
        }
    }

    /// RI — move up, scrolling the region down at its top.
    pub fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }

    pub fn carriage_return(&mut self) {
        self.cursor_x = 0;
        self.wrap_pending = false;
    }

    pub fn backspace(&mut self) {
        self.cursor_x = self.cursor_x.saturating_sub(1);
        self.wrap_pending = false;
    }

    pub fn tab(&mut self) {
        self.wrap_pending = false;
        self.cursor_x = (((self.cursor_x / 8) + 1) * 8).min(self.cols - 1);
    }

    pub fn move_cursor_to(&mut self, x: usize, y: usize) {
        self.wrap_pending = false;
        self.cursor_x = x.min(self.cols - 1);
        self.cursor_y = y.min(self.rows - 1);
    }

    pub fn move_cursor_by(&mut self, dx: isize, dy: isize) {
        let x = self.cursor_x as isize + dx;
        let y = self.cursor_y as isize + dy;
        self.move_cursor_to(x.max(0) as usize, y.max(0) as usize);
    }

    /// DECSC / `CSI s`.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            x: self.cursor_x,
            y: self.cursor_y,
            pen: self.pen,
        });
    }

    /// DECRC / `CSI u`.
    pub fn restore_cursor(&mut self) {
        if let Some(s) = self.saved_cursor {
            self.pen = s.pen;
            self.move_cursor_to(s.x, s.y);
        } else {
            self.pen = Pen::default();
            self.move_cursor_to(0, 0);
        }
    }

    // ---- scrolling ------------------------------------------------------

    /// DECSTBM, 0-based inclusive. An invalid region resets to full screen.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.move_cursor_to(0, 0);
    }

    /// Scroll the region up `n` lines (content moves up). Lines leaving the
    /// top of a full-screen region on the primary screen go to scrollback.
    pub fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(bottom - top + 1);
        let to_history = top == 0 && !self.is_alt_screen();
        for _ in 0..n {
            let row = self.screen.remove(top);
            let blank = self.blank_row();
            self.screen.insert(bottom, blank);
            if to_history {
                self.push_scrollback(row);
            }
        }
    }

    /// Scroll the region down `n` lines (content moves down).
    pub fn scroll_down(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            self.screen.remove(bottom);
            let blank = self.blank_row();
            self.screen.insert(top, blank);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_limit == 0 {
            self.lines_dropped += 1;
            return;
        }
        self.scrollback.push_back(row);
        // Keep a scrolled-back view looking at the same lines.
        if self.display_offset > 0 {
            self.display_offset += 1;
        }
        if self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
            self.lines_dropped += 1;
            if let Some(sel) = self.selection {
                if sel.ordered().0.line < self.lines_dropped {
                    self.selection = None;
                }
            }
        }
        self.display_offset = self.display_offset.min(self.scrollback.len());
    }

    /// IL — insert blank lines at the cursor, inside the scroll region.
    pub fn insert_lines(&mut self, n: usize) {
        let y = self.cursor_y;
        if y < self.scroll_top || y > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - y + 1);
        for _ in 0..n {
            self.screen.remove(self.scroll_bottom);
            let blank = self.blank_row();
            self.screen.insert(y, blank);
        }
        self.cursor_x = 0;
        self.wrap_pending = false;
    }

    /// DL — delete lines at the cursor, inside the scroll region.
    pub fn delete_lines(&mut self, n: usize) {
        let y = self.cursor_y;
        if y < self.scroll_top || y > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - y + 1);
        for _ in 0..n {
            self.screen.remove(y);
            let blank = self.blank_row();
            self.screen.insert(self.scroll_bottom, blank);
        }
        self.cursor_x = 0;
        self.wrap_pending = false;
    }

    // ---- editing --------------------------------------------------------

    /// ICH — insert blanks at the cursor, shifting the rest right.
    pub fn insert_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor_x, self.cursor_y);
        let n = n.min(self.cols - x);
        let blank = self.blank();
        let cells = &mut self.screen[y].cells;
        cells.truncate(self.cols - n);
        for _ in 0..n {
            cells.insert(x, blank);
        }
        self.wrap_pending = false;
    }

    /// DCH — delete chars at the cursor, shifting the rest left.
    pub fn delete_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor_x, self.cursor_y);
        let n = n.min(self.cols - x);
        let blank = self.blank();
        let cells = &mut self.screen[y].cells;
        cells.drain(x..x + n);
        cells.extend(std::iter::repeat_n(blank, n));
        self.wrap_pending = false;
    }

    /// ECH — blank `n` chars from the cursor without moving anything.
    pub fn erase_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor_x, self.cursor_y);
        let end = (x + n.max(1)).min(self.cols);
        let blank = self.blank();
        self.screen[y].cells[x..end].fill(blank);
        self.wrap_pending = false;
    }

    /// ED — erase in display. 0: cursor→end, 1: start→cursor, 2: all,
    /// 3: scrollback.
    pub fn erase_display(&mut self, mode: u16) {
        let blank = self.blank();
        let (cx, cy) = (self.cursor_x, self.cursor_y);
        match mode {
            0 => {
                self.screen[cy].cells[cx..].fill(blank);
                self.screen[cy].wrapped = false;
                for row in &mut self.screen[cy + 1..] {
                    row.cells.fill(blank);
                    row.wrapped = false;
                }
            }
            1 => {
                for row in &mut self.screen[..cy] {
                    row.cells.fill(blank);
                    row.wrapped = false;
                }
                self.screen[cy].cells[..=cx].fill(blank);
            }
            2 => {
                for row in &mut self.screen {
                    row.cells.fill(blank);
                    row.wrapped = false;
                }
                self.selection = None;
            }
            3 => {
                self.lines_dropped += self.scrollback.len();
                self.scrollback.clear();
                self.display_offset = 0;
                self.selection = None;
            }
            _ => {}
        }
        self.wrap_pending = false;
    }

    /// EL — erase in line. 0: cursor→end, 1: start→cursor, 2: all.
    pub fn erase_line(&mut self, mode: u16) {
        let blank = self.blank();
        let (cx, cy) = (self.cursor_x, self.cursor_y);
        let row = &mut self.screen[cy];
        match mode {
            0 => {
                row.cells[cx..].fill(blank);
                row.wrapped = false;
            }
            1 => row.cells[..=cx].fill(blank),
            2 => {
                row.cells.fill(blank);
                row.wrapped = false;
            }
            _ => {}
        }
        self.wrap_pending = false;
    }

    // ---- screens --------------------------------------------------------

    /// ?1049 / ?1047 / ?47 set: switch to a fresh alternate screen.
    pub fn enter_alt_screen(&mut self) {
        if self.is_alt_screen() {
            return;
        }
        let cursor = SavedCursor {
            x: self.cursor_x,
            y: self.cursor_y,
            pen: self.pen,
        };
        let blank_screen = vec![Row::new(self.cols, Cell::default()); self.rows];
        let primary = std::mem::replace(&mut self.screen, blank_screen);
        self.parked_primary = Some((primary, cursor));
        self.display_offset = 0;
        self.selection = None;
        self.wrap_pending = false;
    }

    /// ?1049 / ?1047 / ?47 reset: back to the primary screen as it was.
    pub fn exit_alt_screen(&mut self) {
        if let Some((primary, cursor)) = self.parked_primary.take() {
            self.screen = primary;
            self.pen = cursor.pen;
            self.move_cursor_to(cursor.x, cursor.y);
            self.selection = None;
        }
    }

    /// RIS — full reset (keeps scrollback).
    pub fn reset(&mut self) {
        self.exit_alt_screen();
        self.pen = Pen::default();
        self.modes = Modes::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved_cursor = None;
        for row in &mut self.screen {
            *row = Row::new(self.cols, Cell::default());
        }
        self.move_cursor_to(0, 0);
        self.selection = None;
    }

    /// Resize the screen. Rows that no longer fit above the cursor move to
    /// scrollback; columns are cut or padded.
    /// TODO: reflow (rewrap long lines) like real terminals do. ConPTY
    /// repaints the screen after a resize, which hides most of the gap.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let fit = |screen: &mut Vec<Row>| {
            for row in screen.iter_mut() {
                row.cells.resize(cols, Cell::default());
            }
        };
        // Shrinking: drop lines off the top until the cursor fits, then cut
        // the bottom.
        if rows < self.rows {
            let overflow = (self.cursor_y + 1).saturating_sub(rows);
            for _ in 0..overflow {
                let row = self.screen.remove(0);
                if !self.is_alt_screen() {
                    self.push_scrollback(row);
                }
            }
            self.cursor_y -= overflow;
            self.screen.truncate(rows);
        }
        while self.screen.len() < rows {
            self.screen.push(Row::new(cols, Cell::default()));
        }
        fit(&mut self.screen);
        if let Some((primary, cursor)) = &mut self.parked_primary {
            primary.truncate(rows);
            while primary.len() < rows {
                primary.push(Row::new(cols, Cell::default()));
            }
            fit(primary);
            cursor.x = cursor.x.min(cols - 1);
            cursor.y = cursor.y.min(rows - 1);
        }
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cursor_x = self.cursor_x.min(cols - 1);
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.wrap_pending = false;
        self.selection = None;
        self.display_offset = self.display_offset.min(self.scrollback.len());
    }

    // ---- view + selection -----------------------------------------------

    /// Scroll the view: positive goes back into history.
    pub fn scroll_view(&mut self, lines: isize) {
        let max = self.scrollback.len() as isize;
        self.display_offset = (self.display_offset as isize + lines).clamp(0, max) as usize;
    }

    pub fn scroll_view_to_bottom(&mut self) {
        self.display_offset = 0;
    }

    /// The history position of viewport cell (col, y).
    pub fn viewport_point(&self, col: usize, y: usize) -> Point {
        let y = y.min(self.rows - 1);
        Point {
            line: self.lines_dropped + self.scrollback.len() - self.display_offset + y,
            col: col.min(self.cols - 1),
        }
    }

    fn row_at(&self, line: usize) -> Option<&Row> {
        let idx = line.checked_sub(self.lines_dropped)?;
        if idx < self.scrollback.len() {
            self.scrollback.get(idx)
        } else {
            self.screen.get(idx - self.scrollback.len())
        }
    }

    pub fn start_selection(&mut self, p: Point) {
        self.selection = Some(Selection { anchor: p, head: p });
    }

    pub fn extend_selection(&mut self, p: Point) {
        if let Some(sel) = &mut self.selection {
            sel.head = p;
        }
    }

    /// Whether viewport cell (col, y) is inside the selection.
    pub fn is_selected(&self, col: usize, y: usize) -> bool {
        let Some(sel) = self.selection else {
            return false;
        };
        let (start, end) = sel.ordered();
        let p = self.viewport_point(col, y);
        p >= start && p <= end
    }

    /// The selected text: trailing blanks trimmed per line, wrapped lines
    /// joined, lines separated by `\n`.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection?.ordered();
        let mut out = String::new();
        for line in start.line..=end.line {
            let Some(row) = self.row_at(line) else {
                continue;
            };
            let from = if line == start.line { start.col } else { 0 };
            let to = if line == end.line {
                end.col + 1
            } else {
                row.cells.len()
            };
            let mut text: String = row
                .cells
                .iter()
                .take(to.min(row.cells.len()))
                .skip(from)
                .filter(|c| !c.attr.has(Attr::WIDE_SPACER))
                .map(|c| c.c)
                .collect();
            let joins_next = row.wrapped && line != end.line;
            if !joins_next {
                text.truncate(text.trim_end().len());
            }
            out.push_str(&text);
            if line != end.line && !joins_next {
                out.push('\n');
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(g: &Grid, y: usize) -> String {
        (0..g.cols)
            .map(|x| g.cell(x, y))
            .filter(|c| !c.attr.has(Attr::WIDE_SPACER))
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn type_str(g: &mut Grid, s: &str) {
        for c in s.chars() {
            g.put_char(c);
        }
    }

    #[test]
    fn deferred_wrap_and_wrapped_flag() {
        let mut g = Grid::new(4, 3, 100);
        type_str(&mut g, "abcd");
        assert_eq!((g.cursor_x, g.cursor_y), (3, 0));
        assert!(g.wrap_pending);
        type_str(&mut g, "e");
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1), "e");
        assert!(g.visible_row(0).wrapped);
    }

    #[test]
    fn scrolling_feeds_scrollback_and_view() {
        let mut g = Grid::new(5, 2, 100);
        for line in ["one", "two", "three"] {
            type_str(&mut g, line);
            g.carriage_return();
            g.line_feed();
        }
        assert_eq!(g.scrollback_len(), 2);
        assert_eq!(row_text(&g, 0), "three");
        g.scroll_view(10);
        assert_eq!(g.display_offset, 2);
        let first: String = g.visible_row(0).cells.iter().map(|c| c.c).collect();
        assert_eq!(first.trim_end(), "one");
    }

    #[test]
    fn scrollback_limit_evicts_oldest() {
        let mut g = Grid::new(3, 1, 2);
        for _ in 0..5 {
            g.put_char('x');
            g.line_feed();
        }
        assert_eq!(g.scrollback_len(), 2);
    }

    #[test]
    fn alt_screen_round_trip() {
        let mut g = Grid::new(5, 2, 100);
        type_str(&mut g, "hi");
        g.enter_alt_screen();
        assert_eq!(row_text(&g, 0), "");
        type_str(&mut g, "vim");
        g.line_feed();
        g.line_feed();
        assert_eq!(g.scrollback_len(), 0, "alt screen never feeds scrollback");
        g.exit_alt_screen();
        assert_eq!(row_text(&g, 0), "hi");
        assert_eq!((g.cursor_x, g.cursor_y), (2, 0));
    }

    #[test]
    fn wide_chars_take_two_columns() {
        let mut g = Grid::new(4, 2, 0);
        type_str(&mut g, "a漢");
        assert_eq!(g.cursor_x, 3);
        assert!(g.cell(1, 0).attr.has(Attr::WIDE));
        assert!(g.cell(2, 0).attr.has(Attr::WIDE_SPACER));
        // Doesn't fit in the last column: wraps whole.
        type_str(&mut g, "b字");
        assert_eq!(row_text(&g, 0), "a漢b");
        assert_eq!(row_text(&g, 1), "字");
    }

    #[test]
    fn scroll_region_keeps_outside_lines() {
        let mut g = Grid::new(3, 4, 100);
        for (y, s) in ["a", "b", "c", "d"].iter().enumerate() {
            g.move_cursor_to(0, y);
            type_str(&mut g, s);
        }
        g.set_scroll_region(1, 2);
        g.move_cursor_to(0, 2);
        g.line_feed();
        assert_eq!(row_text(&g, 0), "a");
        assert_eq!(row_text(&g, 1), "c");
        assert_eq!(row_text(&g, 2), "");
        assert_eq!(row_text(&g, 3), "d");
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn selection_text_joins_wrapped_lines() {
        let mut g = Grid::new(4, 3, 100);
        type_str(&mut g, "abcdef");
        g.carriage_return();
        g.line_feed();
        type_str(&mut g, "gh  ");
        g.start_selection(g.viewport_point(1, 0));
        g.extend_selection(g.viewport_point(3, 2));
        assert_eq!(g.selection_text().unwrap(), "bcdef\ngh");
    }

    #[test]
    fn selection_survives_scrolling() {
        let mut g = Grid::new(4, 2, 100);
        type_str(&mut g, "keep");
        g.start_selection(g.viewport_point(0, 0));
        g.extend_selection(g.viewport_point(3, 0));
        g.carriage_return();
        g.line_feed();
        g.line_feed();
        g.line_feed();
        assert_eq!(g.selection_text().unwrap(), "keep");
    }

    #[test]
    fn insert_and_delete_chars() {
        let mut g = Grid::new(5, 1, 0);
        type_str(&mut g, "abcde");
        g.move_cursor_to(1, 0);
        g.delete_chars(2);
        assert_eq!(row_text(&g, 0), "ade");
        g.insert_chars(1);
        assert_eq!(row_text(&g, 0), "a de");
    }

    #[test]
    fn resize_shrink_keeps_cursor_line() {
        let mut g = Grid::new(4, 4, 100);
        for (y, s) in ["a", "b", "c", "d"].iter().enumerate() {
            g.move_cursor_to(0, y);
            type_str(&mut g, s);
        }
        g.resize(2, 2);
        assert_eq!(row_text(&g, 0), "c");
        assert_eq!(row_text(&g, 1), "d");
        assert_eq!(g.scrollback_len(), 2);
        assert_eq!(g.cursor_y, 1);
    }
}
