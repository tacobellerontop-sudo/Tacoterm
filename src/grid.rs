//! The terminal grid: a rows×cols screen of cells, scrollback, an alternate
//! screen, and the selection.
//!
//! The VT parser writes through this; the renderer reads from it. No
//! rendering code lives here — just the buffer and cursor bookkeeping.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use unicode_width::UnicodeWidthChar;

use crate::graphics::Graphics;

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

// ---- clusters and links -----------------------------------------------------

/// Strings kept once and handed out as small ids, shared by every grid so a
/// renderer's glyph cache can key on them.
#[derive(Default)]
struct Interner {
    list: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Interner {
    fn intern(&mut self, s: &str, max: usize) -> Option<u32> {
        if let Some(&id) = self.ids.get(s) {
            return Some(id);
        }
        if self.list.len() >= max {
            return None;
        }
        let id = self.list.len() as u32;
        self.list.push(s.to_string());
        self.ids.insert(s.to_string(), id);
        Some(id)
    }
}

fn clusters() -> &'static Mutex<Interner> {
    static CLUSTERS: OnceLock<Mutex<Interner>> = OnceLock::new();
    CLUSTERS.get_or_init(Default::default)
}

fn links() -> &'static Mutex<Interner> {
    static LINKS: OnceLock<Mutex<Interner>> = OnceLock::new();
    LINKS.get_or_init(|| {
        // Id 0 means "no link".
        let mut i = Interner::default();
        i.intern("", 1);
        Mutex::new(i)
    })
}

/// A cell holding more than one character (a letter and its accents, an
/// emoji sequence) stores a stand-in from Supplementary Private Use
/// Area-B, numbered into a shared table.
const CLUSTER_BASE: u32 = 0x10_0000;
const MAX_CLUSTERS: usize = 0xfffd;

/// The characters a cell's `c` stands for, if it is a cluster stand-in.
pub fn cluster_text(c: char) -> Option<String> {
    let id = (c as u32).checked_sub(CLUSTER_BASE)? as usize;
    clusters().lock().ok()?.list.get(id).cloned()
}

/// Append what `c` shows to `out`.
pub fn push_cell_text(out: &mut String, c: char) {
    match cluster_text(c) {
        Some(s) => out.push_str(&s),
        None => out.push(c),
    }
}

/// The first character of what `c` shows.
pub fn base_char(c: char) -> char {
    cluster_text(c).and_then(|s| s.chars().next()).unwrap_or(c)
}

fn cluster_char(s: &str) -> Option<char> {
    let id = clusters().lock().ok()?.intern(s, MAX_CLUSTERS)?;
    char::from_u32(CLUSTER_BASE + id)
}

/// The URL of an OSC 8 hyperlink id.
pub fn link_url(id: u16) -> Option<String> {
    if id == 0 {
        return None;
    }
    links().lock().ok()?.list.get(id as usize).cloned()
}

/// The id for an OSC 8 hyperlink to `url` (0 when the table is full).
pub fn link_id(url: &str) -> u16 {
    if url.is_empty() {
        return 0;
    }
    links()
        .lock()
        .ok()
        .and_then(|mut l| l.intern(url, u16::MAX as usize))
        .unwrap_or(0) as u16
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
    /// Blank left in the last column of a wrapped row because the wide
    /// character after it didn't fit. Not part of the text: copying, search
    /// and reflow skip it.
    pub const WIDE_PAD: u16 = 1 << 10;
    /// Underline styles beyond the plain line (SGR 4:2 to 4:5, 21). At most
    /// one is set, along with UNDERLINE.
    pub const DOUBLE_UNDERLINE: u16 = 1 << 11;
    pub const CURLY_UNDERLINE: u16 = 1 << 12;
    pub const DOTTED_UNDERLINE: u16 = 1 << 13;
    pub const DASHED_UNDERLINE: u16 = 1 << 14;
    pub const UNDERLINE_STYLES: u16 = Self::DOUBLE_UNDERLINE
        | Self::CURLY_UNDERLINE
        | Self::DOTTED_UNDERLINE
        | Self::DASHED_UNDERLINE;

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
    /// The character, or a stand-in for several (see [`cluster_text`]).
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attr: Attr,
    /// Underline color (SGR 58); `Default` follows the text color.
    pub ul: Color,
    /// OSC 8 hyperlink (see [`link_url`]); 0 for none.
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attr: Attr::default(),
            ul: Color::Default,
            link: 0,
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
            ul: pen.ul,
            link: pen.link,
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
    pub ul: Color,
    /// The OSC 8 hyperlink being written, 0 for none. Not reset by SGR 0.
    pub link: u16,
}

/// One line of cells. `wrapped` means the line continues on the next one
/// (it was auto-wrapped), so copying joins them without a newline.
#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    /// A shell prompt starts on this row (OSC 133;A).
    pub prompt: bool,
}

impl Row {
    fn new(cols: usize, fill: Cell) -> Self {
        Row {
            cells: vec![fill; cols],
            wrapped: false,
            prompt: false,
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
    /// Which mouse events the program wants reported (?9 / ?1000 / ?1002 / ?1003).
    pub mouse: MouseMode,
    /// ?1006: report mouse events in SGR form (`ESC [ < b ; x ; y M`).
    pub mouse_sgr: bool,
    /// ?1004: send `ESC [ I` / `ESC [ O` when the window gains / loses focus.
    pub focus_events: bool,
    /// ?2026: synchronized output. Tracked only; rendering ignores it.
    pub sync_output: bool,
    /// DECSCUSR (`CSI Ps SP q`): the program's cursor shape and whether it
    /// blinks. `None` means "use the configured style".
    pub cursor_shape: Option<(crate::config::CursorStyle, bool)>,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            autowrap: true,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            mouse: MouseMode::Off,
            mouse_sgr: false,
            focus_events: false,
            sync_output: false,
            cursor_shape: None,
        }
    }
}

/// Mouse tracking level requested by the program. Each level reports
/// everything the one before it does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// No reporting: the mouse belongs to the terminal (selection, scrolling).
    #[default]
    Off,
    /// ?1000 (and X10's ?9): presses, releases and the wheel.
    Click,
    /// ?1002: also motion while a button is held.
    Drag,
    /// ?1003: all motion.
    Motion,
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

/// What a selection grows by while dragging: single cells (click-drag),
/// whole words (double-click) or whole logical lines (triple-click).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionKind {
    #[default]
    Cell,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
    pub kind: SelectionKind,
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

/// A search hit, `start` and `end` inclusive. Both point at real cells:
/// for a wide character that's its first half, never the spacer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub start: Point,
    pub end: Point,
}

/// Whether `p` lies inside one of `matches` (sorted and non-overlapping, as
/// [`Grid::search`] returns them). For the spacer half of a wide character,
/// ask about its first half.
pub fn is_match_cell(matches: &[Match], p: Point) -> bool {
    let i = matches.partition_point(|m| m.start <= p);
    i > 0 && matches[i - 1].end >= p
}

/// Characters a double-click word runs over: letters, digits and the
/// punctuation found in paths and URLs. Spaces, brackets and quotes split.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || "_-.~/\\:@%+#?=&".contains(c)
}

/// Characters that may appear in a URL candidate. Quotes and brackets are
/// allowed here and trimmed from the end later if unbalanced.
fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !"<>`{}|^".contains(c)
        // Box drawing and block elements: TUI borders, never URL text.
        && !('\u{2500}'..='\u{259f}').contains(&c)
}

/// Characters a quick-select candidate runs over: spaces, quotes and
/// brackets split.
fn is_token_char(c: char) -> bool {
    is_url_char(c) && !"\"'()[]".contains(c)
}

/// The part of `token` quick select offers, as a range, if any.
fn quick_target(token: &[char]) -> Option<(usize, usize)> {
    if let Some(k) = (0..token.len()).find(|&k| url_prefix_at(token, k).is_some()) {
        let end = k + trim_url_end(&token[k..]);
        return (end > k + 4).then_some((k, end));
    }
    let mut end = token.len();
    while end > 0 && ".,;:!?".contains(token[end - 1]) {
        end -= 1;
    }
    let t = &token[..end];
    let s: String = t.iter().collect();
    let hex = |c: &char| c.is_ascii_hexdigit();
    let is_hash = (7..=64).contains(&t.len())
        && t.iter().all(hex)
        && t.iter().any(char::is_ascii_digit)
        && t.iter().any(char::is_ascii_alphabetic);
    let is_uuid = t.len() == 36
        && t.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    let is_ip = {
        let addr = s.split(':').next().unwrap_or("");
        let parts: Vec<&str> = addr.split('.').collect();
        parts.len() == 4
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
    };
    let is_number = t.len() >= 4 && t.iter().all(char::is_ascii_digit);
    let is_path = t.len() >= 3
        && t.iter().any(|&c| c == '/' || c == '\\')
        && t.iter().any(|c| c.is_alphanumeric())
        && !t.starts_with(&['-'])
        && t.iter().all(|&c| is_word_char(c) || c == ',');
    (is_hash || is_uuid || is_ip || is_number || is_path).then_some((0, end))
}

/// URL prefixes [`Grid::url_at`] recognizes, matched case-insensitively.
const URL_PREFIXES: [&str; 6] = [
    "https://", "http://", "file://", "ftp://", "mailto:", "www.",
];

/// The length of the URL prefix starting at `run[k]`, if there is one.
fn url_prefix_at(run: &[char], k: usize) -> Option<&'static str> {
    URL_PREFIXES.into_iter().find(|prefix| {
        let n = prefix.chars().count();
        run.len() >= k + n
            && run[k..k + n]
                .iter()
                .zip(prefix.chars())
                .all(|(a, b)| a.to_ascii_lowercase() == b)
            // "www." only at a word start, so "awww.x" isn't a link.
            && (*prefix != "www." || k == 0 || !run[k - 1].is_alphanumeric())
    })
}

/// How much of `url` to keep once trailing sentence punctuation and
/// unbalanced closing brackets/quotes are cut.
fn trim_url_end(url: &[char]) -> usize {
    let mut end = url.len();
    while let Some(&c) = url[..end].last() {
        let count = |x: char| url[..end].iter().filter(|&&y| y == x).count();
        let unbalanced = match c {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => count('(') < count(')'),
            ']' => count('[') < count(']'),
            '\'' | '"' => count(c) % 2 == 1,
            _ => false,
        };
        if !unbalanced {
            break;
        }
        end -= 1;
    }
    end
}

/// The index of the cell at `p` in a [`Grid::logical_cells`] run: the last
/// cell at or before it, so a wide character's spacer maps to its first half.
fn cell_index(cells: &[(Point, char)], p: Point) -> usize {
    cells.partition_point(|&(q, _)| q <= p).saturating_sub(1)
}

/// A cell reflow may drop from the end of a logical line.
fn is_trailing_blank(cell: &Cell) -> bool {
    cell.c == ' ' && cell.bg == Color::Default && !cell.attr.has(Attr::INVERSE | Attr::WIDE_SPACER)
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
    /// Pictures shown in the terminal.
    pub graphics: Graphics,
    /// A cell's size in pixels, for sizing pictures (the app keeps it
    /// current).
    pub cell_pixels: (u32, u32),
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
            graphics: Graphics::default(),
            cell_pixels: (10, 20),
        }
    }

    /// The history line the cursor is on.
    pub fn cursor_line(&self) -> usize {
        self.screen_line(self.cursor_y)
    }

    /// The history line of live screen row `y`.
    fn screen_line(&self, y: usize) -> usize {
        self.lines_dropped + self.scrollback.len() + y
    }

    /// The history line at the top of the view.
    pub fn view_top(&self) -> usize {
        self.lines_dropped + self.scrollback.len() - self.display_offset
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
        if width == 0 || self.joins_previous(c) {
            // Accents, variation selectors, emoji joined with ZWJ, flags:
            // part of the character before.
            self.append_to_previous(c);
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
                let mut pad = self.blank();
                pad.attr.set(Attr::WIDE_PAD, true);
                self.screen[y].cells[self.cursor_x] = pad;
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

    /// The cell the last character went into.
    fn previous_cell(&self) -> Option<(usize, usize)> {
        let y = self.cursor_y;
        let x = if self.wrap_pending {
            self.cursor_x
        } else {
            self.cursor_x.checked_sub(1)?
        };
        let cells = &self.screen[y].cells;
        if cells.get(x)?.attr.has(Attr::WIDE_SPACER) && x > 0 {
            return Some((x - 1, y));
        }
        Some((x, y))
    }

    /// Whether `c` (which has a width) continues the character before: the
    /// next emoji of a ZWJ sequence, or a flag's second letter.
    fn joins_previous(&self, c: char) -> bool {
        let Some((x, y)) = self.previous_cell() else {
            return false;
        };
        let before = self.screen[y].cells[x].c;
        let text = cluster_text(before).unwrap_or_else(|| before.to_string());
        let regional = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
        text.ends_with('\u{200D}') || (regional(c) && text.chars().count() == 1 && regional(before))
    }

    fn append_to_previous(&mut self, c: char) {
        let Some((x, y)) = self.previous_cell() else {
            return;
        };
        let cell = self.screen[y].cells[x];
        let mut text = cluster_text(cell.c).unwrap_or_else(|| cell.c.to_string());
        if text.chars().count() >= 32 {
            return;
        }
        text.push(c);
        let Some(stand_in) = cluster_char(&text) else {
            return;
        };
        self.screen[y].cells[x].c = stand_in;
        // VS16 asks for emoji presentation, which is two cells wide.
        let narrow = !cell.attr.has(Attr::WIDE);
        let emoji_like = base_char(cell.c) as u32 >= 0x2000;
        if c == '\u{FE0F}' && narrow && emoji_like && !self.wrap_pending && x + 1 < self.cols {
            self.screen[y].cells[x].attr.set(Attr::WIDE, true);
            self.clear_wide_at(x + 1, y);
            let mut spacer = Cell::with_pen(' ', &self.pen);
            spacer.attr.set(Attr::WIDE_SPACER, true);
            self.screen[y].cells[x + 1] = spacer;
            if x + 2 >= self.cols {
                self.cursor_x = self.cols - 1;
                self.wrap_pending = self.modes.autowrap;
            } else {
                self.cursor_x = x + 2;
            }
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
        if !to_history {
            // The rows move but keep their line numbers: carry pictures.
            let (first, last) = (self.screen_line(top), self.screen_line(bottom));
            self.shift_images(first, last, -(n as isize));
        }
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
        let (first, last) = (self.screen_line(top), self.screen_line(bottom));
        self.shift_images(first, last, n as isize);
        for _ in 0..n {
            self.screen.remove(bottom);
            let blank = self.blank_row();
            self.screen.insert(top, blank);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_limit == 0 {
            self.lines_dropped += 1;
            self.drop_old_images();
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
            self.drop_old_images();
            if let Some(sel) = self.selection {
                if sel.ordered().0.line < self.lines_dropped {
                    self.selection = None;
                }
            }
        }
        self.display_offset = self.display_offset.min(self.scrollback.len());
    }

    /// Forget pictures that have left the top of the history.
    fn drop_old_images(&mut self) {
        let first = self.lines_dropped;
        self.graphics.placements.retain(|p| p.line + p.rows > first);
    }

    /// IL — insert blank lines at the cursor, inside the scroll region.
    pub fn insert_lines(&mut self, n: usize) {
        let y = self.cursor_y;
        if y < self.scroll_top || y > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - y + 1);
        let (first, last) = (self.screen_line(y), self.screen_line(self.scroll_bottom));
        self.shift_images(first, last, n as isize);
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
        let (first, last) = (self.screen_line(y), self.screen_line(self.scroll_bottom));
        self.clear_images(first, first + n);
        self.shift_images(first, last, -(n as isize));
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
                    row.prompt = false;
                }
                self.selection = None;
                let first = self.screen_line(0);
                self.clear_images(first, usize::MAX);
            }
            3 => {
                let first = self.lines_dropped;
                self.clear_images(first, self.screen_line(0));
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
        self.graphics.parked = Some(std::mem::take(&mut self.graphics.placements));
        self.display_offset = 0;
        self.selection = None;
        self.wrap_pending = false;
    }

    /// ?1049 / ?1047 / ?47 reset: back to the primary screen as it was.
    pub fn exit_alt_screen(&mut self) {
        if let Some((primary, cursor)) = self.parked_primary.take() {
            self.graphics.placements = self.graphics.parked.take().unwrap_or_default();
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
        let first = self.screen_line(0);
        self.clear_images(first, usize::MAX);
        self.move_cursor_to(0, 0);
        self.selection = None;
    }

    /// Resize the screen. On the primary screen a width change reflows
    /// scrollback and screen (see [`Grid::reflow`]). Otherwise rows that no
    /// longer fit above the cursor move to scrollback and columns are cut or
    /// padded — the alternate screen's app repaints on resize anyway.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        if cols != self.cols && !self.is_alt_screen() {
            self.reflow(cols, rows);
        } else {
            self.cut_to(cols, rows);
        }
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.selection = None;
        self.display_offset = self.display_offset.min(self.scrollback.len());
    }

    /// Resize without rewrapping: cut or pad columns, drop lines off the top
    /// until the cursor fits.
    fn cut_to(&mut self, cols: usize, rows: usize) {
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
        self.cursor_x = self.cursor_x.min(cols - 1);
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.wrap_pending = false;
    }

    /// Rewrap scrollback + screen to `cols` columns: rows joined by
    /// `wrapped` become one logical line again and are split at the new
    /// width, with trailing blanks dropped. The cursor keeps its place in
    /// its logical line; blank rows below it are discarded, and whatever
    /// doesn't fit in `rows` goes to scrollback.
    fn reflow(&mut self, cols: usize, rows: usize) {
        let cursor_row = self.scrollback.len() + self.cursor_y;
        let old: Vec<Row> = self
            .scrollback
            .drain(..)
            .chain(self.screen.drain(..))
            .collect();
        let mut out: Vec<Row> = Vec::new();
        // (row in `out`, col, wrap pending)
        let mut cursor = (0, 0, false);
        // Old row index -> new row index.
        let mut moved = vec![0; old.len()];
        let mut i = 0;
        while i < old.len() {
            let (first_old, first_new, prompt) = (i, out.len(), old[i].prompt);
            // Join one logical line, noting where the cursor falls in it.
            let mut cells: Vec<Cell> = Vec::new();
            let mut cursor_at = None;
            loop {
                let row = &old[i];
                for (x, cell) in row.cells.iter().enumerate() {
                    if i == cursor_row && x == self.cursor_x {
                        cursor_at = Some(cells.len() + usize::from(self.wrap_pending));
                    }
                    if !(row.wrapped && cell.attr.has(Attr::WIDE_PAD)) {
                        cells.push(*cell);
                    }
                }
                i += 1;
                if !row.wrapped || i == old.len() {
                    break;
                }
            }
            let content = cells.len()
                - cells
                    .iter()
                    .rev()
                    .take_while(|c| is_trailing_blank(c))
                    .count();
            let n = content.max(cursor_at.unwrap_or(0).min(cells.len()));

            // Split it at the new width.
            let mut row: Vec<Cell> = Vec::with_capacity(cols);
            let mut k = 0;
            while k < n {
                let mut cell = cells[k];
                let mut width = 1;
                if cell.attr.has(Attr::WIDE) && cols >= 2 {
                    width = 2;
                } else if cell.attr.has(Attr::WIDE | Attr::WIDE_SPACER) {
                    // An orphaned half, or a wide char in a 1-column grid.
                    cell.c = ' ';
                    cell.attr.set(Attr::WIDE | Attr::WIDE_SPACER, false);
                }
                if row.len() + width > cols {
                    if row.len() < cols {
                        let mut pad = Cell::default();
                        pad.attr.set(Attr::WIDE_PAD, true);
                        row.push(pad);
                    }
                    out.push(Row {
                        cells: std::mem::replace(&mut row, Vec::with_capacity(cols)),
                        wrapped: true,
                        prompt: false,
                    });
                }
                if cursor_at == Some(k) {
                    cursor = (out.len(), row.len(), false);
                }
                row.push(cell);
                k += 1;
                if width == 2 {
                    if cursor_at == Some(k) {
                        cursor = (out.len(), row.len(), false);
                    }
                    let mut spacer = cells
                        .get(k)
                        .copied()
                        .filter(|c| c.attr.has(Attr::WIDE_SPACER));
                    if spacer.is_some() {
                        k += 1;
                    }
                    row.push(*spacer.get_or_insert_with(|| {
                        let mut s = Cell { c: ' ', ..cell };
                        s.attr.set(Attr::WIDE, false);
                        s.attr.set(Attr::WIDE_SPACER, true);
                        s
                    }));
                }
            }
            if cursor_at == Some(n) {
                // Right after a full row: deferred wrap, as if just typed.
                cursor = if row.len() < cols {
                    (out.len(), row.len(), false)
                } else {
                    (out.len(), cols - 1, true)
                };
            }
            out.push(Row {
                cells: row,
                wrapped: false,
                prompt: false,
            });
            // Where this logical line's rows went, for prompts and pictures.
            out[first_new].prompt = prompt;
            let made = out.len() - first_new;
            for (k, to) in moved[first_old..i].iter_mut().enumerate() {
                *to = first_new + k.min(made - 1);
            }
        }
        for row in &mut out {
            row.cells.resize(cols, Cell::default());
        }
        let blank_row = |r: &Row| !r.wrapped && r.cells.iter().all(is_trailing_blank);
        while out.len() > cursor.0 + 1 && out.last().is_some_and(blank_row) {
            out.pop();
        }

        let start = out.len().saturating_sub(rows).min(cursor.0);
        let mut screen = out.split_off(start);
        screen.truncate(rows);
        screen.resize(rows, Row::new(cols, Cell::default()));
        let excess = out.len().saturating_sub(self.scrollback_limit);
        // History lines keep counting from the same place, so a picture's
        // line maps through `moved`.
        let first = self.lines_dropped;
        for p in &mut self.graphics.placements {
            if let Some(&to) = p.line.checked_sub(first).and_then(|k| moved.get(k)) {
                p.line = first + to;
            }
        }
        self.lines_dropped += excess;
        self.drop_old_images();
        self.scrollback = out.into_iter().skip(excess).collect();
        self.screen = screen;
        self.cursor_x = cursor.1;
        self.cursor_y = cursor.0 - start;
        self.wrap_pending = cursor.2;
    }

    // ---- shell integration ----------------------------------------------

    /// OSC 133;A: a prompt starts on the cursor's row.
    pub fn mark_prompt(&mut self) {
        let y = self.cursor_y;
        self.screen[y].prompt = true;
    }

    /// History lines where prompts start, oldest first.
    pub fn prompt_lines(&self) -> Vec<usize> {
        self.scrollback
            .iter()
            .chain(self.screen.iter())
            .enumerate()
            .filter(|(_, r)| r.prompt)
            .map(|(i, _)| self.lines_dropped + i)
            .collect()
    }

    /// Scroll so the previous (`up`) or next prompt is at the top of the
    /// view. Returns false when there is none that way.
    pub fn scroll_to_prompt(&mut self, up: bool) -> bool {
        let top = self.view_top();
        let lines = self.prompt_lines();
        let target = if up {
            lines.into_iter().rev().find(|&l| l < top)
        } else {
            lines.into_iter().find(|&l| l > top)
        };
        let Some(line) = target else {
            if !up {
                self.display_offset = 0;
            }
            return false;
        };
        let end = self.lines_dropped + self.scrollback.len();
        self.display_offset = end.saturating_sub(line).min(self.scrollback.len());
        true
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

    /// Make history line `p.line` visible, centering it if it was off-view.
    pub fn scroll_to_point(&mut self, p: Point) {
        if self.point_in_viewport(Point { col: 0, ..p }).is_some() {
            return;
        }
        let idx = p.line.saturating_sub(self.lines_dropped);
        let top = idx.saturating_sub(self.rows / 2).min(self.scrollback.len());
        self.display_offset = self.scrollback.len() - top;
    }

    /// Where history point `p` is in the viewport as (col, row), if it's
    /// on screen.
    pub fn point_in_viewport(&self, p: Point) -> Option<(usize, usize)> {
        let top = self.lines_dropped + self.scrollback.len() - self.display_offset;
        let y = p.line.checked_sub(top)?;
        (y < self.rows && p.col < self.cols).then_some((p.col, y))
    }

    /// One past the last history line (the first is `lines_dropped`).
    fn history_end(&self) -> usize {
        self.lines_dropped + self.scrollback.len() + self.screen.len()
    }

    /// The first and last line of the logical line (rows joined by
    /// `wrapped`) holding `line`, which must exist.
    fn logical_bounds(&self, line: usize) -> (usize, usize) {
        let (first, end) = (self.lines_dropped, self.history_end());
        let wraps = |l: usize| self.row_at(l).is_some_and(|r| r.wrapped);
        let mut start = line;
        while start > first && wraps(start - 1) {
            start -= 1;
        }
        let mut last = line;
        while last + 1 < end && wraps(last) {
            last += 1;
        }
        (start, last)
    }

    /// The text of lines `first..=last` as (position, char) pairs, skipping
    /// wide-char spacers and wrap padding.
    fn logical_cells(&self, first: usize, last: usize) -> Vec<(Point, char)> {
        let mut out = Vec::new();
        for line in first..=last {
            let Some(row) = self.row_at(line) else {
                continue;
            };
            for (col, cell) in row.cells.iter().enumerate() {
                let pad = row.wrapped && cell.attr.has(Attr::WIDE_PAD);
                if !pad && !cell.attr.has(Attr::WIDE_SPACER) {
                    out.push((Point { line, col }, base_char(cell.c)));
                }
            }
        }
        out
    }

    /// The first and last cell of the `kind` unit at `p`: the cell itself,
    /// its word (or just it, if it's a separator), or its logical line.
    fn unit_at(&self, kind: SelectionKind, p: Point) -> (Point, Point) {
        if kind == SelectionKind::Cell || self.row_at(p.line).is_none() {
            return (p, p);
        }
        let (first, last) = self.logical_bounds(p.line);
        if kind == SelectionKind::Line {
            let end_col = self
                .row_at(last)
                .map_or(0, |r| r.cells.len().saturating_sub(1));
            return (
                Point {
                    line: first,
                    col: 0,
                },
                Point {
                    line: last,
                    col: end_col,
                },
            );
        }
        let cells = self.logical_cells(first, last);
        if cells.is_empty() {
            return (p, p);
        }
        let i = cell_index(&cells, p);
        if !is_word_char(cells[i].1) {
            return (cells[i].0, cells[i].0);
        }
        let mut a = i;
        while a > 0 && is_word_char(cells[a - 1].1) {
            a -= 1;
        }
        let mut b = i;
        while b + 1 < cells.len() && is_word_char(cells[b + 1].1) {
            b += 1;
        }
        (cells[a].0, cells[b].0)
    }

    /// Start a click-drag selection at `p`.
    pub fn start_selection(&mut self, p: Point) {
        self.select_unit(SelectionKind::Cell, p);
    }

    /// Double-click: select the word at `p`.
    pub fn select_word(&mut self, p: Point) {
        self.select_unit(SelectionKind::Word, p);
    }

    /// Triple-click: select the logical line at `p`, wrapped rows included.
    pub fn select_line(&mut self, p: Point) {
        self.select_unit(SelectionKind::Line, p);
    }

    fn select_unit(&mut self, kind: SelectionKind, p: Point) {
        let (anchor, head) = self.unit_at(kind, p);
        self.selection = Some(Selection { anchor, head, kind });
    }

    /// Select all of scrollback and the screen.
    pub fn select_all(&mut self) {
        let last = self.history_end() - 1;
        let col = self
            .row_at(last)
            .map_or(0, |r| r.cells.len().saturating_sub(1));
        self.selection = Some(Selection {
            anchor: Point {
                line: self.lines_dropped,
                col: 0,
            },
            head: Point { line: last, col },
            kind: SelectionKind::Cell,
        });
    }

    /// Drag the selection's head to `p`. Word and line selections snap to
    /// whole units and always keep the unit they started on.
    pub fn extend_selection(&mut self, p: Point) {
        let Some(sel) = self.selection else {
            return;
        };
        // The anchor always sits inside the starting unit (its first or
        // last cell), so it can be found again from there.
        let (us, ue) = self.unit_at(sel.kind, sel.anchor);
        let (hs, he) = self.unit_at(sel.kind, p);
        let (anchor, head) = if p < us { (ue, hs) } else { (us, he) };
        self.selection = Some(Selection {
            anchor,
            head,
            ..sel
        });
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
                .filter(|c| !(row.wrapped && c.attr.has(Attr::WIDE_PAD)))
                .fold(String::new(), |mut s, c| {
                    push_cell_text(&mut s, c.c);
                    s
                });
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

    // ---- search + links -------------------------------------------------

    /// Every occurrence of `needle` in scrollback and screen, oldest first.
    /// Matches run across wrapped rows but not across real line breaks;
    /// wide characters count once.
    pub fn search(&self, needle: &str, case_sensitive: bool) -> Vec<Match> {
        let fold = |c: char| {
            if case_sensitive {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        };
        let needle: Vec<char> = needle.chars().map(fold).collect();
        let mut matches = Vec::new();
        if needle.is_empty() {
            return matches;
        }
        let end = self.history_end();
        let mut line = self.lines_dropped;
        while line < end {
            let (first, last) = self.logical_bounds(line);
            let cells = self.logical_cells(first, last);
            let hay: Vec<char> = cells.iter().map(|&(_, c)| fold(c)).collect();
            let mut i = 0;
            while i + needle.len() <= hay.len() {
                if hay[i..i + needle.len()] == needle[..] {
                    matches.push(Match {
                        start: cells[i].0,
                        end: cells[i + needle.len() - 1].0,
                    });
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
            line = last + 1;
        }
        matches
    }

    /// The OSC 8 hyperlink at `p`, or else a URL written out in the text:
    /// the link and its first and last cell.
    pub fn link_at(&self, p: Point) -> Option<(String, Point, Point)> {
        let row = self.row_at(p.line)?;
        let id = row.cells.get(p.col)?.link;
        if id == 0 {
            return self.url_at(p);
        }
        let url = link_url(id)?;
        let same = |c: &Cell| c.link == id;
        let start = row.cells[..p.col]
            .iter()
            .rev()
            .take_while(|c| same(c))
            .count();
        let end = row.cells[p.col..].iter().take_while(|c| same(c)).count();
        Some((
            url,
            Point {
                line: p.line,
                col: p.col - start,
            },
            Point {
                line: p.line,
                col: p.col + end - 1,
            },
        ))
    }

    /// Things on screen worth copying, top to bottom, for quick select:
    /// URLs, file paths, git hashes, UUIDs, IP addresses and numbers of
    /// four or more digits, each with its text.
    pub fn quick_targets(&self) -> Vec<(Match, String)> {
        let top = self.viewport_point(0, 0).line;
        let bottom = (top + self.rows).min(self.history_end());
        let mut out = Vec::new();
        let mut line = top;
        while line < bottom {
            let (first, last) = self.logical_bounds(line);
            let cells = self.logical_cells(first, last);
            let mut i = 0;
            while i < cells.len() {
                if !is_token_char(cells[i].1) {
                    i += 1;
                    continue;
                }
                let j = i + cells[i..]
                    .iter()
                    .take_while(|&&(_, c)| is_token_char(c))
                    .count();
                let token: Vec<char> = cells[i..j].iter().map(|&(_, c)| c).collect();
                if let Some((a, b)) = quick_target(&token) {
                    let (start, end) = (cells[i + a].0, cells[i + b - 1].0);
                    if start.line < bottom && end.line >= top {
                        out.push((Match { start, end }, token[a..b].iter().collect()));
                    }
                }
                i = j;
            }
            line = last + 1;
        }
        out
    }

    /// The URL under `p` with its first and last cell (inclusive), if any.
    /// Knows `http(s)://`, `file://`, `ftp://`, `mailto:` and bare `www.`
    /// (returned with `https://` in front); follows wrapped rows and drops
    /// trailing punctuation and unbalanced closing brackets or quotes.
    pub fn url_at(&self, p: Point) -> Option<(String, Point, Point)> {
        self.row_at(p.line)?;
        let (first, last) = self.logical_bounds(p.line);
        let cells = self.logical_cells(first, last);
        if cells.is_empty() {
            return None;
        }
        let i = cell_index(&cells, p);
        if !is_url_char(cells[i].1) {
            return None;
        }
        let mut a = i;
        while a > 0 && is_url_char(cells[a - 1].1) {
            a -= 1;
        }
        let mut b = i + 1;
        while b < cells.len() && is_url_char(cells[b].1) {
            b += 1;
        }
        let run: Vec<char> = cells[a..b].iter().map(|&(_, c)| c).collect();
        let at = i - a;
        // The first URL in the run that covers `p`, like a left-to-right
        // scan would find it.
        (0..=at).find_map(|k| {
            let prefix = url_prefix_at(&run, k)?;
            let end = k + trim_url_end(&run[k..]);
            if end <= k + prefix.len() || end <= at {
                return None;
            }
            let mut url: String = run[k..end].iter().collect();
            if prefix == "www." {
                url.insert_str(0, "https://");
            }
            Some((url, cells[a + k].0, cells[a + end - 1].0))
        })
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

    fn lines(g: &mut Grid, lines: &[&str]) {
        for (i, s) in lines.iter().enumerate() {
            if i > 0 {
                g.carriage_return();
                g.line_feed();
            }
            type_str(g, s);
        }
    }

    #[test]
    fn word_selection_runs_over_path_chars() {
        let mut g = Grid::new(30, 2, 100);
        type_str(&mut g, "ls ./src/grid.rs (x)  y");
        g.select_word(g.viewport_point(10, 0));
        assert_eq!(g.selection_text().unwrap(), "./src/grid.rs");
        g.select_word(g.viewport_point(17, 0));
        assert_eq!(g.selection_text().unwrap(), "(", "separator selects itself");
        g.select_word(g.viewport_point(21, 0));
        assert_eq!(g.selection_text().unwrap(), "");
        assert_eq!(g.selection.unwrap().kind, SelectionKind::Word);
    }

    #[test]
    fn word_selection_follows_wrapped_rows() {
        let mut g = Grid::new(4, 3, 100);
        type_str(&mut g, "ab cdefg h");
        g.select_word(g.viewport_point(0, 1));
        assert_eq!(g.selection_text().unwrap(), "cdefg");
    }

    #[test]
    fn word_drag_snaps_and_keeps_anchor_word() {
        let mut g = Grid::new(20, 2, 100);
        type_str(&mut g, "one two three");
        g.select_word(g.viewport_point(5, 0));
        g.extend_selection(g.viewport_point(9, 0));
        assert_eq!(g.selection_text().unwrap(), "two three");
        g.extend_selection(g.viewport_point(1, 0));
        assert_eq!(g.selection_text().unwrap(), "one two");
    }

    #[test]
    fn line_selection_takes_whole_logical_line() {
        let mut g = Grid::new(4, 4, 100);
        lines(&mut g, &["abcdef", "gh", "ij"]);
        g.select_line(g.viewport_point(2, 1));
        assert_eq!(g.selection_text().unwrap(), "abcdef");
        g.extend_selection(g.viewport_point(0, 2));
        assert_eq!(g.selection_text().unwrap(), "abcdef\ngh");
    }

    #[test]
    fn select_all_covers_scrollback() {
        let mut g = Grid::new(5, 2, 100);
        lines(&mut g, &["one", "two", "three"]);
        g.select_all();
        assert_eq!(g.selection_text().unwrap(), "one\ntwo\nthree");
    }

    #[test]
    fn search_across_wraps_and_wide_chars() {
        let mut g = Grid::new(4, 4, 100);
        lines(&mut g, &["xFOOba", "漢foo"]);
        let m = g.search("foo", false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].start, g.viewport_point(1, 0));
        assert_eq!(m[1].start, g.viewport_point(2, 2), "wide char counts once");
        assert_eq!(m[1].end, g.viewport_point(0, 3));
        assert_eq!(
            g.search("ab漢", true).len(),
            0,
            "no match over a real newline"
        );
        assert_eq!(g.search("oba", false)[0].end, g.viewport_point(1, 1));
        assert_eq!(g.search("漢f", true)[0].end, g.viewport_point(2, 2));
        assert!(g.search("FOO", true).len() == 1 && g.search("", false).is_empty());
        assert!(is_match_cell(&m, g.viewport_point(2, 0)));
        assert!(!is_match_cell(&m, g.viewport_point(0, 0)));
    }

    #[test]
    fn scroll_to_point_brings_line_into_view() {
        let mut g = Grid::new(4, 2, 100);
        for i in 0..10 {
            lines(&mut g, &[&i.to_string(), ""]);
        }
        let p = g.search("2", true)[0].start;
        assert_eq!(g.point_in_viewport(p), None);
        g.scroll_to_point(p);
        let (col, y) = g.point_in_viewport(p).unwrap();
        assert_eq!(g.visible_row(y).cells[col].c, '2');
        let offset = g.display_offset;
        g.scroll_to_point(p);
        assert_eq!(g.display_offset, offset, "already visible: no move");
    }

    #[test]
    fn url_detection() {
        let mut g = Grid::new(40, 3, 100);
        lines(
            &mut g,
            &[
                "see (https://x.org/a_(b)), ok",
                "<www.y.com/p?q=1.>",
                "mailto:me@z.io",
            ],
        );
        let (url, start, end) = g.url_at(g.viewport_point(10, 0)).unwrap();
        assert_eq!(url, "https://x.org/a_(b)");
        assert_eq!((start.col, end.col), (5, 23));
        assert_eq!(
            g.url_at(g.viewport_point(5, 1)).unwrap().0,
            "https://www.y.com/p?q=1"
        );
        assert_eq!(
            g.url_at(g.viewport_point(0, 2)).unwrap().0,
            "mailto:me@z.io"
        );
        assert_eq!(g.url_at(g.viewport_point(1, 0)), None);
        assert_eq!(
            g.url_at(g.viewport_point(26, 0)),
            None,
            "past the trimmed end"
        );
    }

    #[test]
    fn url_follows_wrapped_rows() {
        let mut g = Grid::new(8, 3, 100);
        type_str(&mut g, "'http://a.b/cd' x");
        let (url, start, end) = g.url_at(g.viewport_point(2, 1)).unwrap();
        assert_eq!(url, "http://a.b/cd");
        assert_eq!(start, g.viewport_point(1, 0));
        assert_eq!(end, g.viewport_point(5, 1));
    }

    #[test]
    fn reflow_narrower_then_wider_round_trips() {
        let mut g = Grid::new(6, 2, 100);
        type_str(&mut g, "abcdefgh");
        g.resize(3, 2);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("def".into(), "gh".into())
        );
        assert_eq!((g.cursor_x, g.cursor_y), (2, 1));
        g.resize(6, 2);
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("abcdef".into(), "gh".into())
        );
        assert!(g.visible_row(0).wrapped && !g.visible_row(1).wrapped);
        assert_eq!((g.cursor_x, g.cursor_y), (2, 1));
        type_str(&mut g, "i");
        assert_eq!(row_text(&g, 1), "ghi");
    }

    #[test]
    fn reflow_keeps_cursor_after_last_char() {
        let mut g = Grid::new(4, 3, 100);
        type_str(&mut g, "abcd");
        assert!(g.wrap_pending);
        g.resize(8, 3);
        assert_eq!((g.cursor_x, g.cursor_y, g.wrap_pending), (4, 0, false));
        g.resize(4, 3);
        assert_eq!((g.cursor_x, g.cursor_y, g.wrap_pending), (3, 0, true));
        type_str(&mut g, "e");
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("abcd".into(), "e".into())
        );
        g.resize(2, 3);
        assert_eq!(row_text(&g, 2), "e");
        assert_eq!((g.cursor_x, g.cursor_y), (1, 2));
    }

    #[test]
    fn reflow_trailing_blanks_dont_wrap() {
        let mut g = Grid::new(8, 3, 100);
        lines(&mut g, &["ab", "cd"]);
        g.resize(4, 3);
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("ab".into(), "cd".into())
        );
        assert!(!g.visible_row(0).wrapped);
        assert_eq!((g.cursor_x, g.cursor_y), (2, 1));
    }

    #[test]
    fn reflow_never_splits_wide_chars() {
        let mut g = Grid::new(5, 2, 100);
        type_str(&mut g, "ab漢c");
        g.resize(3, 2);
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("ab".into(), "漢c".into())
        );
        assert!(g.cell(2, 0).attr.has(Attr::WIDE_PAD));
        g.select_all();
        assert_eq!(g.selection_text().unwrap(), "ab漢c", "pad isn't text");
        g.resize(5, 2);
        assert_eq!(row_text(&g, 0), "ab漢c");
        assert!(g.cell(2, 0).attr.has(Attr::WIDE) && g.cell(3, 0).attr.has(Attr::WIDE_SPACER));
        assert_eq!((g.cursor_x, g.cursor_y, g.wrap_pending), (4, 0, true));
    }

    #[test]
    fn reflow_respects_scrollback_limit() {
        let mut g = Grid::new(4, 1, 1);
        type_str(&mut g, "abcdefgh");
        g.resize(2, 1);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(row_text(&g, 0), "gh");
        assert_eq!(
            g.viewport_point(0, 0).line,
            3,
            "dropped lines keep counting"
        );
    }

    #[test]
    fn alt_screen_resize_cuts_instead_of_reflowing() {
        let mut g = Grid::new(6, 2, 100);
        g.enter_alt_screen();
        type_str(&mut g, "abcdef");
        g.resize(3, 2);
        assert_eq!(
            (row_text(&g, 0), row_text(&g, 1)),
            ("abc".into(), "".into())
        );
    }

    #[test]
    fn quick_select_finds_things_to_copy() {
        let mut g = Grid::new(80, 3, 100);
        type_str(
            &mut g,
            "see https://example.com/a. at C:\\src\\main.rs:12, 1a2b3c4d ok",
        );
        g.carriage_return();
        g.line_feed();
        type_str(&mut g, "ip 10.0.0.1:80 pid 31337 (./run.sh) word 12");
        let texts: Vec<String> = g.quick_targets().into_iter().map(|(_, t)| t).collect();
        assert_eq!(
            texts,
            [
                "https://example.com/a",
                "C:\\src\\main.rs:12",
                "1a2b3c4d",
                "10.0.0.1:80",
                "31337",
                "./run.sh"
            ]
        );
    }
}
