//! `img`: show a picture in the terminal, three ways:
//!
//! - `-k`: the kitty graphics protocol (kitty, WezTerm, Ghostty, tacoterm),
//!   real pixels
//! - `-s`: sixel graphics (Windows Terminal 1.22+, and others), real pixels
//! - `-b`: two pixels per character cell, one above the other (▀ in 24-bit
//!   colors), which works in any true-color terminal
//!
//! Without a flag it picks kitty where the terminal is known to speak it
//! and blocks elsewhere; `TACOSHELL_IMG=kitty|sixel|blocks` overrides that.

use image::{imageops::FilterType, RgbaImage};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Kitty,
    Sixel,
    Blocks,
}

/// The mode to use when none is asked for.
pub fn default_mode(env: &dyn Fn(&str) -> Option<String>) -> Mode {
    match env("TACOSHELL_IMG").map(|v| v.to_lowercase()).as_deref() {
        Some("kitty") => return Mode::Kitty,
        Some("sixel") => return Mode::Sixel,
        Some("blocks") => return Mode::Blocks,
        _ => {}
    }
    let program = env("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let term = env("TERM").unwrap_or_default();
    if env("KITTY_WINDOW_ID").is_some()
        || term.contains("kitty")
        || ["tacoterm", "wezterm", "ghostty"].contains(&program.as_str())
    {
        Mode::Kitty
    } else {
        Mode::Blocks
    }
}

pub fn run(
    files: &[String],
    mode: Option<Mode>,
    width: Option<&str>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> i32 {
    if files.is_empty() {
        let _ = writeln!(
            err,
            "img: img [-k | -s | -b] [-w COLUMNS] PICTURE...  (png, jpeg, gif, bmp, webp)"
        );
        return 2;
    }
    let mode = mode.unwrap_or_else(|| default_mode(&|k| std::env::var(k).ok()));
    let width = match width.map(str::parse::<u32>) {
        None => None,
        Some(Ok(w)) if w > 0 => Some(w),
        Some(_) => {
            let _ = writeln!(err, "img: -w takes a width in columns");
            return 2;
        }
    };
    let cols = crate::term::columns() as u32;
    let mut status = 0;
    for file in files {
        let picture = match image::open(file) {
            Ok(p) => p.to_rgba8(),
            Err(e) => {
                let _ = writeln!(err, "img: {file}: {e}");
                status = 1;
                continue;
            }
        };
        match mode {
            Mode::Kitty => {
                // The terminal scales it: to -w columns if asked, and down
                // to the window if it's wider (at ~10 pixels a column).
                let over = picture.width() > cols * 10;
                let columns = width.or(over.then_some(cols));
                match kitty_encode(&picture, columns) {
                    Ok(s) => {
                        let _ = out.write_all(s.as_bytes());
                        let _ = writeln!(out);
                    }
                    Err(e) => {
                        let _ = writeln!(err, "img: {file}: {e}");
                        status = 1;
                    }
                }
            }
            Mode::Sixel => {
                // Roughly 10 pixels per column; never enlarge.
                let max = width.unwrap_or(cols) * 10;
                let _ = out.write_all(sixel_encode(&fit(&picture, max)).as_bytes());
                let _ = writeln!(out);
            }
            Mode::Blocks => {
                let max = width.unwrap_or(cols.saturating_sub(1).max(1));
                let _ = out.write_all(half_blocks(&fit(&picture, max)).as_bytes());
            }
        }
    }
    status
}

/// Shrink to at most `max` pixels wide, keeping the shape.
fn fit(picture: &RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = picture.dimensions();
    if w <= max {
        return picture.clone();
    }
    let new_h = ((h as u64 * max as u64) / w as u64).max(1) as u32;
    image::imageops::resize(picture, max, new_h, FilterType::Triangle)
}

/// Two pixel rows per text row: ▀ in the top pixel's color over the bottom
/// pixel's. See-through pixels show the terminal's own background.
pub fn half_blocks(picture: &RgbaImage) -> String {
    let (w, h) = picture.dimensions();
    let mut s = String::new();
    for y in (0..h).step_by(2) {
        for x in 0..w {
            let top = *picture.get_pixel(x, y);
            let bottom = if y + 1 < h {
                Some(*picture.get_pixel(x, y + 1))
            } else {
                None
            };
            let shown = |p: &image::Rgba<u8>| p[3] >= 128;
            let rgb = |p: &image::Rgba<u8>| format!("{};{};{}", p[0], p[1], p[2]);
            match (shown(&top), bottom.filter(shown)) {
                (true, Some(b)) => s.push_str(&format!(
                    "\x1b[38;2;{}m\x1b[48;2;{}m\u{2580}",
                    rgb(&top),
                    rgb(&b)
                )),
                (true, None) => s.push_str(&format!("\x1b[49m\x1b[38;2;{}m\u{2580}", rgb(&top))),
                (false, Some(b)) => s.push_str(&format!("\x1b[49m\x1b[38;2;{}m\u{2584}", rgb(&b))),
                (false, None) => s.push_str("\x1b[0m "),
            }
        }
        s.push_str("\x1b[0m\n");
    }
    s
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() {
                TABLE[(n >> shift) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The kitty graphics protocol: the picture as PNG, in base64, in chunks of
/// at most 4096 bytes. `q=2` keeps the terminal from answering (the answer
/// would land in the shell's input).
pub fn kitty_encode(picture: &RgbaImage, columns: Option<u32>) -> image::ImageResult<String> {
    let mut png = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    let data = base64(&png);
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let mut s = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        if i == 0 {
            let cols = columns.map_or(String::new(), |c| format!(",c={c}"));
            s.push_str(&format!("\x1b_Ga=T,f=100,q=2{cols},m={more};{chunk}\x1b\\"));
        } else {
            s.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    Ok(s)
}

/// Sixel graphics with a 6×6×6 color cube palette. See-through pixels are
/// left unpainted.
pub fn sixel_encode(picture: &RgbaImage) -> String {
    let (w, h) = picture.dimensions();
    let level = |v: u8| (v as u32 * 5 + 127) / 255;
    let index = |p: &image::Rgba<u8>| {
        (p[3] >= 128).then(|| (level(p[0]) * 36 + level(p[1]) * 6 + level(p[2])) as usize)
    };
    let mut s = format!("\x1bP0;1;0q\"1;1;{w};{h}");
    for i in 0..216u32 {
        let pct = |l: u32| l * 100 / 5;
        s.push_str(&format!(
            "#{i};2;{};{};{}",
            pct(i / 36),
            pct(i / 6 % 6),
            pct(i % 6)
        ));
    }
    for band in (0..h).step_by(6) {
        // For each color in this band, a row of sixels: bit n is pixel row
        // band + n.
        let mut rows: Vec<Option<Vec<u8>>> = vec![None; 216];
        for x in 0..w {
            for bit in 0..6 {
                let y = band + bit;
                if y >= h {
                    break;
                }
                if let Some(c) = index(picture.get_pixel(x, y)) {
                    rows[c].get_or_insert_with(|| vec![0; w as usize])[x as usize] |= 1 << bit;
                }
            }
        }
        let mut first = true;
        for (c, row) in rows.iter().enumerate() {
            let Some(row) = row else { continue };
            if !first {
                s.push('$');
            }
            first = false;
            s.push_str(&format!("#{c}"));
            // Run-length encode: !count char.
            let mut x = 0;
            while x < row.len() {
                let run = row[x..].iter().take_while(|&&b| b == row[x]).count();
                let ch = (63 + row[x]) as char;
                if run > 3 {
                    s.push_str(&format!("!{run}{ch}"));
                } else {
                    s.extend(std::iter::repeat_n(ch, run));
                }
                x += run;
            }
        }
        s.push('-');
    }
    s.push_str("\x1b\\");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn picture() -> RgbaImage {
        // 2×3: red over blue, then a see-through pixel and green.
        let mut p = RgbaImage::new(2, 3);
        p.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        p.put_pixel(0, 1, Rgba([0, 0, 255, 255]));
        p.put_pixel(1, 0, Rgba([0, 0, 0, 0]));
        p.put_pixel(1, 1, Rgba([0, 255, 0, 255]));
        p.put_pixel(0, 2, Rgba([255, 255, 255, 255]));
        p
    }

    #[test]
    fn half_block_rows() {
        let s = half_blocks(&picture());
        let rows: Vec<&str> = s.lines().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            "\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m\u{2580}\x1b[49m\x1b[38;2;0;255;0m\u{2584}\x1b[0m"
        );
        assert_eq!(
            rows[1],
            "\x1b[49m\x1b[38;2;255;255;255m\u{2580}\x1b[0m \x1b[0m"
        );
    }

    #[test]
    fn sixel_bands() {
        let s = sixel_encode(&picture());
        assert!(s.starts_with("\x1bP0;1;0q\"1;1;2;3#0;2;0;0;0"));
        assert!(s.ends_with("\x1b\\"));
        // Red (index 180) paints row 0 of column 0: bit 0 → '@', column 1 → '?'.
        assert!(s.contains("#180@?"));
        // White (215) paints row 2 of column 0.
        assert!(s.contains("#215C?"));
        assert_eq!(s.matches('-').count(), 1);
    }

    #[test]
    fn kitty_chunks() {
        let s = kitty_encode(&picture(), Some(4)).unwrap();
        assert!(s.starts_with("\x1b_Ga=T,f=100,q=2,c=4,m=0;iVBORw0KGgo"));
        assert!(s.ends_with("\x1b\\"));
        // A big, noisy picture spans several chunks; only the last has m=0.
        let big = RgbaImage::from_fn(200, 200, |x, y| {
            Rgba([
                ((x * 7) ^ (y * 13)) as u8,
                (x * y) as u8,
                (x + y) as u8,
                255,
            ])
        });
        let s = kitty_encode(&big, None).unwrap();
        let parts: Vec<&str> = s.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert!(parts.len() > 2);
        assert!(parts[0].starts_with("\x1b_Ga=T,f=100,q=2,m=1;"));
        assert!(parts[1..parts.len() - 1]
            .iter()
            .all(|p| p.starts_with("\x1b_Gm=1;")));
        assert!(parts.last().unwrap().starts_with("\x1b_Gm=0;"));
        assert!(parts
            .iter()
            .all(|p| p.split_once(';').unwrap().1.len() <= 4096));
        assert_eq!(base64(b"taco"), "dGFjbw==");
    }

    #[test]
    fn picks_a_mode() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(default_mode(&env(&[])), Mode::Blocks);
        assert_eq!(
            default_mode(&env(&[("TERM_PROGRAM", "tacoterm")])),
            Mode::Kitty
        );
        assert_eq!(default_mode(&env(&[("TERM", "xterm-kitty")])), Mode::Kitty);
        assert_eq!(
            default_mode(&env(&[
                ("TERM_PROGRAM", "WezTerm"),
                ("TACOSHELL_IMG", "sixel")
            ])),
            Mode::Sixel
        );
    }

    #[test]
    fn shrinks_to_fit() {
        let p = RgbaImage::new(100, 50);
        assert_eq!(fit(&p, 20).dimensions(), (20, 10));
        assert_eq!(fit(&p, 200).dimensions(), (100, 50));
    }
}
