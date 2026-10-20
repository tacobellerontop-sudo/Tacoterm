//! Feeds raw PTY bytes through `vte` into the [`Grid`].
//!
//! Implements what ConPTY and the programs behind it actually emit:
//! printing, C0 controls, cursor movement and save/restore, erasing,
//! insert/delete, scroll regions, the alternate screen, the common private
//! modes (including mouse and focus reporting), cursor shape, device status
//! and attribute reports, window titles, the bell, OSC 52 clipboard writes,
//! the working directory (OSC 7 / OSC 9;9), SGR colors (16 ANSI + 256 +
//! truecolor) and pictures (kitty graphics, sixel, iTerm2 inline images; see
//! [`crate::graphics`]). Everything else is ignored rather than misrendered —
//! extend as needed.

use vte::{Params, Perform};

use crate::config::CursorStyle;
use crate::graphics::{self, Sixel};
use crate::grid::{Attr, Color, Grid, MouseMode, Pen};

/// The longest APC string kept (a kitty graphics chunk is at most 4 KiB;
/// this leaves room for programs that don't chunk).
const MAX_APC: usize = 64 << 20;

/// Where the APC scanner is: `vte` drops APC strings, so kitty graphics
/// commands are picked out before the bytes reach it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Apc {
    Normal,
    /// Saw ESC at the end of a chunk.
    Esc,
    /// Inside `ESC _ … ESC \`.
    In,
    /// Inside, and just saw ESC.
    InEsc,
}

pub struct Vt {
    parser: vte::Parser,
    /// Bytes the terminal must send back to the shell (replies to status
    /// queries). The app drains this into the PTY after each `advance`.
    pub responses: Vec<u8>,
    /// Set when the program changed the window title (OSC 0 / 2).
    pub title: Option<String>,
    /// Set on BEL. The app clears it once it has rung / flashed.
    pub bell: bool,
    /// Text the program asked to put on the clipboard (OSC 52). The app
    /// takes it after each `advance`.
    pub clipboard: Option<String>,
    /// The shell's working directory as last reported (OSC 7 / OSC 9;9),
    /// in Windows form when it names a drive (`C:\Users\me`).
    pub cwd: Option<String>,
    /// A desktop notification the program asked for (OSC 9 / OSC 777):
    /// title and body. The app takes it.
    pub notification: Option<(String, String)>,
    /// Progress for the taskbar (ConEmu's OSC 9;4): state 0 clear, 1
    /// normal, 2 error, 3 indeterminate, 4 paused; percent.
    pub progress: Option<(u8, u8)>,
    apc_state: Apc,
    apc: Vec<u8>,
    /// A sixel picture arriving in a DCS string.
    sixel: Option<Sixel>,
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
            bell: false,
            clipboard: None,
            cwd: None,
            notification: None,
            progress: None,
            apc_state: Apc::Normal,
            apc: Vec::new(),
            sixel: None,
        }
    }

    /// Parse raw PTY output. UTF-8 sequences split across calls are fine:
    /// the parser keeps its state between chunks.
    pub fn advance(&mut self, grid: &mut Grid, bytes: &[u8]) {
        // Pass everything to vte except APC strings, which go to `apc`.
        let mut run = 0;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match self.apc_state {
                Apc::Normal if b == 0x1b => {
                    if i + 1 == bytes.len() {
                        self.feed(grid, &bytes[run..i]);
                        self.apc_state = Apc::Esc;
                        run = i + 1;
                    } else if bytes[i + 1] == b'_' {
                        self.feed(grid, &bytes[run..i]);
                        self.apc_state = Apc::In;
                        self.apc.clear();
                        i += 1;
                        run = i + 1;
                    }
                }
                Apc::Normal => {}
                Apc::Esc => {
                    if b == b'_' {
                        self.apc_state = Apc::In;
                        self.apc.clear();
                        run = i + 1;
                    } else {
                        // The ESC wasn't ours after all.
                        self.apc_state = Apc::Normal;
                        self.feed(grid, b"\x1b");
                        run = i;
                        continue;
                    }
                }
                Apc::In if b == 0x1b => self.apc_state = Apc::InEsc,
                Apc::In => {
                    if self.apc.len() < MAX_APC {
                        self.apc.push(b);
                    }
                }
                Apc::InEsc => {
                    if b == b'\\' {
                        self.apc_state = Apc::Normal;
                        let apc = std::mem::take(&mut self.apc);
                        self.apc_done(grid, &apc);
                        run = i + 1;
                    } else {
                        // ESC inside an APC ends it badly: drop it and let
                        // this ESC start whatever comes next.
                        self.apc_state = Apc::Normal;
                        self.apc.clear();
                        self.feed(grid, b"\x1b");
                        run = i;
                        continue;
                    }
                }
            }
            i += 1;
        }
        if self.apc_state == Apc::Normal {
            self.feed(grid, &bytes[run..]);
        }
    }

    fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut performer = Performer {
            grid,
            responses: &mut self.responses,
            title: &mut self.title,
            bell: &mut self.bell,
            clipboard: &mut self.clipboard,
            cwd: &mut self.cwd,
            notification: &mut self.notification,
            progress: &mut self.progress,
            sixel: &mut self.sixel,
        };
        self.parser.advance(&mut performer, bytes);
    }

    fn apc_done(&mut self, grid: &mut Grid, apc: &[u8]) {
        if let Some(body) = apc.strip_prefix(b"G") {
            if let Some(reply) = graphics::kitty(grid, body) {
                self.responses.extend_from_slice(&reply);
            }
        }
    }
}

struct Performer<'a> {
    grid: &'a mut Grid,
    responses: &'a mut Vec<u8>,
    title: &'a mut Option<String>,
    bell: &'a mut bool,
    clipboard: &'a mut Option<String>,
    cwd: &'a mut Option<String>,
    notification: &'a mut Option<(String, String)>,
    progress: &'a mut Option<(u8, u8)>,
    sixel: &'a mut Option<Sixel>,
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
            0x07 => *self.bell = true,
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Sixel: DCS P1 ; P2 ; P3 q.
        if action == 'q' && intermediates.is_empty() {
            *self.sixel = Some(Sixel::new());
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some(sixel) = self.sixel {
            sixel.put(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(image) = self.sixel.take().and_then(Sixel::finish) {
            graphics::place_sixel(self.grid, image);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some((&kind, rest)) = params.split_first() else {
            return;
        };
        // The remaining params rejoined: titles and paths may contain `;`.
        let joined = |ps: &[&[u8]]| {
            ps.iter()
                .map(|p| String::from_utf8_lossy(p))
                .collect::<Vec<_>>()
                .join(";")
        };
        match kind {
            // Window title.
            b"0" | b"2" if !rest.is_empty() => *self.title = Some(joined(rest)),
            // Working directory as a `file://host/path` URL.
            b"7" if !rest.is_empty() => {
                if let Some(dir) = file_url_path(&joined(rest)) {
                    *self.cwd = Some(dir);
                }
            }
            // Hyperlink: `8;params;URI` starts one, an empty URI ends it.
            b"8" if !rest.is_empty() => {
                let url = joined(&rest[1.min(rest.len())..]);
                self.grid.pen.link = crate::grid::link_id(&url);
            }
            // Shell integration (FinalTerm / OSC 133): A starts a prompt.
            b"133" if rest.first().is_some_and(|r| r.starts_with(b"A")) => {
                self.grid.mark_prompt();
            }
            // ConEmu's taskbar progress: `9;4;state;percent`.
            b"9" if rest.len() > 1 && rest[0] == b"4" => {
                let num = |i: usize| {
                    rest.get(i)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse::<u8>().ok())
                        .unwrap_or(0)
                };
                *self.progress = Some((num(1).min(4), num(2).min(100)));
            }
            // A notification: `9;text` (iTerm2) or
            // `777;notify;title;body` (urxvt, WezTerm).
            // (ConEmu's numbered `9;n;…` commands are something else.)
            b"9" if !rest.is_empty() && !rest[0].iter().all(u8::is_ascii_digit) => {
                *self.notification = Some((String::new(), joined(rest)));
            }
            b"777" if rest.len() >= 2 && rest[0] == b"notify" => {
                let title = String::from_utf8_lossy(rest[1]).into_owned();
                *self.notification = Some((title, joined(&rest[2.min(rest.len())..])));
            }
            // ConEmu's `9;9;"path"`: working directory, quotes optional.
            b"9" if rest.len() > 1 && rest[0] == b"9" => {
                let raw = joined(&rest[1..]);
                let dir = raw.trim_matches('"');
                if !dir.is_empty() {
                    *self.cwd = Some(dir.to_string());
                }
            }
            // Clipboard write: `52;<selection>;<base64>`, any selection. A
            // `?` asks to read the clipboard, which we never answer.
            // iTerm2 inline image: `1337;File=args:base64`.
            b"1337" if rest.first().is_some_and(|r| r.starts_with(b"File=")) => {
                let joined: Vec<u8> = rest.join(&b';');
                graphics::iterm_file(self.grid, &joined[5..]);
            }
            b"52" if rest.len() >= 2 && rest[1] != b"?" => {
                if let Some(text) = base64_decode(rest[1]).and_then(|b| String::from_utf8(b).ok()) {
                    *self.clipboard = Some(text);
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let private = intermediates.first() == Some(&b'?');
        let g = &mut *self.grid;
        match (action, intermediates) {
            // DECSCUSR: cursor shape. 0 (or unknown) hands it back to config.
            ('q', b" ") => {
                g.modes.cursor_shape = match param(params, 0, 0) {
                    1 => Some((CursorStyle::Block, true)),
                    2 => Some((CursorStyle::Block, false)),
                    3 => Some((CursorStyle::Underline, true)),
                    4 => Some((CursorStyle::Underline, false)),
                    5 => Some((CursorStyle::Bar, true)),
                    6 => Some((CursorStyle::Bar, false)),
                    _ => None,
                };
                return;
            }
            // XTVERSION: our name, as a DCS string.
            ('q', b">") => {
                self.responses.extend_from_slice(b"\x1bP>|tacoterm\x1b\\");
                return;
            }
            // Secondary device attributes: VT100-class, version 10, no ROM.
            ('c', b">") => {
                self.responses.extend_from_slice(b"\x1b[>0;10;1c");
                return;
            }
            // XTSMGRAPHICS: sixel color registers (1) and largest size (2).
            ('S', b"?") => {
                let reply = match (param(params, 0, 0), param(params, 1, 0)) {
                    (1, 1 | 4) => "\x1b[?1;0;1024S".to_string(),
                    (2, 1 | 4) => "\x1b[?2;0;10000;10000S".to_string(),
                    (item, _) => format!("\x1b[?{item};1;0S"),
                };
                self.responses.extend_from_slice(reply.as_bytes());
                return;
            }
            // DECRQM for a private mode: 1 set, 2 reset, 0 not recognized.
            ('p', b"?$") => {
                let mode = param(params, 0, 0);
                let state = match private_mode_state(g, mode) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                let reply = format!("\x1b[?{mode};{state}$y");
                self.responses.extend_from_slice(reply.as_bytes());
                return;
            }
            _ => {}
        }
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
            // Primary device attributes: VT220 with sixel graphics (4) and
            // ANSI color (22).
            ('c', false) if intermediates.is_empty() => {
                self.responses.extend_from_slice(b"\x1b[?62;4;22c")
            }
            // XTWINOPS size reports, which picture tools use to size
            // their output: text area in pixels, a cell, text area in cells.
            ('t', false) => {
                let (cw, ch) = g.cell_pixels;
                let reply = match param(params, 0, 0) {
                    14 => format!("\x1b[4;{};{}t", g.rows as u32 * ch, g.cols as u32 * cw),
                    16 => format!("\x1b[6;{ch};{cw}t"),
                    18 => format!("\x1b[8;{};{}t", g.rows, g.cols),
                    _ => String::new(),
                };
                self.responses.extend_from_slice(reply.as_bytes());
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
    let mouse = |level: MouseMode| if on { level } else { MouseMode::Off };
    match mode {
        1 => g.modes.app_cursor = on,
        7 => g.modes.autowrap = on,
        // Cursor blink: only adjusts a shape the program already chose.
        12 => {
            if let Some((_, blink)) = &mut g.modes.cursor_shape {
                *blink = on;
            }
        }
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
        // X10 (?9) is treated as normal tracking.
        9 | 1000 => g.modes.mouse = mouse(MouseMode::Click),
        1002 => g.modes.mouse = mouse(MouseMode::Drag),
        1003 => g.modes.mouse = mouse(MouseMode::Motion),
        1004 => g.modes.focus_events = on,
        1006 => g.modes.mouse_sgr = on,
        2004 => g.modes.bracketed_paste = on,
        2026 => g.modes.sync_output = on,
        _ => {}
    }
}

/// Whether a private mode is set, for DECRQM. `None` when we don't know it.
fn private_mode_state(g: &Grid, mode: u16) -> Option<bool> {
    let m = &g.modes;
    Some(match mode {
        1 => m.app_cursor,
        7 => m.autowrap,
        25 => m.cursor_visible,
        1049 => g.is_alt_screen(),
        9 | 1000 => m.mouse == MouseMode::Click,
        1002 => m.mouse == MouseMode::Drag,
        1003 => m.mouse == MouseMode::Motion,
        1004 => m.focus_events,
        1006 => m.mouse_sgr,
        2004 => m.bracketed_paste,
        2026 => m.sync_output,
        _ => return None,
    })
}

/// The local path out of a `file://host/path` URL, percent-decoded. A drive
/// path (`/C:/Users`) becomes Windows form (`C:\Users`); anything else (a
/// plain Unix path) is kept as is.
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // Skip the host, which is often empty (`file:///C:/...`).
    let path = percent_decode(&rest[rest.find('/')?..]);
    let b = path.as_bytes();
    if b.len() >= 3 && b[1].is_ascii_alphabetic() && b[2] == b':' {
        let win = path[1..].replace('/', "\\");
        // A bare drive (`/C:`) means its root.
        return Some(if win.len() == 2 { win + "\\" } else { win });
    }
    Some(path)
}

/// Decode `%XX` escapes; malformed ones are kept literally.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i).and_then(|&c| (c as char).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (hex(i + 1), hex(i + 2)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode standard (RFC 4648) base64, padding optional. `None` on anything
/// outside the alphabet or a length no encoder produces.
pub(crate) fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        Some(v as u32)
    };
    let data = data
        .strip_suffix(b"==")
        .or_else(|| data.strip_suffix(b"="))
        .unwrap_or(data);
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    for chunk in data.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = acc << 6 | value(c)?;
        }
        // Left-align as if the chunk were full, then keep the whole bytes
        // it carries (n chars -> n - 1 bytes).
        acc <<= 6 * (4 - chunk.len() as u32);
        out.extend_from_slice(&acc.to_be_bytes()[1..chunk.len()]);
    }
    Some(out)
}

/// SGR — select graphic rendition.
fn sgr(g: &mut Grid, params: &Params) {
    let groups: Vec<&[u16]> = params.iter().collect();
    // SGR 0 resets everything but the hyperlink, which only OSC 8 ends.
    let reset = |g: &mut Grid| {
        g.pen = Pen {
            link: g.pen.link,
            ..Pen::default()
        }
    };
    if groups.is_empty() {
        reset(g);
        return;
    }
    let mut i = 0;
    while i < groups.len() {
        let n = groups[i].first().copied().unwrap_or(0);
        let attr = &mut g.pen.attr;
        match n {
            0 => reset(g),
            1 => attr.set(Attr::BOLD, true),
            2 => attr.set(Attr::DIM, true),
            3 => attr.set(Attr::ITALIC, true),
            4 => {
                // 4:n picks a style: 0 none, 1 single, 2 double, 3 curly,
                // 4 dotted, 5 dashed.
                let style = groups[i].get(1).copied().unwrap_or(1);
                attr.set(Attr::UNDERLINE | Attr::UNDERLINE_STYLES, false);
                let extra = match style {
                    2 => Attr::DOUBLE_UNDERLINE,
                    3 => Attr::CURLY_UNDERLINE,
                    4 => Attr::DOTTED_UNDERLINE,
                    5 => Attr::DASHED_UNDERLINE,
                    _ => 0,
                };
                if style != 0 {
                    attr.set(Attr::UNDERLINE | extra, true);
                }
            }
            7 => attr.set(Attr::INVERSE, true),
            8 => attr.set(Attr::HIDDEN, true),
            9 => attr.set(Attr::STRIKE, true),
            21 => {
                attr.set(Attr::UNDERLINE_STYLES, false);
                attr.set(Attr::UNDERLINE | Attr::DOUBLE_UNDERLINE, true);
            }
            22 => {
                attr.set(Attr::BOLD, false);
                attr.set(Attr::DIM, false);
            }
            23 => attr.set(Attr::ITALIC, false),
            24 => attr.set(Attr::UNDERLINE | Attr::UNDERLINE_STYLES, false),
            59 => g.pen.ul = Color::Default,
            27 => attr.set(Attr::INVERSE, false),
            28 => attr.set(Attr::HIDDEN, false),
            29 => attr.set(Attr::STRIKE, false),
            30..=37 => g.pen.fg = Color::Indexed((n - 30) as u8),
            39 => g.pen.fg = Color::Default,
            40..=47 => g.pen.bg = Color::Indexed((n - 40) as u8),
            49 => g.pen.bg = Color::Default,
            90..=97 => g.pen.fg = Color::Indexed((n - 90 + 8) as u8),
            100..=107 => g.pen.bg = Color::Indexed((n - 100 + 8) as u8),
            38 | 48 | 58 => {
                let (color, used) = extended_color(&groups[i..]);
                if let Some(color) = color {
                    match n {
                        38 => g.pen.fg = color,
                        48 => g.pen.bg = color,
                        _ => g.pen.ul = color,
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
        // The BEL that ended the OSC is a terminator, not a bell.
        assert!(!vt.bell);
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

    #[test]
    fn mouse_focus_and_sync_modes() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        assert_eq!(g.modes.mouse, MouseMode::Off);
        feed(&mut g, &mut vt, "\x1b[?1000;1006h\x1b[?1004h\x1b[?2026h");
        assert_eq!(g.modes.mouse, MouseMode::Click);
        assert!(g.modes.mouse_sgr && g.modes.focus_events && g.modes.sync_output);
        feed(&mut g, &mut vt, "\x1b[?1002h");
        assert_eq!(g.modes.mouse, MouseMode::Drag);
        feed(&mut g, &mut vt, "\x1b[?1003h");
        assert_eq!(g.modes.mouse, MouseMode::Motion);
        feed(&mut g, &mut vt, "\x1b[?1003l\x1b[?9h");
        assert_eq!(g.modes.mouse, MouseMode::Click);
        feed(&mut g, &mut vt, "\x1b[?9l\x1b[?1006l\x1b[?1004l\x1b[?2026l");
        assert_eq!(g.modes.mouse, MouseMode::Off);
        assert!(!g.modes.mouse_sgr && !g.modes.focus_events && !g.modes.sync_output);
        // ESC c puts every mode back.
        feed(&mut g, &mut vt, "\x1b[?1003;1006;1004h\x1b[5 q\x1bc");
        assert_eq!(g.modes.mouse, MouseMode::Off);
        assert!(!g.modes.mouse_sgr && !g.modes.focus_events);
        assert_eq!(g.modes.cursor_shape, None);
    }

    #[test]
    fn decscusr_cursor_shape() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        let cases = [
            (1, Some((CursorStyle::Block, true))),
            (2, Some((CursorStyle::Block, false))),
            (3, Some((CursorStyle::Underline, true))),
            (4, Some((CursorStyle::Underline, false))),
            (5, Some((CursorStyle::Bar, true))),
            (6, Some((CursorStyle::Bar, false))),
            (0, None),
        ];
        for (n, want) in cases {
            feed(&mut g, &mut vt, &format!("\x1b[{n} q"));
            assert_eq!(g.modes.cursor_shape, want, "DECSCUSR {n}");
        }
        feed(&mut g, &mut vt, "\x1b[6 q\x1b[?12h");
        assert_eq!(g.modes.cursor_shape, Some((CursorStyle::Bar, true)));
        feed(&mut g, &mut vt, "\x1b[?12l");
        assert_eq!(g.modes.cursor_shape, Some((CursorStyle::Bar, false)));
        // No intermediate: not DECSCUSR (it's DECLL), leave the shape alone.
        feed(&mut g, &mut vt, "\x1b[0q");
        assert_eq!(g.modes.cursor_shape, Some((CursorStyle::Bar, false)));
    }

    #[test]
    fn clusters_underlines_links_prompts_and_notifications() {
        let mut g = Grid::new(20, 3, 100);
        let mut vt = Vt::new();
        // e + combining acute, a ZWJ family, a flag.
        feed(&mut g, &mut vt, "e\u{301}👨\u{200d}👩x🇳🇱y");
        assert_eq!(
            crate::grid::cluster_text(g.cell(0, 0).c).unwrap(),
            "e\u{301}"
        );
        assert_eq!(
            crate::grid::cluster_text(g.cell(1, 0).c).unwrap(),
            "👨\u{200d}👩"
        );
        assert_eq!(g.cell(3, 0).c, 'x');
        assert_eq!(crate::grid::cluster_text(g.cell(4, 0).c).unwrap(), "🇳🇱");
        // Underline styles and color.
        feed(
            &mut g,
            &mut vt,
            "\r\n\x1b[4:3;58:2::255:0:0ma\x1b[21mb\x1b[24;59mc",
        );
        let a = g.cell(0, 1);
        assert!(a.attr.has(Attr::UNDERLINE | Attr::CURLY_UNDERLINE));
        assert_eq!(a.ul, Color::Rgb(255, 0, 0));
        assert!(g.cell(1, 1).attr.has(Attr::DOUBLE_UNDERLINE));
        assert!(!g.cell(2, 1).attr.has(Attr::UNDERLINE));
        assert_eq!(g.cell(2, 1).ul, Color::Default);
        // A hyperlink survives SGR 0 and ends with an empty URI.
        feed(
            &mut g,
            &mut vt,
            "\x1b]8;;https://x.y\x1b\\L\x1b[0mK\x1b]8;;\x1b\\z",
        );
        assert_eq!(
            crate::grid::link_url(g.cell(3, 1).link).unwrap(),
            "https://x.y"
        );
        assert_ne!(g.cell(4, 1).link, 0);
        assert_eq!(g.cell(5, 1).link, 0);
        // Prompt marks, notifications and progress.
        feed(
            &mut g,
            &mut vt,
            "\x1b]133;A\x07\x1b]9;done\x07\x1b]9;4;1;42\x07",
        );
        assert_eq!(g.prompt_lines(), vec![1]);
        assert_eq!(vt.notification.take().unwrap().1, "done");
        assert_eq!(vt.progress, Some((1, 42)));
        feed(
            &mut g,
            &mut vt,
            "\x1b]777;notify;Build;ok\x07\x1b]9;9;C:\\x\x07",
        );
        assert_eq!(
            vt.notification.take().unwrap(),
            ("Build".into(), "ok".into())
        );
        assert!(vt.notification.is_none());
    }

    #[test]
    fn pictures_from_apc_dcs_and_osc() {
        let mut g = Grid::new(20, 5, 100);
        g.cell_pixels = (8, 16);
        let mut vt = Vt::new();
        // A 1×1 red RGBA picture over kitty, split mid-introducer and
        // mid-terminator, with text either side.
        vt.advance(&mut g, b"ab\x1b");
        vt.advance(&mut g, b"_Ga=T,f=32,s=1,v=1,i=3;/wAA/w==\x1b");
        vt.advance(&mut g, b"\\cd");
        assert_eq!(vt.responses, b"\x1b_Gi=3;OK\x1b\\");
        assert_eq!(g.visible_images().len(), 1);
        assert_eq!(g.cell(0, 0).c, 'a');
        assert_eq!(g.cell(3, 0).c, 'c', "the cursor moved past the picture");
        // Sixel: one red column 6 pixels tall.
        feed(&mut g, &mut vt, "\r\n\x1bPq#1;2;100;0;0#1~\x1b\\");
        assert_eq!(g.visible_images().len(), 2);
        // Cell size report.
        vt.responses.clear();
        feed(&mut g, &mut vt, "\x1b[16t");
        assert_eq!(vt.responses, b"\x1b[6;16;8t");
        // An ESC that isn't an APC still works.
        feed(&mut g, &mut vt, "\x1b[2J");
        assert!(g.visible_images().is_empty());
    }

    #[test]
    fn attribute_and_version_queries() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        feed(&mut g, &mut vt, "\x1b[>c");
        assert_eq!(vt.responses, b"\x1b[>0;10;1c");
        vt.responses.clear();
        feed(&mut g, &mut vt, "\x1b[>q");
        assert_eq!(vt.responses, b"\x1bP>|tacoterm\x1b\\");
        vt.responses.clear();
        feed(&mut g, &mut vt, "\x1b[c");
        assert_eq!(vt.responses, b"\x1b[?62;4;22c");
        vt.responses.clear();
        feed(
            &mut g,
            &mut vt,
            "\x1b[?2026$p\x1b[?2026h\x1b[?2026$p\x1b[?4242$p",
        );
        assert_eq!(vt.responses, b"\x1b[?2026;2$y\x1b[?2026;1$y\x1b[?4242;0$y");
    }

    #[test]
    fn bell() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        feed(&mut g, &mut vt, "a\x1b]2;t\x1b\\b");
        assert!(!vt.bell);
        feed(&mut g, &mut vt, "\x07");
        assert!(vt.bell);
        assert_eq!(row_text(&g, 0), "ab");
    }

    #[test]
    fn osc52_clipboard() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        // "hello, wörld"
        feed(&mut g, &mut vt, "\x1b]52;c;aGVsbG8sIHfDtnJsZA==\x07");
        assert_eq!(vt.clipboard.as_deref(), Some("hello, wörld"));
        // Other selections, no padding, ST terminator.
        feed(&mut g, &mut vt, "\x1b]52;p;aGk\x1b\\");
        assert_eq!(vt.clipboard.as_deref(), Some("hi"));
        // Queries and garbage are ignored.
        feed(&mut g, &mut vt, "\x1b]52;c;?\x07\x1b]52;c;not*base64\x07");
        assert_eq!(vt.clipboard.as_deref(), Some("hi"));
    }

    #[test]
    fn base64_round_trips() {
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert_eq!(base64_decode(b"Zg==").unwrap(), b"f");
        assert_eq!(base64_decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode(b"Zm9vYg").unwrap(), b"foob");
        assert_eq!(base64_decode(b"+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
        assert_eq!(base64_decode(b"Zm9vY"), None);
        assert_eq!(base64_decode(b"Zm=v"), None);
    }

    #[test]
    fn working_directory() {
        let mut g = Grid::new(10, 2, 0);
        let mut vt = Vt::new();
        feed(
            &mut g,
            &mut vt,
            "\x1b]7;file://box/C:/Users/me/My%20Docs\x07",
        );
        assert_eq!(vt.cwd.as_deref(), Some(r"C:\Users\me\My Docs"));
        feed(&mut g, &mut vt, "\x1b]7;file:///d:/\x1b\\");
        assert_eq!(vt.cwd.as_deref(), Some(r"d:\"));
        feed(&mut g, &mut vt, "\x1b]7;file:///E:\x07");
        assert_eq!(vt.cwd.as_deref(), Some(r"E:\"));
        feed(&mut g, &mut vt, "\x1b]7;file://host/home/me/a;b\x07");
        assert_eq!(vt.cwd.as_deref(), Some("/home/me/a;b"));
        feed(&mut g, &mut vt, "\x1b]9;9;\"C:\\Program Files\"\x07");
        assert_eq!(vt.cwd.as_deref(), Some(r"C:\Program Files"));
        feed(&mut g, &mut vt, "\x1b]9;9;D:\\src\x07");
        assert_eq!(vt.cwd.as_deref(), Some(r"D:\src"));
        // Not a file URL, or a plain OSC 9 notification: ignored.
        feed(&mut g, &mut vt, "\x1b]7;http://x/y\x07\x1b]9;hello\x07");
        assert_eq!(vt.cwd.as_deref(), Some(r"D:\src"));
    }
}
