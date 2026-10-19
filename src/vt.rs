//! Feeds raw PTY bytes through `vte` into the [`Grid`].
//!
//! Implements what ConPTY and the programs behind it actually emit:
//! printing, C0 controls, cursor movement and save/restore, erasing,
//! insert/delete, scroll regions, the alternate screen, the common private
//! modes, device status reports, window titles, and SGR colors (16 ANSI +
//! 256 + truecolor). Everything else is ignored rather than misrendered —
//! extend as needed.

use vte::{Params, Perform};

use crate::grid::{Attr, Color, Grid, Pen};

pub struct Vt {
    parser: vte::Parser,
    /// Bytes the terminal must send back to the shell (replies to status
    /// queries). The app drains this into the PTY after each `advance`.
    pub responses: Vec<u8>,
    /// Set when the program changed the window title (OSC 0 / 2).
    pub title: Option<String>,
}

impl Default for Vt {
    fn default() -> Self {
        Self::new()
    }
}

impl Vt {
    pub fn new() -> Self {
        Vt {
            parser: vte::Parser::new(),
            responses: Vec::new(),
            title: None,
        }
    }

    /// Parse raw PTY output. UTF-8 sequences split across calls are fine:
    /// the parser keeps its state between chunks.
    pub fn advance(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let mut performer = Performer {
            grid,
            responses: &mut self.responses,
            title: &mut self.title,
        };
        self.parser.advance(&mut performer, bytes);
    }
}

struct Performer<'a> {
    grid: &'a mut Grid,
    responses: &'a mut Vec<u8>,
    title: &'a mut Option<String>,
}

/// params[i], or `default` when missing or 0 (VT counts treat 0 as 1).
fn count(params: &Params, i: usize, default: u16) -> usize {
    match params.iter().nth(i).and_then(|g| g.first().copied()) {
        None | Some(0) => default as usize,
        Some(n) => n as usize,
    }
}

/// params[i] verbatim, or `default` when missing.
fn param(params: &Params, i: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(i)
        .and_then(|g| g.first().copied())
        .unwrap_or(default)
}

impl Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0a..=0x0c => self.grid.line_feed(),
            0x0d => self.grid.carriage_return(),
            0x08 => self.grid.backspace(),
            0x09 => self.grid.tab(),
            0x07 => {} // bell: flash the window eventually; ignore for now
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 2: window title. TODO: OSC 52 (clipboard), OSC 8 (links).
        if let [kind, rest @ ..] = params {
            if (*kind == b"0" || *kind == b"2") && !rest.is_empty() {
                let title = rest
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(";");
                *self.title = Some(title);
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let private = intermediates.first() == Some(&b'?');
        let g = &mut *self.grid;
        match (action, private) {
            ('A', false) => g.move_cursor_by(0, -(count(params, 0, 1) as isize)),
            ('B' | 'e', false) => g.move_cursor_by(0, count(params, 0, 1) as isize),
            ('C' | 'a', false) => g.move_cursor_by(count(params, 0, 1) as isize, 0),
            ('D', false) => g.move_cursor_by(-(count(params, 0, 1) as isize), 0),
            ('E', false) => {
                g.move_cursor_by(0, count(params, 0, 1) as isize);
                g.carriage_return();
            }
            ('F', false) => {
                g.move_cursor_by(0, -(count(params, 0, 1) as isize));
                g.carriage_return();
            }
            ('G' | '`', false) => {
                let y = g.cursor_y;
                g.move_cursor_to(count(params, 0, 1) - 1, y);
            }
            ('d', false) => {
                let x = g.cursor_x;
                g.move_cursor_to(x, count(params, 0, 1) - 1);
            }
            ('H' | 'f', false) => {
                g.move_cursor_to(count(params, 1, 1) - 1, count(params, 0, 1) - 1);
            }
            ('J', _) => g.erase_display(param(params, 0, 0)),
            ('K', _) => g.erase_line(param(params, 0, 0)),
            ('L', false) => g.insert_lines(count(params, 0, 1)),
            ('M', false) => g.delete_lines(count(params, 0, 1)),
            ('@', false) => g.insert_chars(count(params, 0, 1)),
            ('P', false) => g.delete_chars(count(params, 0, 1)),
            ('X', false) => g.erase_chars(count(params, 0, 1)),
            ('S', false) => g.scroll_up(count(params, 0, 1)),
            ('T', false) => g.scroll_down(count(params, 0, 1)),
            ('r', false) => {
                let top = count(params, 0, 1) - 1;
                let bottom = count(params, 1, g.rows as u16) - 1;
                g.set_scroll_region(top, bottom);
            }
            ('s', false) => g.save_cursor(),
            ('u', false) => g.restore_cursor(),
            ('m', false) => sgr(g, params),
            ('h', true) | ('l', true) => {
                let on = action == 'h';
                for group in params.iter() {
                    for &mode in group {
                        set_private_mode(g, mode, on);
                    }
                }
            }
            ('n', false) => match param(params, 0, 0) {
                // Device status: "OK".
                5 => self.responses.extend_from_slice(b"\x1b[0n"),
                // Cursor position report.
                6 => {
                    let reply = format!("\x1b[{};{}R", g.cursor_y + 1, g.cursor_x + 1);
                    self.responses.extend_from_slice(reply.as_bytes());
                }
                _ => {}
            },
            // Primary device attributes: "VT100 with advanced video".
            ('c', false) if intermediates.is_empty() => {
                self.responses.extend_from_slice(b"\x1b[?1;2c")
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            // Charset designations (ESC ( B …) and friends: ignore.
            return;
        }
        match byte {
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'D' => self.grid.line_feed(),
            b'E' => {
                self.grid.carriage_return();
                self.grid.line_feed();
            }
            b'M' => self.grid.reverse_index(),
            b'c' => self.grid.reset(),
            _ => {}
        }
    }
}

fn set_private_mode(g: &mut Grid, mode: u16, on: bool) {
    match mode {
        1 => g.modes.app_cursor = on,
        7 => g.modes.autowrap = on,
        25 => g.modes.cursor_visible = on,
        47 | 1047 => {
            if on {
                g.enter_alt_screen()
            } else {
                g.exit_alt_screen()
            }
        }
        1049 => {
            if on {
                g.save_cursor();
                g.enter_alt_screen();
            } else {
                g.exit_alt_screen();
                g.restore_cursor();
            }
        }
        2004 => g.modes.bracketed_paste = on,
        _ => {}
    }
}

/// SGR — select graphic rendition.
fn sgr(g: &mut Grid, params: &Params) {
    let groups: Vec<&[u16]> = params.iter().collect();
    if groups.is_empty() {
        g.pen = Pen::default();
        return;
    }
    let mut i = 0;
    while i < groups.len() {
        let n = groups[i].first().copied().unwrap_or(0);
        let attr = &mut g.pen.attr;
        match n {
            0 => g.pen = Pen::default(),
            1 => attr.set(Attr::BOLD, true),
            2 => attr.set(Attr::DIM, true),
            3 => attr.set(Attr::ITALIC, true),
            4 => attr.set(Attr::UNDERLINE, true),
            7 => attr.set(Attr::INVERSE, true),
            8 => attr.set(Attr::HIDDEN, true),
            9 => attr.set(Attr::STRIKE, true),
            21 => attr.set(Attr::UNDERLINE, true),
            22 => {
                attr.set(Attr::BOLD, false);
                attr.set(Attr::DIM, false);
            }
            23 => attr.set(Attr::ITALIC, false),
            24 => attr.set(Attr::UNDERLINE, false),
            27 => attr.set(Attr::INVERSE, false),
            28 => attr.set(Attr::HIDDEN, false),
            29 => attr.set(Attr::STRIKE, false),
            30..=37 => g.pen.fg = Color::Indexed((n - 30) as u8),
            39 => g.pen.fg = Color::Default,
            40..=47 => g.pen.bg = Color::Indexed((n - 40) as u8),
            49 => g.pen.bg = Color::Default,
            90..=97 => g.pen.fg = Color::Indexed((n - 90 + 8) as u8),
            100..=107 => g.pen.bg = Color::Indexed((n - 100 + 8) as u8),
            38 | 48 => {
                let (color, used) = extended_color(&groups[i..]);
                if let Some(color) = color {
                    if n == 38 {
                        g.pen.fg = color;
                    } else {
                        g.pen.bg = color;
                    }
                }
                i += used;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

/// Parse an extended color starting at `groups[0]` (which holds 38 or 48).
/// Two spellings:
///   colon form:     `38:2:{r}:{g}:{b}` / `38:2::{r}:{g}:{b}` / `38:5:{idx}`
///   semicolon form: `38;2;{r};{g};{b}` / `38;5;{idx}`
/// Returns the color and how many groups it consumed.
fn extended_color(groups: &[&[u16]]) -> (Option<Color>, usize) {
    let first = groups[0];
    if first.len() > 1 {
        let color = match first[1] {
            // With 6 values the third is a colorspace id; skip it.
            2 if first.len() >= 6 => Some(rgb(first[3], first[4], first[5])),
            2 if first.len() == 5 => Some(rgb(first[2], first[3], first[4])),
            5 if first.len() >= 3 => Some(Color::Indexed(first[2].min(255) as u8)),
            _ => None,
        };
        return (color, 1);
    }
    let at = |k: usize| groups.get(k).and_then(|g| g.first().copied());
    match at(1) {
        Some(2) => match (at(2), at(3), at(4)) {
            (Some(r), Some(g), Some(b)) => (Some(rgb(r, g, b)), 5),
            _ => (None, groups.len()),
        },
        Some(5) => match at(2) {
            Some(idx) => (Some(Color::Indexed(idx.min(255) as u8)), 3),
            None => (None, groups.len()),
        },
        _ => (None, 1),
    }
}

fn rgb(r: u16, g: u16, b: u16) -> Color {
    Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(g: &mut Grid, vt: &mut Vt, s: &str) {
        vt.advance(g, s.as_bytes());
    }

    fn row_text(g: &Grid, y: usize) -> String {
        (0..g.cols)
            .map(|x| g.cell(x, y).c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn cursor_moves_and_erase() {
        let mut g = Grid::new(10, 3, 0);
        let mut vt = Vt::new();
        feed(&mut g, &mut vt, "hello\x1b[2;3Hxy\x1b[1;1H\x1b[K");
        assert_eq!(row_text(&g, 0), "");
        assert_eq!(row_text(&g, 1), "  xy");
    }

    #[test]
    fn sgr_colors() {
        let mut g = Grid::new(10, 1, 0);
        let mut vt = Vt::new();
        feed(
            &mut g,
            &mut vt,
            "\x1b[1;31ma\x1b[38;2;1;2;3mb\x1b[48:5:200mc\x1b[38:2::9:8:7md\x1b[0me",
        );
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(1));
        assert!(g.cell(0, 0).attr.has(Attr::BOLD));
        assert_eq!(g.cell(1, 0).fg, Color::Rgb(1, 2, 3));
        assert_eq!(g.cell(2, 0).bg, Color::Indexed(200));
        assert_eq!(g.cell(3, 0).fg, Color::Rgb(9, 8, 7));
        assert_eq!(g.cell(4, 0).fg, Color::Default);
        assert!(!g.cell(4, 0).attr.has(Attr::BOLD));
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut g = Grid::new(10, 1, 0);
        let mut vt = Vt::new();
        let bytes = "é漢".as_bytes();
        for b in bytes {
            vt.advance(&mut g, std::slice::from_ref(b));
        }
        assert_eq!(g.cell(0, 0).c, 'é');
        assert_eq!(g.cell(1, 0).c, '漢');
    }

    #[test]
    fn status_reports_and_title() {
        let mut g = Grid::new(10, 5, 0);
        let mut vt = Vt::new();
        feed(&mut g, &mut vt, "\x1b[3;4H\x1b[6n\x1b]0;my shell\x07");
        assert_eq!(vt.responses, b"\x1b[3;4R");
        assert_eq!(vt.title.as_deref(), Some("my shell"));
    }

    #[test]
    fn private_modes() {
        let mut g = Grid::new(10, 2, 10);
        let mut vt = Vt::new();
        feed(&mut g, &mut vt, "x\x1b[?1049h\x1b[?25l\x1b[?1h\x1b[?2004h");
        assert!(g.is_alt_screen());
        assert!(!g.modes.cursor_visible);
        assert!(g.modes.app_cursor);
        assert!(g.modes.bracketed_paste);
        feed(&mut g, &mut vt, "\x1b[?1049l");
        assert!(!g.is_alt_screen());
        assert_eq!(row_text(&g, 0), "x");
    }
}
