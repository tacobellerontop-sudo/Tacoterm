//! Pictures in the terminal: the kitty graphics protocol, sixel and
//! iTerm2's inline images.
//!
//! Each protocol decodes to an RGBA [`Image`]; a [`Placement`] pins one to
//! a cell of the grid's history, so it scrolls with the text around it.
//! The renderer draws placements over the cell backgrounds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::grid::Grid;
use crate::vt::base64_decode;

/// Pictures wider or taller than this are refused.
const MAX_SIDE: u32 = 10_000;
/// Image memory one terminal may hold, in bytes of RGBA.
const BUDGET: usize = 320 << 20;
/// The most a kitty transmission (all chunks, base64) may carry.
const MAX_PAYLOAD: usize = 400 << 20;

/// A decoded picture.
pub struct Image {
    /// Unique for the life of the process; the renderer keys its GPU
    /// textures on it.
    pub uid: u64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 rows, not premultiplied.
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Arc<Image> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Image {
            uid: NEXT.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            rgba,
        })
    }

    fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// A picture shown at a spot in the grid.
#[derive(Clone)]
pub struct Placement {
    pub image: Arc<Image>,
    /// kitty's image and placement ids (0 for the other protocols).
    pub image_id: u32,
    pub placement_id: u32,
    /// The history line (as in [`crate::grid::Point::line`]) and column of
    /// the top-left cell.
    pub line: usize,
    pub col: usize,
    /// The cells it covers.
    pub cols: usize,
    pub rows: usize,
    /// The part of the image shown, in image pixels: x, y, w, h.
    pub src: [u32; 4],
    /// Where in the first cell it starts, in pixels.
    pub offset: (u32, u32),
    /// Stretched to fill `cols`×`rows` cells. Otherwise drawn at
    /// `draw_size` pixels, or the shown part's own size.
    pub fill: bool,
    pub draw_size: Option<(u32, u32)>,
    pub z: i32,
}

/// Where the cursor goes after a picture lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorAfter {
    Stay,
    /// Just right of the picture, on its last row (kitty, iTerm2).
    Right,
    /// The picture's first column, on the row after it (sixel), so what
    /// comes next starts below the picture.
    Below,
}

/// A terminal's pictures.
#[derive(Default)]
pub struct Graphics {
    pub placements: Vec<Placement>,
    /// The primary screen's placements while the alternate screen is up.
    pub parked: Option<Vec<Placement>>,
    /// kitty images by id, placed or not.
    images: HashMap<u32, Arc<Image>>,
    /// kitty image numbers (`I=`) to the id they were given.
    numbers: HashMap<u32, u32>,
    next_id: u32,
    /// A kitty transmission still arriving in chunks.
    pending: Option<(Keys, Vec<u8>)>,
}

impl Graphics {
    /// Make room after something new was added: drop the oldest
    /// placements, then images nothing shows, until under budget.
    fn enforce_budget(&mut self) {
        loop {
            let mut seen = std::collections::HashSet::new();
            let total: usize = self
                .placements
                .iter()
                .map(|p| &p.image)
                .chain(self.parked.iter().flatten().map(|p| &p.image))
                .chain(self.images.values())
                .filter(|i| seen.insert(i.uid))
                .map(|i| i.bytes())
                .sum();
            if total <= BUDGET {
                return;
            }
            let shown: std::collections::HashSet<u64> =
                self.placements.iter().map(|p| p.image.uid).collect();
            if let Some(id) = self
                .images
                .iter()
                .find(|(_, i)| !shown.contains(&i.uid))
                .map(|(id, _)| *id)
            {
                self.images.remove(&id);
            } else if !self.placements.is_empty() {
                self.placements.remove(0);
            } else {
                self.images.clear();
                return;
            }
        }
    }
}

// ---- the grid side -------------------------------------------------------

impl Grid {
    /// Show `p` at the cursor and move the cursor past it.
    pub fn place_image(&mut self, mut p: Placement, after: CursorAfter) {
        p.line = self.cursor_line();
        p.col = self.cursor_x;
        let (col, rows, cols) = (p.col, p.rows.max(1), p.cols.max(1));
        if p.placement_id != 0 {
            self.graphics
                .placements
                .retain(|q| !(q.image_id == p.image_id && q.placement_id == p.placement_id));
        }
        self.graphics.placements.push(p);
        self.graphics.enforce_budget();
        if after == CursorAfter::Stay {
            return;
        }
        // Down to the last row (or past it), scrolling like text would.
        let down = if after == CursorAfter::Below {
            rows
        } else {
            rows - 1
        };
        for _ in 0..down {
            self.line_feed();
        }
        let y = self.cursor_y;
        let x = match after {
            CursorAfter::Right => (col + cols).min(self.cols - 1),
            _ => col,
        };
        self.move_cursor_to(x, y);
    }

    /// Placements on screen in the current view, with the viewport row of
    /// their top (negative when it has scrolled off the top).
    pub fn visible_images(&self) -> Vec<(&Placement, isize)> {
        let top = self.view_top() as isize;
        let mut out: Vec<(&Placement, isize)> = self
            .graphics
            .placements
            .iter()
            .filter_map(|p| {
                let y = p.line as isize - top;
                (y < self.rows as isize && y + p.rows as isize > 0).then_some((p, y))
            })
            .collect();
        out.sort_by_key(|(p, _)| p.z);
        out
    }

    /// Placements are pinned to history lines; move the ones in history
    /// lines `first..=last` by `delta` (a scroll region scrolling, where
    /// the text moves but the line numbers stay), dropping any pushed out.
    pub(crate) fn shift_images(&mut self, first: usize, last: usize, delta: isize) {
        if self.graphics.placements.is_empty() {
            return;
        }
        self.graphics.placements.retain_mut(|p| {
            if p.line < first || p.line > last {
                return true;
            }
            let line = p.line as isize + delta;
            if line < first as isize || line > last as isize {
                return false;
            }
            p.line = line as usize;
            true
        });
    }

    /// Drop placements that touch history lines `first..end`.
    pub(crate) fn clear_images(&mut self, first: usize, end: usize) {
        self.graphics
            .placements
            .retain(|p| p.line + p.rows <= first || p.line >= end);
    }
}

// ---- kitty -------------------------------------------------------------

/// The keys of a kitty graphics command (`ESC _ G keys ; payload ESC \`).
#[derive(Clone, Default, Debug)]
struct Keys {
    action: u8,
    format: u32,
    medium: u8,
    compression: u8,
    width: u32,
    height: u32,
    size: u32,
    offset: u32,
    id: u32,
    number: u32,
    placement: u32,
    more: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    cell_x: u32,
    cell_y: u32,
    cols: u32,
    rows: u32,
    cursor: u32,
    z: i32,
    quiet: u32,
    delete: u8,
}

fn parse_keys(text: &[u8]) -> Keys {
    let mut k = Keys {
        action: b't',
        format: 32,
        medium: b'd',
        delete: b'a',
        ..Keys::default()
    };
    for pair in text.split(|&b| b == b',') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        let num = || {
            std::str::from_utf8(value)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        };
        let ch = value.first().copied().unwrap_or(0);
        match key {
            b"a" => k.action = ch,
            b"f" => k.format = num().unwrap_or(32),
            b"t" => k.medium = ch,
            b"o" => k.compression = ch,
            b"s" => k.width = num().unwrap_or(0),
            b"v" => k.height = num().unwrap_or(0),
            b"S" => k.size = num().unwrap_or(0),
            b"O" => k.offset = num().unwrap_or(0),
            b"i" => k.id = num().unwrap_or(0),
            b"I" => k.number = num().unwrap_or(0),
            b"p" => k.placement = num().unwrap_or(0),
            b"m" => k.more = ch == b'1',
            b"x" => k.x = num().unwrap_or(0),
            b"y" => k.y = num().unwrap_or(0),
            b"w" => k.w = num().unwrap_or(0),
            b"h" => k.h = num().unwrap_or(0),
            b"X" => k.cell_x = num().unwrap_or(0),
            b"Y" => k.cell_y = num().unwrap_or(0),
            b"c" => k.cols = num().unwrap_or(0),
            b"r" => k.rows = num().unwrap_or(0),
            b"C" => k.cursor = num().unwrap_or(0),
            b"z" => {
                k.z = std::str::from_utf8(value)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            }
            b"q" => k.quiet = num().unwrap_or(0),
            b"d" => k.delete = ch,
            _ => {}
        }
    }
    k
}

/// Run a kitty graphics command: the APC body after `G`. Returns the reply
/// to send back, if any.
pub fn kitty(grid: &mut Grid, body: &[u8]) -> Option<Vec<u8>> {
    let (head, payload) = match body.iter().position(|&b| b == b';') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &[][..]),
    };
    let keys = parse_keys(head);
    let g = &mut grid.graphics;
    // Chunked transmissions: the first chunk has the keys, the rest only m.
    let (keys, data) = if let Some((first, mut data)) = g.pending.take() {
        data.extend_from_slice(payload);
        if data.len() > MAX_PAYLOAD {
            return reply(&first, Err("EFBIG:image too large"));
        }
        if keys.more {
            g.pending = Some((first, data));
            return None;
        }
        (first, data)
    } else if keys.more && matches!(keys.action, b't' | b'T' | b'q') {
        g.pending = Some((keys, payload.to_vec()));
        return None;
    } else {
        (keys, payload.to_vec())
    };

    match keys.action {
        b'q' => {
            let result = load(&keys, &data).map(|_| ());
            reply(&keys, result)
        }
        b't' | b'T' => {
            let image = match load(&keys, &data) {
                Ok(i) => i,
                Err(e) => return reply(&keys, Err(e)),
            };
            let mut keys = keys;
            let g = &mut grid.graphics;
            if keys.id == 0 {
                g.next_id = g.next_id.wrapping_add(1).max(1);
                // Ids we pick live far from the small ones programs choose.
                keys.id = 0x8000_0000 | g.next_id;
                if keys.number != 0 {
                    g.numbers.insert(keys.number, keys.id);
                }
            }
            g.images.insert(keys.id, image.clone());
            g.enforce_budget();
            if keys.action == b'T' {
                place(grid, &keys, image);
            }
            reply(&keys, Ok(()))
        }
        b'p' => {
            let g = &grid.graphics;
            let id = if keys.id != 0 {
                keys.id
            } else {
                g.numbers.get(&keys.number).copied().unwrap_or(0)
            };
            let Some(image) = g.images.get(&id).cloned() else {
                return reply(&keys, Err("ENOENT:no such image"));
            };
            let keys = Keys { id, ..keys };
            place(grid, &keys, image);
            reply(&keys, Ok(()))
        }
        b'd' => {
            delete(grid, &keys);
            None
        }
        // Animation frames and the rest: not supported, quietly.
        _ => None,
    }
}

/// The reply kitty sends for a command, if the program asked for one.
fn reply(keys: &Keys, result: Result<(), &str>) -> Option<Vec<u8>> {
    if keys.id == 0 && keys.number == 0 {
        return None;
    }
    match (&result, keys.quiet) {
        (_, 2) | (Ok(()), 1) => return None,
        _ => {}
    }
    let mut out = format!("\x1b_Gi={}", keys.id);
    if keys.number != 0 {
        out += &format!(",I={}", keys.number);
    }
    if keys.placement != 0 {
        out += &format!(",p={}", keys.placement);
    }
    out += ";";
    out += result.err().unwrap_or("OK");
    out += "\x1b\\";
    Some(out.into_bytes())
}

/// Decode a transmission's pixels.
fn load(keys: &Keys, data: &[u8]) -> Result<Arc<Image>, &'static str> {
    let clean: Vec<u8> = data
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let raw = base64_decode(&clean).ok_or("EINVAL:bad base64")?;
    let bytes = match keys.medium {
        b'd' => raw,
        b'f' | b't' => {
            let path = String::from_utf8(raw).map_err(|_| "EINVAL:bad path")?;
            let mut bytes = std::fs::read(&path).map_err(|_| "EBADF:cannot read file")?;
            // A temporary file is ours to delete, if it looks like one.
            if keys.medium == b't' && path.contains("tty-graphics-protocol") {
                let _ = std::fs::remove_file(&path);
            }
            let start = (keys.offset as usize).min(bytes.len());
            let end = if keys.size > 0 {
                (start + keys.size as usize).min(bytes.len())
            } else {
                bytes.len()
            };
            bytes.truncate(end);
            bytes.drain(..start);
            bytes
        }
        _ => return Err("EINVAL:transmission medium not supported"),
    };
    let bytes = if keys.compression == b'z' {
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&bytes, MAX_PAYLOAD)
            .map_err(|_| "EINVAL:bad zlib data")?
    } else {
        bytes
    };
    match keys.format {
        100 => decode_file(&bytes).ok_or("EBADPNG:cannot decode PNG"),
        24 | 32 => {
            let (w, h) = (keys.width, keys.height);
            if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
                return Err("EINVAL:bad size");
            }
            let bpp = if keys.format == 24 { 3 } else { 4 };
            let n = (w * h) as usize;
            if bytes.len() < n * bpp {
                return Err("ENODATA:not enough pixel data");
            }
            let rgba = if bpp == 4 {
                bytes[..n * 4].to_vec()
            } else {
                bytes[..n * 3]
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|&[r, g, b]| [r, g, b, 255])
                    .collect()
            };
            Ok(Image::new(w, h, rgba))
        }
        _ => Err("EINVAL:unknown format"),
    }
}

/// Decode an image file (PNG, JPEG, GIF, BMP, WebP).
pub fn decode_file(bytes: &[u8]) -> Option<Arc<Image>> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    Some(Image::new(w, h, img.into_rgba8().into_raw()))
}

/// The number of cells `px` pixels covers.
fn cells(px: u32, cell: u32) -> usize {
    px.div_ceil(cell.max(1)).max(1) as usize
}

fn place(grid: &mut Grid, keys: &Keys, image: Arc<Image>) {
    let (cw, ch) = grid.cell_pixels;
    let x = keys.x.min(image.width - 1);
    let y = keys.y.min(image.height - 1);
    let w = if keys.w == 0 {
        image.width - x
    } else {
        keys.w.min(image.width - x)
    };
    let h = if keys.h == 0 {
        image.height - y
    } else {
        keys.h.min(image.height - y)
    };
    // c and r stretch it over that many cells; given one, the other keeps
    // the picture's shape.
    let aspect = h as f32 / w as f32;
    let (cols, rows) = match (keys.cols, keys.rows) {
        (0, 0) => (cells(w + keys.cell_x, cw), cells(h + keys.cell_y, ch)),
        (c, 0) => {
            let px = c as f32 * cw as f32 * aspect;
            (c as usize, cells(px.round() as u32, ch))
        }
        (0, r) => {
            let px = r as f32 * ch as f32 / aspect;
            (cells(px.round() as u32, cw), r as usize)
        }
        (c, r) => (c as usize, r as usize),
    };
    let fill = keys.cols != 0 || keys.rows != 0;
    let after = if keys.cursor == 1 {
        CursorAfter::Stay
    } else {
        CursorAfter::Right
    };
    grid.place_image(
        Placement {
            image,
            image_id: keys.id,
            placement_id: keys.placement,
            line: 0,
            col: 0,
            cols,
            rows,
            src: [x, y, w, h],
            offset: (keys.cell_x.min(cw), keys.cell_y.min(ch)),
            fill,
            draw_size: None,
            z: keys.z,
        },
        after,
    );
}

fn delete(grid: &mut Grid, keys: &Keys) {
    let free = keys.delete.is_ascii_uppercase();
    let what = keys.delete.to_ascii_lowercase();
    let (cx, cy) = (grid.cursor_x, grid.cursor_line());
    let top = grid.view_top();
    let id_of_number = grid.graphics.numbers.get(&keys.number).copied();
    let covers = |p: &Placement, col: usize, line: usize| {
        (p.col..p.col + p.cols).contains(&col) && (p.line..p.line + p.rows).contains(&line)
    };
    let gone = |p: &Placement| match what {
        b'a' => true,
        b'i' => p.image_id == keys.id && (keys.placement == 0 || p.placement_id == keys.placement),
        b'n' => Some(p.image_id) == id_of_number,
        b'c' => covers(p, cx, cy),
        b'p' => covers(
            p,
            (keys.x as usize).saturating_sub(1),
            top + (keys.y as usize).saturating_sub(1),
        ),
        b'x' => (p.col..p.col + p.cols).contains(&(keys.x as usize).saturating_sub(1)),
        b'y' => (p.line..p.line + p.rows).contains(&(top + (keys.y as usize).saturating_sub(1))),
        b'z' => p.z == keys.z,
        _ => false,
    };
    let g = &mut grid.graphics;
    let mut touched = Vec::new();
    g.placements.retain(|p| {
        let drop = gone(p);
        if drop {
            touched.push(p.image_id);
        }
        !drop
    });
    if free {
        if what == b'i' {
            touched.push(keys.id);
        }
        if let Some(id) = id_of_number.filter(|_| what == b'n') {
            touched.push(id);
        }
        for id in touched {
            if !g.placements.iter().any(|p| p.image_id == id) {
                g.images.remove(&id);
                g.numbers.retain(|_, v| *v != id);
            }
        }
    }
}

// ---- sixel ---------------------------------------------------------------

/// The VT340's default 16 colors, as percentages.
const SIXEL_PALETTE: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (20, 20, 80),
    (80, 13, 13),
    (20, 80, 20),
    (80, 20, 80),
    (20, 80, 80),
    (80, 80, 20),
    (53, 53, 53),
    (26, 26, 26),
    (33, 33, 60),
    (60, 26, 26),
    (33, 60, 33),
    (60, 33, 60),
    (33, 60, 60),
    (60, 60, 33),
    (80, 80, 80),
];

fn pct(v: u32) -> u8 {
    (v.min(100) * 255 / 100) as u8
}

/// DEC's HLS (hue 0 = blue) to RGB.
fn hls(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = ((h as f32 + 240.0) % 360.0) / 360.0;
    let (l, s) = (l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return [v; 3];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u8
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
}

/// A sixel picture being decoded (the body of `DCS … q … ST`).
pub struct Sixel {
    palette: Vec<[u8; 3]>,
    color: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    /// Declared by raster attributes, if any.
    declared: (usize, usize),
    /// RGBA, `stride` pixels per row; grows as drawing reaches further.
    pixels: Vec<u8>,
    stride: usize,
    /// A command being read: its introducer and numbers so far.
    command: u8,
    numbers: Vec<u32>,
    repeat: usize,
    too_big: bool,
}

impl Default for Sixel {
    fn default() -> Self {
        Self::new()
    }
}

impl Sixel {
    pub fn new() -> Sixel {
        let mut palette: Vec<[u8; 3]> = SIXEL_PALETTE
            .iter()
            .map(|&(r, g, b)| [pct(r as u32), pct(g as u32), pct(b as u32)])
            .collect();
        palette.resize(1024, [0, 0, 0]);
        Sixel {
            palette,
            color: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            declared: (0, 0),
            pixels: Vec::new(),
            stride: 0,
            command: 0,
            numbers: Vec::new(),
            repeat: 1,
            too_big: false,
        }
    }

    pub fn put(&mut self, b: u8) {
        if b.is_ascii_digit() && self.command != 0 {
            let n = self.numbers.last_mut();
            match n {
                Some(n) => *n = n.saturating_mul(10).saturating_add((b - b'0') as u32),
                None => self.numbers.push((b - b'0') as u32),
            }
            return;
        }
        if b == b';' && self.command != 0 {
            self.numbers.push(0);
            return;
        }
        self.finish_command();
        match b {
            b'#' | b'!' | b'"' => {
                self.command = b;
                self.numbers.clear();
            }
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.y += 6;
            }
            0x3f..=0x7e => {
                let n = std::mem::replace(&mut self.repeat, 1);
                self.draw(b - 0x3f, n);
            }
            _ => {}
        }
    }

    fn finish_command(&mut self) {
        let numbers = std::mem::take(&mut self.numbers);
        let at = |i: usize| numbers.get(i).copied().unwrap_or(0);
        match std::mem::take(&mut self.command) {
            b'#' => {
                let reg = at(0) as usize % self.palette.len();
                if numbers.len() >= 5 {
                    self.palette[reg] = match at(1) {
                        1 => hls(at(2), at(3), at(4)),
                        _ => [pct(at(2)), pct(at(3)), pct(at(4))],
                    };
                }
                self.color = reg;
            }
            b'!' => self.repeat = (at(0) as usize).clamp(1, MAX_SIDE as usize),
            b'"' => {
                let (w, h) = (at(2) as usize, at(3) as usize);
                if w <= MAX_SIDE as usize && h <= MAX_SIDE as usize {
                    self.declared = (w, h);
                }
            }
            _ => {}
        }
    }

    /// Paint one sixel (six pixels stacked, bit 0 on top), `n` times.
    fn draw(&mut self, bits: u8, n: usize) {
        if bits == 0 {
            self.x += n;
            return;
        }
        let (x_end, y_end) = (self.x + n, self.y + 6);
        if x_end > MAX_SIDE as usize || y_end > MAX_SIDE as usize {
            self.too_big = true;
            return;
        }
        self.grow(x_end, y_end);
        let [r, g, b] = self.palette[self.color];
        for dy in 0..6 {
            if bits & (1 << dy) == 0 {
                continue;
            }
            let row = (self.y + dy) * self.stride;
            for x in self.x..x_end {
                let i = (row + x) * 4;
                self.pixels[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
            self.height = self.height.max(self.y + dy + 1);
        }
        self.width = self.width.max(x_end);
        self.x = x_end;
    }

    /// Make the canvas at least `w`×`h`.
    fn grow(&mut self, w: usize, h: usize) {
        let rows = (self.pixels.len() / 4)
            .checked_div(self.stride)
            .unwrap_or(0);
        if w <= self.stride && h <= rows {
            return;
        }
        let stride = w
            .max(self.stride)
            .max(self.declared.0)
            .next_power_of_two()
            .min(MAX_SIDE as usize);
        let new_rows = h.max(rows).max(self.declared.1);
        let mut pixels = vec![0u8; stride * new_rows * 4];
        for y in 0..rows {
            let from = y * self.stride * 4;
            pixels[y * stride * 4..y * stride * 4 + self.stride * 4]
                .copy_from_slice(&self.pixels[from..from + self.stride * 4]);
        }
        self.pixels = pixels;
        self.stride = stride;
    }

    /// The finished picture; pixels never painted are transparent.
    pub fn finish(mut self) -> Option<Arc<Image>> {
        self.finish_command();
        if self.too_big {
            return None;
        }
        let w = self.width.max(self.declared.0);
        let h = self.height.max(self.declared.1);
        if w == 0 || h == 0 {
            return None;
        }
        self.grow(w, h);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let from = y * self.stride * 4;
            rgba.extend_from_slice(&self.pixels[from..from + w * 4]);
        }
        Some(Image::new(w as u32, h as u32, rgba))
    }
}

/// Show a finished sixel picture at the cursor.
pub fn place_sixel(grid: &mut Grid, image: Arc<Image>) {
    let (cw, ch) = grid.cell_pixels;
    let (w, h) = (image.width, image.height);
    grid.place_image(
        Placement {
            image,
            image_id: 0,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: cells(w, cw),
            rows: cells(h, ch),
            src: [0, 0, w, h],
            offset: (0, 0),
            fill: false,
            draw_size: None,
            z: 0,
        },
        CursorAfter::Below,
    );
}

// ---- iTerm2 --------------------------------------------------------------

/// A size in an iTerm2 `width=` / `height=`: cells, pixels, a percentage
/// of the screen, or auto.
fn iterm_size(spec: &str, cell: u32, screen_cells: usize) -> Option<u32> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("auto") {
        None
    } else if let Some(px) = spec.strip_suffix("px") {
        px.parse().ok()
    } else if let Some(p) = spec.strip_suffix('%') {
        let p: f32 = p.parse().ok()?;
        Some((p / 100.0 * (screen_cells as u32 * cell) as f32) as u32)
    } else {
        spec.parse::<u32>().ok().map(|n| n * cell)
    }
}

/// Run iTerm2's `OSC 1337 ; File=args:base64`: `text` is everything after
/// `File=`.
pub fn iterm_file(grid: &mut Grid, text: &[u8]) {
    let Some(colon) = text.iter().position(|&b| b == b':') else {
        return;
    };
    let (args, data) = (&text[..colon], &text[colon + 1..]);
    let args = String::from_utf8_lossy(args);
    let arg = |name: &str| {
        args.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.to_string())
        })
    };
    // Without inline=1 it's a download, which we don't do.
    if arg("inline").as_deref() != Some("1") {
        return;
    }
    let clean: Vec<u8> = data
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let Some(image) = base64_decode(&clean).and_then(|b| decode_file(&b)) else {
        return;
    };
    let (cw, ch) = grid.cell_pixels;
    let (iw, ih) = (image.width as f32, image.height as f32);
    let want_w = arg("width").and_then(|s| iterm_size(&s, cw, grid.cols));
    let want_h = arg("height").and_then(|s| iterm_size(&s, ch, grid.rows));
    let keep = arg("preserveAspectRatio").as_deref() != Some("0");
    // Never wider than the screen unless asked.
    let max_w = (grid.cols as u32 * cw) as f32;
    let (w, h) = match (want_w, want_h) {
        (None, None) => {
            let s = (max_w / iw).min(1.0);
            (iw * s, ih * s)
        }
        (Some(w), None) => (w as f32, if keep { w as f32 * ih / iw } else { ih }),
        (None, Some(h)) => (if keep { h as f32 * iw / ih } else { iw }, h as f32),
        (Some(w), Some(h)) if keep => {
            let s = (w as f32 / iw).min(h as f32 / ih);
            (iw * s, ih * s)
        }
        (Some(w), Some(h)) => (w as f32, h as f32),
    };
    let (w, h) = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
    let src = [0, 0, image.width, image.height];
    grid.place_image(
        Placement {
            image,
            image_id: 0,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: cells(w, cw),
            rows: cells(h, ch),
            src,
            offset: (0, 0),
            fill: false,
            draw_size: Some((w, h)),
            z: 0,
        },
        CursorAfter::Right,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = (chunk[0] as u32) << 16
                | (*chunk.get(1).unwrap_or(&0) as u32) << 8
                | *chunk.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn grid() -> Grid {
        let mut g = Grid::new(80, 24, 100);
        g.cell_pixels = (10, 20);
        g
    }

    #[test]
    fn kitty_png_in_chunks_with_columns() {
        let mut g = grid();
        let data = b64(&png(40, 40));
        let (a, b) = data.split_at(16);
        assert!(kitty(&mut g, format!("a=T,f=100,q=2,c=8,m=1;{a}").as_bytes()).is_none());
        assert!(kitty(&mut g, format!("m=0;{b}").as_bytes()).is_none());
        let shown = g.visible_images();
        assert_eq!(shown.len(), 1);
        let (p, y) = shown[0];
        assert_eq!(
            (p.cols, p.rows, y),
            (8, 4, 0),
            "square: 8 cols of 10px = 4 rows of 20px"
        );
        assert_eq!((g.cursor_x, g.cursor_y), (8, 3));
    }

    #[test]
    fn kitty_raw_rgb_reply_and_delete() {
        let mut g = grid();
        let pixels = vec![9u8; 2 * 3 * 3];
        let r = kitty(
            &mut g,
            format!("a=t,f=24,s=2,v=3,i=7;{}", b64(&pixels)).as_bytes(),
        );
        assert_eq!(r.unwrap(), b"\x1b_Gi=7;OK\x1b\\");
        assert!(g.visible_images().is_empty(), "transmit only");
        kitty(&mut g, b"a=p,i=7,p=1,q=1");
        kitty(&mut g, b"a=p,i=7,p=1,q=1");
        assert_eq!(g.visible_images().len(), 1, "same placement id replaces");
        let r = kitty(&mut g, b"a=p,i=99");
        assert_eq!(r.unwrap(), b"\x1b_Gi=99;ENOENT:no such image\x1b\\");
        kitty(&mut g, b"a=d,d=I,i=7");
        assert!(g.visible_images().is_empty());
        assert!(kitty(&mut g, b"a=p,i=7").is_some_and(|r| r.windows(6).any(|w| w == b"ENOENT")));
    }

    #[test]
    fn images_scroll_with_text_and_clear() {
        let mut g = grid();
        kitty(
            &mut g,
            format!("a=T,f=100,q=2;{}", b64(&png(10, 40))).as_bytes(),
        );
        assert_eq!(g.visible_images()[0].1, 0);
        for _ in 0..30 {
            g.line_feed();
        }
        assert!(g.visible_images().is_empty(), "scrolled off");
        g.scroll_view(30);
        assert_eq!(g.visible_images().len(), 1, "still in history");
        g.scroll_view_to_bottom();
        kitty(
            &mut g,
            format!("a=T,f=100,q=2;{}", b64(&png(10, 20))).as_bytes(),
        );
        assert_eq!(g.visible_images().len(), 1);
        g.erase_display(2);
        assert!(g.visible_images().is_empty());
    }

    #[test]
    fn sixel_decodes_colors_and_repeats() {
        let mut s = Sixel::new();
        // Color 1 = pure red (RGB percents), 3 columns full, newline, 1 more.
        for &b in b"\"1;1;4;12#1;2;100;0;0#1!3~-~" {
            s.put(b);
        }
        let img = s.finish().unwrap();
        assert_eq!((img.width, img.height), (4, 12), "raster size wins");
        assert_eq!(&img.rgba[..4], &[255, 0, 0, 255]);
        // Row 6 has only its first pixel painted.
        let row6 = 6 * 4 * 4;
        assert_eq!(img.rgba[row6 + 3], 255);
        assert_eq!(img.rgba[row6 + 4 + 3], 0, "unpainted is clear");
    }

    #[test]
    fn iterm_inline_file() {
        let mut g = grid();
        let body = format!("inline=1;width=5:{}", b64(&png(100, 50)));
        iterm_file(&mut g, body.as_bytes());
        let (p, _) = g.visible_images()[0];
        assert_eq!((p.cols, p.rows), (5, 2), "5 cells wide, 25px tall");
        assert_eq!(p.draw_size, Some((50, 25)));
    }
}
