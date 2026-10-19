//! The terminal grid: a rows×cols buffer of cells plus scrollback.
//!
//! The VT parser writes through this; the renderer reads from it. No
//! rendering code lives here — just the buffer and cursor bookkeeping.

/// A color a cell can carry. Resolved to RGB by the renderer from the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// Terminal default (theme foreground/background).
    Default,
    /// One of the 16 ANSI colors, 0-15.
    Ansi(u8),
    /// Truecolor from SGR 38;2 / 48;2.
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Color::Default
    }
}

/// Text attributes, packed as bitflags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr(pub u8);

impl Attr {
    pub const BOLD: u8 = 0b0000_0001;
    pub const UNDERLINE: u8 = 0b0000_0010;
    pub const INVERSE: u8 = 0b0000_0100;

    pub fn set(&mut self, flag: u8, on: bool) {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Clone, Copy, Debug)]
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
}

/// The current drawing state the VT parser writes with.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    /// Lines scrolled off the top, oldest first.
    pub scrollback: Vec<Vec<Cell>>,
    pub scrollback_limit: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub pen: Pen,
    /// Set when a printable landed in the last column: the *next* printable
    /// wraps. (The classic deferred-wrap.)
    pub wrap_pending: bool,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            scrollback: Vec::new(),
            scrollback_limit,
            cursor_x: 0,
            cursor_y: 0,
            pen: Pen::default(),
            wrap_pending: false,
        }
    }

    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.cells[y * self.cols + x]
    }

    fn cell_mut(&mut self, x: usize, y: usize) -> &mut Cell {
        &mut self.cells[y * self.cols + x]
    }

    /// Print one character at the cursor, honoring deferred wrap.
    /// TODO: wide (double-column) characters currently take one column.
    pub fn put_char(&mut self, c: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor_x = 0;
            self.line_feed();
        }
        let pen = self.pen; // Copy first: cell_mut borrows self mutably
        let (x, y) = (self.cursor_x, self.cursor_y);
        *self.cell_mut(x, y) = Cell::with_pen(c, &pen);
        if x + 1 >= self.cols {
            self.wrap_pending = true;
        } else {
            self.cursor_x = x + 1;
        }
    }

    pub fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_y + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_y += 1;
        }
    }

    fn scroll_up(&mut self) {
        let top: Vec<Cell> = self.cells.drain(..self.cols).collect();
        self.scrollback.push(top);
        if self.scrollback.len() > self.scrollback_limit {
            let excess = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(..excess);
        }
        self.cells
            .extend(std::iter::repeat(Cell::default()).take(self.cols));
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
        self.cursor_x = ((self.cursor_x / 8) + 1) * 8;
        if self.cursor_x >= self.cols {
            self.cursor_x = self.cols - 1;
        }
    }

    pub fn move_cursor_to(&mut self, x: usize, y: usize) {
        self.wrap_pending = false;
        self.cursor_x = x.min(self.cols.saturating_sub(1));
        self.cursor_y = y.min(self.rows.saturating_sub(1));
    }

    pub fn move_cursor_by(&mut self, dx: isize, dy: isize) {
        let x = self.cursor_x as isize + dx;
        let y = self.cursor_y as isize + dy;
        self.move_cursor_to(x.max(0) as usize, y.max(0) as usize);
    }

    /// ED — erase in display. 0: cursor→end, 1: start→cursor, 2/3: all.
    pub fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                let (cx, cy) = (self.cursor_x, self.cursor_y);
                for y in cy..self.rows {
                    let start_x = if y == cy { cx } else { 0 };
                    for x in start_x..self.cols {
                        *self.cell_mut(x, y) = Cell::default();
                    }
                }
            }
            1 => {
                let (cx, cy) = (self.cursor_x, self.cursor_y);
                for y in 0..=cy {
                    let end_x = if y == cy { cx } else { self.cols - 1 };
                    for x in 0..=end_x {
                        *self.cell_mut(x, y) = Cell::default();
                    }
                }
            }
            2 | 3 => {
                // 3 would also clear scrollback; we keep it simple for now.
                self.cells.fill(Cell::default());
            }
            _ => {}
        }
    }

    /// EL — erase in line. 0: cursor→end, 1: start→cursor, 2: all.
    pub fn erase_line(&mut self, mode: u16) {
        let y = self.cursor_y;
        match mode {
            0 => {
                for x in self.cursor_x..self.cols {
                    *self.cell_mut(x, y) = Cell::default();
                }
            }
            1 => {
                for x in 0..=self.cursor_x {
                    *self.cell_mut(x, y) = Cell::default();
                }
            }
            2 => {
                for x in 0..self.cols {
                    *self.cell_mut(x, y) = Cell::default();
                }
            }
            _ => {}
        }
    }

    /// Resize, keeping the overlapping top-left content.
    /// TODO: proper reflow (rewrap long lines) like real terminals do.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut cells = vec![Cell::default(); cols * rows];
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                cells[y * cols + x] = self.cells[y * self.cols + x];
            }
        }
        self.cells = cells;
        self.cols = cols;
        self.rows = rows;
        self.cursor_x = self.cursor_x.min(cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(rows.saturating_sub(1));
        self.wrap_pending = false;
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.wrap_pending = false;
    }
}
