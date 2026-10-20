//! The line editor: decodes the VT key sequences the console sends in raw
//! mode, edits the line, and redraws it (wrapping across rows included).
//! Everything here is pure; `term` does the actual console I/O.

use crate::complete::{common_prefix, Completion};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
    Tab,
    CtrlC,
    CtrlD,
    CtrlL,
    /// Search history
    CtrlR,
    /// Cancel a search
    CtrlG,
    KillStart,
    KillEnd,
    KillWordBack,
    Escape,
    Unknown,
}

/// Turns input bytes into keys. Sequences split across reads are kept until
/// the rest arrives; an Esc at the very end of a read is the Esc key.
#[derive(Default)]
pub struct KeyDecoder {
    buf: Vec<u8>,
}

impl KeyDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.buf.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.buf.len() {
            match decode(&self.buf[i..]) {
                Some((key, len)) => {
                    keys.push(key);
                    i += len;
                }
                None => break,
            }
        }
        self.buf.drain(..i);
        if self.buf == [0x1b] {
            self.buf.clear();
            keys.push(Key::Escape);
        }
        keys
    }
}

/// Decode one key from the front of `b`, or `None` if it's incomplete.
fn decode(b: &[u8]) -> Option<(Key, usize)> {
    let key = match b[0] {
        0x1b => return decode_escape(b),
        b'\r' if b.get(1) == Some(&b'\n') => return Some((Key::Enter, 2)),
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        b'\t' => Key::Tab,
        0x01 => Key::Home,
        0x02 => Key::Left,
        0x03 => Key::CtrlC,
        0x04 => Key::CtrlD,
        0x05 => Key::End,
        0x06 => Key::Right,
        0x07 => Key::CtrlG,
        0x0b => Key::KillEnd,
        0x0c => Key::CtrlL,
        0x0e => Key::Down,
        0x10 => Key::Up,
        0x12 => Key::CtrlR,
        0x15 => Key::KillStart,
        0x17 => Key::KillWordBack,
        c if c < 0x20 => Key::Unknown,
        c if c < 0x80 => Key::Char(c as char),
        c => {
            let len = match c {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Some((Key::Unknown, 1)),
            };
            if b.len() < len {
                return None;
            }
            return Some(match std::str::from_utf8(&b[..len]) {
                Ok(s) => (Key::Char(s.chars().next().unwrap()), len),
                Err(_) => (Key::Unknown, 1),
            });
        }
    };
    Some((key, 1))
}

fn decode_escape(b: &[u8]) -> Option<(Key, usize)> {
    match b.get(1) {
        None => None,
        Some(b'[') | Some(b'O') => {
            // CSI / SS3: parameters, then a final byte.
            let end = b[2..].iter().position(|&c| (0x40..=0x7e).contains(&c))?;
            let params = std::str::from_utf8(&b[2..2 + end]).unwrap_or("");
            let fin = b[2 + end];
            let ctrl = params.ends_with(";5");
            let key = match (fin, params.split(';').next().unwrap_or("")) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) if ctrl => Key::WordRight,
                (b'D', _) if ctrl => Key::WordLeft,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) => Key::Home,
                (b'F', _) => Key::End,
                (b'~', "1") | (b'~', "7") => Key::Home,
                (b'~', "4") | (b'~', "8") => Key::End,
                (b'~', "3") => Key::Delete,
                _ => Key::Unknown,
            };
            Some((key, 3 + end))
        }
        // Alt+key and such: drop the Esc.
        Some(_) => Some((Key::Unknown, 1)),
    }
}

/// The line being edited; `cursor` counts chars.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Line {
    pub chars: Vec<char>,
    pub cursor: usize,
}

impl Line {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Replace chars `start..cursor` with `s`.
    pub fn replace_back_to(&mut self, start: usize, s: &str) {
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        self.insert_str(s);
    }

    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        let mut i = self.cursor;
        let n = self.chars.len();
        while i < n && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Apply an editing key; returns false for keys that aren't edits.
    pub fn edit(&mut self, key: Key) -> bool {
        match key {
            Key::Char(c) => self.insert_str(&c.to_string()),
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            Key::Delete | Key::CtrlD if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.chars.len(),
            Key::WordLeft => self.cursor = self.word_start(),
            Key::WordRight => self.cursor = self.word_end(),
            Key::KillStart => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::KillEnd => self.chars.truncate(self.cursor),
            Key::KillWordBack => {
                let start = self.word_start();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Escape => self.set(""),
            Key::Backspace | Key::Delete | Key::CtrlD => {}
            _ => return false,
        }
        true
    }
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Display width of a string that may hold escape sequences (CSI and OSC),
/// which take no room.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            width += c.width().unwrap_or(0);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // Ends at BEL or ST (ESC \).
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    width
}

/// How the line looks: the text with colors added (the same width as the
/// text), and a suggestion shown dimmed after it.
#[derive(Default)]
pub struct Look {
    pub colored: Option<String>,
    pub ghost: String,
}

/// Redraw the prompt and line. `cursor_row` is how many rows below the
/// prompt's first row the cursor is now; returns the output and the new
/// value.
pub fn render(
    prompt: &str,
    prompt_width: usize,
    line: &Line,
    look: &Look,
    cols: usize,
    cursor_row: usize,
) -> (String, usize) {
    let cols = cols.max(1);
    let mut out = String::new();
    if cursor_row > 0 {
        out.push_str(&format!("\x1b[{cursor_row}A"));
    }
    out.push_str("\r\x1b[J");
    out.push_str(prompt);
    let text = line.text();
    out.push_str(look.colored.as_deref().unwrap_or(&text));
    if !look.ghost.is_empty() {
        out.push_str(&format!("\x1b[90m{}\x1b[0m", look.ghost));
    }
    let total = prompt_width + str_width(&text) + str_width(&look.ghost);
    if total > 0 && total.is_multiple_of(cols) {
        // The cursor is parked past the last column; step onto the next row
        // so where it is isn't up to the terminal.
        out.push_str(" \r\x1b[K");
    }
    let end_row = total / cols;
    let before: String = line.chars[..line.cursor].iter().collect();
    let at = prompt_width + str_width(&before);
    let (row, col) = (at / cols, at % cols);
    if end_row > row {
        out.push_str(&format!("\x1b[{}A", end_row - row));
    }
    out.push('\r');
    if col > 0 {
        out.push_str(&format!("\x1b[{col}C"));
    }
    (out, row)
}

/// Lay names out in columns, like `ls`.
pub fn columns(names: &[String], cols: usize) -> String {
    let widest = names.iter().map(|n| str_width(n)).max().unwrap_or(0) + 2;
    let per_row = (cols / widest).max(1);
    let mut out = String::new();
    for row in names.chunks(per_row) {
        let mut line = String::new();
        for (i, name) in row.iter().enumerate() {
            line.push_str(name);
            if i + 1 < row.len() {
                line.push_str(&" ".repeat(widest - str_width(name)));
            }
        }
        out.push_str(line.trim_end());
        out.push_str("\r\n");
    }
    out
}

/// Up / Down through earlier lines; what was being typed comes back at the
/// bottom.
pub struct HistoryWalk<'a> {
    entries: &'a [String],
    index: usize,
    draft: String,
}

impl<'a> HistoryWalk<'a> {
    pub fn new(entries: &'a [String]) -> Self {
        HistoryWalk {
            entries,
            index: entries.len(),
            draft: String::new(),
        }
    }

    pub fn step(&mut self, line: &mut Line, up: bool) {
        if self.index == self.entries.len() {
            self.draft = line.text();
        }
        let next = if up {
            match self.index.checked_sub(1) {
                Some(i) => i,
                None => return,
            }
        } else if self.index < self.entries.len() {
            self.index + 1
        } else {
            return;
        };
        self.index = next;
        line.set(self.entries.get(next).map_or(&self.draft, |s| s));
    }
}

/// The console, as the editor sees it.
pub trait Io {
    /// Blocks until there's input; 0 means end of input.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write(&mut self, s: &str) -> std::io::Result<()>;
    fn cols(&self) -> usize;
}

#[derive(Debug, PartialEq)]
pub enum ReadLine {
    Line(String),
    /// Ctrl+C: the line was thrown away.
    Interrupted,
    /// Ctrl+D on an empty line, or the input ended.
    Eof,
}

/// The newest history entry that starts with `text` (and goes further),
/// for the suggestion shown after the cursor.
pub fn suggest<'a>(history: &'a [String], text: &str) -> Option<&'a str> {
    if text.trim().is_empty() {
        return None;
    }
    history
        .iter()
        .rev()
        .find(|h| h.len() > text.len() && h.starts_with(text) && !h.contains('\n'))
        .map(|h| &h[text.len()..])
}

/// The newest entry before `before` containing `query`, ignoring case.
fn search_back(history: &[String], query: &str, before: usize) -> Option<usize> {
    let query = query.to_lowercase();
    history[..before.min(history.len())]
        .iter()
        .rposition(|h| h.to_lowercase().contains(&query))
}

/// Ctrl+R: searching history as you type.
struct Search {
    query: String,
    found: Option<usize>,
    /// The line from before the search, back on Esc.
    saved: Line,
}

/// Read one line with editing, history, suggestions, search, colors and
/// completion.
pub fn read_line(
    io: &mut dyn Io,
    prompt: &str,
    history: &[String],
    complete: &dyn Fn(&[char]) -> Completion,
    highlight: &dyn Fn(&[char]) -> String,
) -> std::io::Result<ReadLine> {
    let prompt_width = visible_width(prompt);
    let mut decoder = KeyDecoder::default();
    let mut line = Line::default();
    let mut walk = HistoryWalk::new(history);
    let mut search: Option<Search> = None;
    // What to draw: the prompt (or the search's), and how the line looks.
    let draw = |line: &Line, search: &Option<Search>, ghost: bool, cols: usize, row: usize| {
        let look = |line: &Line| Look {
            colored: Some(highlight(&line.chars)),
            ghost: if ghost && line.cursor == line.chars.len() {
                suggest(history, &line.text()).unwrap_or("").to_string()
            } else {
                String::new()
            },
        };
        match search {
            None => render(prompt, prompt_width, line, &look(line), cols, row),
            Some(s) => {
                let label = if s.found.is_none() && !s.query.is_empty() {
                    "search, no match"
                } else {
                    "search"
                };
                let p = format!("\x1b[2m({label})\x1b[0m {}\x1b[2m:\x1b[0m ", s.query);
                let mut shown = Line::default();
                if let Some(i) = s.found {
                    shown.set(&history[i].replace('\n', " "));
                }
                render(&p, visible_width(&p), &shown, &look(&shown), cols, row)
            }
        }
    };
    let (out, mut row) = draw(&line, &search, true, io.cols(), 0);
    io.write(&out)?;
    let mut buf = [0u8; 1024];
    loop {
        let n = io.read(&mut buf)?;
        if n == 0 {
            return Ok(ReadLine::Eof);
        }
        for key in decoder.feed(&buf[..n]) {
            let cols = io.cols();
            // Keys typed while searching change the search; anything else
            // takes the line found and carries on.
            let key = match search.as_mut() {
                None => Some(key),
                Some(s) => match key {
                    Key::Char(c) => {
                        s.query.push(c);
                        s.found = search_back(history, &s.query, history.len());
                        None
                    }
                    Key::Backspace => {
                        s.query.pop();
                        s.found = search_back(history, &s.query, history.len());
                        None
                    }
                    Key::CtrlR => {
                        if let Some(at) = s.found {
                            s.found = search_back(history, &s.query, at).or(s.found);
                        }
                        None
                    }
                    Key::CtrlG | Key::Escape | Key::CtrlC => {
                        line = s.saved.clone();
                        search = None;
                        None
                    }
                    other => {
                        if let Some(i) = s.found {
                            line.set(&history[i]);
                        }
                        search = None;
                        Some(other)
                    }
                },
            };
            if let Some(key) = key {
                let finish = |line: &Line| {
                    // Put the cursor after the whole line before leaving it.
                    let mut end = line.clone();
                    end.cursor = end.chars.len();
                    draw(&end, &None, false, cols, row).0
                };
                match key {
                    Key::Enter => {
                        io.write(&(finish(&line) + "\r\n"))?;
                        return Ok(ReadLine::Line(line.text()));
                    }
                    Key::CtrlC => {
                        io.write(&(finish(&line) + "^C\r\n"))?;
                        return Ok(ReadLine::Interrupted);
                    }
                    Key::CtrlD if line.chars.is_empty() => {
                        io.write(&(finish(&line) + "\r\n"))?;
                        return Ok(ReadLine::Eof);
                    }
                    Key::CtrlL => {
                        io.write("\x1b[H\x1b[2J")?;
                        row = 0;
                    }
                    Key::CtrlR => {
                        search = Some(Search {
                            query: String::new(),
                            found: None,
                            saved: line.clone(),
                        });
                    }
                    Key::Right | Key::End if line.cursor == line.chars.len() => {
                        // Take the suggestion.
                        if let Some(rest) = suggest(history, &line.text()) {
                            line.insert_str(rest);
                        }
                    }
                    Key::Up | Key::Down => walk.step(&mut line, key == Key::Up),
                    Key::Tab => {
                        let before = &line.chars[..line.cursor];
                        let c = complete(before);
                        let prefix = common_prefix(&c.candidates);
                        let typed: String = line.chars[c.start..line.cursor].iter().collect();
                        if let [only] = c.candidates.as_slice() {
                            let mut text = only.replace.clone();
                            if !only.is_dir {
                                text.push(' ');
                            }
                            line.replace_back_to(c.start, &text);
                        } else if prefix.chars().count() > typed.chars().count() {
                            line.replace_back_to(c.start, &prefix);
                        } else if !c.candidates.is_empty() {
                            let names: Vec<String> =
                                c.candidates.into_iter().map(|c| c.display).collect();
                            io.write(&(finish(&line) + "\r\n" + &columns(&names, cols)))?;
                            row = 0;
                        }
                    }
                    other => {
                        line.edit(other);
                    }
                }
            }
            let (out, r) = draw(&line, &search, true, cols, row);
            row = r;
            io.write(&out)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_keys() {
        let mut d = KeyDecoder::default();
        let keys = d.feed(b"a\x1b[D\x1b[1;5C\x1b[3~\x1bOH\r\n\x7f\t\x03");
        assert_eq!(
            keys,
            vec![
                Key::Char('a'),
                Key::Left,
                Key::WordRight,
                Key::Delete,
                Key::Home,
                Key::Enter,
                Key::Backspace,
                Key::Tab,
                Key::CtrlC
            ]
        );
    }

    #[test]
    fn keeps_split_sequences() {
        let mut d = KeyDecoder::default();
        assert_eq!(d.feed(b"\x1b["), vec![]);
        assert_eq!(d.feed(b"A"), vec![Key::Up]);
        // "é" split in two
        assert_eq!(d.feed(&[0xc3]), vec![]);
        assert_eq!(d.feed(&[0xa9]), vec![Key::Char('é')]);
        // A lone Esc at the end of a read is the Esc key.
        assert_eq!(d.feed(b"\x1b"), vec![Key::Escape]);
    }

    #[test]
    fn edits() {
        let mut l = Line::default();
        for k in "echo hello world".chars() {
            l.edit(Key::Char(k));
        }
        l.edit(Key::KillWordBack);
        assert_eq!(l.text(), "echo hello ");
        l.edit(Key::WordLeft);
        assert_eq!(l.cursor, 5);
        l.edit(Key::Backspace);
        assert_eq!(l.text(), "echohello ");
        l.edit(Key::KillEnd);
        assert_eq!(l.text(), "echo");
        l.edit(Key::Home);
        l.edit(Key::Delete);
        assert_eq!(l.text(), "cho");
        l.edit(Key::Escape);
        assert_eq!(l.text(), "");
    }

    #[test]
    fn widths_skip_escapes() {
        assert_eq!(
            visible_width("\x1b]9;9;C:\\\x07\x1b[1;34mC:\\\x1b[0m ❯ "),
            6
        );
        assert_eq!(str_width("日本"), 4);
    }

    #[test]
    fn render_positions_cursor() {
        let mut l = Line::default();
        l.set("hello");
        l.cursor = 2;
        let (out, row) = render("> ", 2, &l, &Look::default(), 80, 0);
        assert_eq!(out, "\r\x1b[J> hello\r\x1b[4C");
        assert_eq!(row, 0);
    }

    #[test]
    fn render_wraps() {
        let mut l = Line::default();
        l.set("abcdefgh"); // prompt 2 + 8 = 10 cells on 4 columns: 3 rows
        l.cursor = 1; // cell 3: row 0
        let (out, row) = render("> ", 2, &l, &Look::default(), 4, 1);
        assert!(out.starts_with("\x1b[1A\r\x1b[J"));
        assert!(out.ends_with("\x1b[2A\r\x1b[3C"));
        assert_eq!(row, 0);
        // Ending exactly on the edge moves to the next row.
        l.set("ab");
        let (out, row) = render("> ", 2, &l, &Look::default(), 4, 0);
        assert!(out.ends_with(" \r\x1b[K\r"));
        assert_eq!(row, 1);
    }

    #[test]
    fn history_walk() {
        let entries = vec!["one".to_string(), "two".to_string()];
        let mut walk = HistoryWalk::new(&entries);
        let mut l = Line::default();
        l.set("dra");
        walk.step(&mut l, true);
        assert_eq!(l.text(), "two");
        walk.step(&mut l, true);
        walk.step(&mut l, true);
        assert_eq!(l.text(), "one");
        walk.step(&mut l, false);
        walk.step(&mut l, false);
        assert_eq!(l.text(), "dra");
    }

    #[test]
    fn lays_out_columns() {
        let names: Vec<String> = ["a", "bb", "ccc", "d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(columns(&names, 12), "a    bb\r\nccc  d\r\n");
    }

    struct Fake {
        input: Vec<Vec<u8>>,
        output: String,
    }

    impl Io for Fake {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.input.is_empty() {
                return Ok(0);
            }
            let chunk = self.input.remove(0);
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
        fn write(&mut self, s: &str) -> std::io::Result<()> {
            self.output.push_str(s);
            Ok(())
        }
        fn cols(&self) -> usize {
            80
        }
    }

    fn run(input: &[&[u8]], history: &[String]) -> ReadLine {
        let mut io = Fake {
            input: input.iter().map(|c| c.to_vec()).collect(),
            output: String::new(),
        };
        let complete = |chars: &[char]| {
            let word: String = chars.iter().collect();
            let names = ["sort", "sorted", "set"];
            Completion {
                start: 0,
                candidates: names
                    .iter()
                    .filter(|n| n.starts_with(&word))
                    .map(|n| crate::complete::Candidate {
                        replace: n.to_string(),
                        display: n.to_string(),
                        is_dir: false,
                    })
                    .collect(),
            }
        };
        read_line(&mut io, "$ ", history, &complete, &|c| c.iter().collect()).unwrap()
    }

    #[test]
    fn reads_lines() {
        let history = vec!["first".to_string(), "second".to_string()];
        assert_eq!(run(&[b"ls\r"], &[]), ReadLine::Line("ls".into()));
        assert_eq!(
            run(&[b"\x1b[A\x1b[A", b"\r"], &history),
            ReadLine::Line("first".into())
        );
        assert_eq!(run(&[b"oops\x03"], &[]), ReadLine::Interrupted);
        assert_eq!(run(&[b"\x04"], &[]), ReadLine::Eof);
        assert_eq!(run(&[b"abc"], &[]), ReadLine::Eof);
        // Tab: common prefix, then the only match plus a space.
        assert_eq!(
            run(&[b"so\t", b"e\t", b"x\r"], &[]),
            ReadLine::Line("sorted x".into())
        );
    }

    #[test]
    fn suggestions_and_search() {
        let history = vec![
            "git status".to_string(),
            "git push".to_string(),
            "ls".to_string(),
        ];
        assert_eq!(suggest(&history, "git"), Some(" push"));
        assert_eq!(suggest(&history, "git s"), Some("tatus"));
        assert_eq!(suggest(&history, "ls"), None);
        assert_eq!(suggest(&history, ""), None);
        // Right takes the suggestion.
        assert_eq!(
            run(&[b"git s", b"\x1b[C\r"], &history),
            ReadLine::Line("git status".into())
        );
        // Ctrl+R finds the newest match; again goes further back.
        assert_eq!(
            run(&[b"\x12git\r"], &history),
            ReadLine::Line("git push".into())
        );
        assert_eq!(
            run(&[b"\x12git\x12\r"], &history),
            ReadLine::Line("git status".into())
        );
        // A key that isn't for the search takes the match and edits it.
        assert_eq!(
            run(&[b"\x12pu\x1b[D\x7f\r"], &history),
            ReadLine::Line("git puh".into())
        );
        // Esc goes back to what was typed.
        assert_eq!(
            run(&[b"ab\x12git\x1b", b"c\r"], &history),
            ReadLine::Line("abc".into())
        );
    }
}
