//! Feeds raw PTY bytes through `vte` into the [`Grid`].
//!
//! Implements the common subset real programs use: printing, C0 controls,
//! cursor movement, erasing, and SGR colors (16 ANSI + 256 + truecolor).
//! Everything else is ignored rather than misrendered — extend as needed.

use vte::{Params, Perform};

use crate::grid::{Attr, Color, Grid, Pen};

pub struct Vt {
    parser: vte::Parser,
}

impl Vt {
    pub fn new() -> Self {
        Vt {
            parser: vte::Parser::new(),
        }
    }

    pub fn advance(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let mut performer = Performer { grid };
        self.parser.advance(&mut performer, bytes);
    }
}

struct Performer<'a> {
    grid: &'a mut Grid,
}

/// params[i], or `default` when missing.
fn param(params: &Params, i: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(i)
        .and_then(|g| g.first().copied())
        .unwrap_or(default)
}

impl<'a> Perform for Performer<'a> {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0a | 0x0b | 0x0c => self.grid.line_feed(),
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

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // TODO: OSC 0/2 -> window title, OSC 52 -> clipboard
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            'A' => self.grid.move_cursor_by(0, -(param(params, 0, 1) as isize)), // up
            'B' | 'e' => self.grid.move_cursor_by(0, param(params, 0, 1) as isize), // down
            'C' | 'a' => self.grid.move_cursor_by(param(params, 0, 1) as isize, 0), // right
            'D' => self.grid.move_cursor_by(-(param(params, 0, 1) as isize), 0), // left
            'E' => {
                self.grid.move_cursor_by(0, param(params, 0, 1) as isize);
                self.grid.carriage_return();
            }
            'F' => {
                self.grid.move_cursor_by(0, -(param(params, 0, 1) as isize));
                self.grid.carriage_return();
            }
            'G' | '`' => {
                let x = param(params, 0, 1).saturating_sub(1) as usize;
                self.grid.move_cursor_to(x, self.grid.cursor_y);
            }
            'H' | 'f' => {
                let y = param(params, 0, 1).saturating_sub(1) as usize;
                let x = param(params, 1, 1).saturating_sub(1) as usize;
                self.grid.move_cursor_to(x, y);
            }
            'J' => self.grid.erase_display(param(params, 0, 0)),
            'K' => self.grid.erase_line(param(params, 0, 0)),
            'm' => self.sgr(params),
            'h' | 'l' => {
                // TODO: modes — notably ?7 (autowrap) and ?25 (cursor visibility)
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

impl<'a> Performer<'a> {
    /// SGR — select graphic rendition. Handles reset, bold, underline,
    /// inverse, 16 ANSI colors, 256-color and truecolor.
    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.grid.pen = Pen::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let n = groups[i].first().copied().unwrap_or(0);
            match n {
                0 => self.grid.pen = Pen::default(),
                1 => self.grid.pen.attr.set(Attr::BOLD, true),
                4 => self.grid.pen.attr.set(Attr::UNDERLINE, true),
                7 => self.grid.pen.attr.set(Attr::INVERSE, true),
                22 => self.grid.pen.attr.set(Attr::BOLD, false),
                24 => self.grid.pen.attr.set(Attr::UNDERLINE, false),
                27 => self.grid.pen.attr.set(Attr::INVERSE, false),
                30..=37 => self.grid.pen.fg = Color::Ansi((n - 30) as u8),
                39 => self.grid.pen.fg = Color::Default,
                40..=47 => self.grid.pen.bg = Color::Ansi((n - 40) as u8),
                49 => self.grid.pen.bg = Color::Default,
                90..=97 => self.grid.pen.fg = Color::Ansi((n - 90 + 8) as u8),
                100..=107 => self.grid.pen.bg = Color::Ansi((n - 100 + 8) as u8),
                38 | 48 => {
                    // Extended color, two spellings:
                    //   colon form:     `38:2:{r}:{g}:{b}` / `38:5:{idx}`
                    //   semicolon form: `38;2;{r};{g};{b}` / `38;5;{idx}`
                    let is_fg = n == 38;
                    let set = |grid: &mut Grid, color: Color| {
                        if is_fg {
                            grid.pen.fg = color;
                        } else {
                            grid.pen.bg = color;
                        }
                    };
                    let g = groups[i];
                    if g.len() > 1 {
                        match g.get(1) {
                            Some(2) if g.len() >= 5 => set(
                                &mut self.grid,
                                Color::Rgb(
                                    g[2].min(255) as u8,
                                    g[3].min(255) as u8,
                                    g[4].min(255) as u8,
                                ),
                            ),
                            Some(5) if g.len() >= 3 => {
                                set(&mut self.grid, Color::Ansi(g[2].min(255) as u8))
                            }
                            _ => {}
                        }
                        i += 1;
                        continue;
                    }
                    let mode = groups.get(i + 1).and_then(|g| g.first().copied());
                    match mode {
                        Some(2) => {
                            // Gather r, g, b across group boundaries.
                            let mut rgb = [0u8; 3];
                            let mut got = 0;
                            let mut j = i + 2; // first value group
                            while got < 3 {
                                if let Some(g) = groups.get(j) {
                                    for &v in *g {
                                        if got < 3 {
                                            rgb[got] = v.min(255) as u8;
                                            got += 1;
                                        }
                                    }
                                } else {
                                    break;
                                }
                                j += 1;
                            }
                            set(&mut self.grid, Color::Rgb(rgb[0], rgb[1], rgb[2]));
                            i = j;
                            continue;
                        }
                        Some(5) => {
                            if let Some(idx) =
                                groups.get(i + 2).and_then(|g| g.first().copied())
                            {
                                set(&mut self.grid, Color::Ansi(idx.min(255) as u8));
                            }
                            i += 3;
                            continue;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}
