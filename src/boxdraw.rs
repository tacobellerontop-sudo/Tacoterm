//! Procedurally drawn glyphs: box drawing, block elements and Powerline
//! triangles.
//!
//! Fonts draw these at their own size and position, so lines rarely meet
//! across cell boundaries and blocks leave hairline seams. Drawing them from
//! the cell size instead makes a horizontal line in one cell continue exactly
//! into the next and a vertical line run edge to edge. Every glyph comes out
//! as a coverage mask: one byte per pixel, row-major, 0 (empty) to 255 (full).

/// Whether `c` is drawn by this module rather than the font.
pub fn handles(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259F | 0xE0B0..=0xE0B3)
}

/// Coverage mask for `c` in a cell of `width`×`height` pixels, or `None` if
/// [`handles`] says no.
///
/// `thin` is the light line thickness in pixels (at least 1); heavy lines are
/// twice that and double lines are two thin lines with a thin gap.
pub fn draw(c: char, width: u32, height: u32, thin: u32) -> Option<Vec<u8>> {
    if !handles(c) {
        return None;
    }
    let mut m = Mask::new(width, height);
    if width == 0 || height == 0 {
        return Some(m.px);
    }
    let thin = thin.max(1);
    match c as u32 {
        cp @ 0x2500..=0x257F => box_drawing(&mut m, cp, thin),
        cp @ 0x2580..=0x259F => block(&mut m, cp),
        cp => powerline(&mut m, cp, thin),
    }
    Some(m.px)
}

/// A coverage buffer being drawn into.
struct Mask {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Mask {
    fn new(w: u32, h: u32) -> Mask {
        Mask {
            w,
            h,
            px: vec![0; (w as usize) * (h as usize)],
        }
    }

    /// Fill the half-open rectangle `[x0, x1) × [y0, y1)` (clamped) with `v`.
    fn fill_value(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, v: u8) {
        let (x1, y1) = (x1.min(self.w), y1.min(self.h));
        for y in y0.min(y1)..y1 {
            let row = (y * self.w) as usize;
            self.px[row + x0.min(x1) as usize..row + x1 as usize].fill(v);
        }
    }

    fn fill(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.fill_value(x0, y0, x1, y1, 255);
    }

    /// Antialiased fill: each pixel gets the fraction of a 4×4 grid of
    /// sample points for which `inside(x, y)` holds (in pixel coordinates,
    /// origin at the top-left corner). Existing coverage is kept.
    fn sample(&mut self, inside: impl Fn(f32, f32) -> bool) {
        const N: u32 = 4;
        for y in 0..self.h {
            for x in 0..self.w {
                let mut hits = 0;
                for sy in 0..N {
                    for sx in 0..N {
                        let px = x as f32 + (sx as f32 + 0.5) / N as f32;
                        let py = y as f32 + (sy as f32 + 0.5) / N as f32;
                        hits += inside(px, py) as u32;
                    }
                }
                let v = ((hits * 255 + N * N / 2) / (N * N)) as u8;
                let p = &mut self.px[(y * self.w + x) as usize];
                *p = (*p).max(v);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Box drawing (U+2500–U+257F)

/// The weight of one arm of a box-drawing character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

impl Weight {
    fn from_digit(d: u8) -> Weight {
        match d {
            b'1' => Weight::Light,
            b'2' => Weight::Heavy,
            b'3' => Weight::Double,
            _ => Weight::None,
        }
    }

    fn present(self) -> bool {
        self != Weight::None
    }

    /// `self` if present, else `other` (dashed lines have one axis only).
    fn or(self, other: Weight) -> Weight {
        if self.present() {
            self
        } else {
            other
        }
    }
}

/// The arms of every character in U+2500–U+257F as `LURD` digits (left, up,
/// right, down; 0 none, 1 light, 2 heavy, 3 double), five bytes per entry.
/// Dashes, arcs and diagonals are drawn specially; their entries give the
/// orientation and weight (diagonals have none).
const ARMS: &[u8] = b"\
1010 2020 0101 0202 1010 2020 0101 0202 1010 2020 0101 0202 0011 0021 0012 0022 \
1001 2001 1002 2002 0110 0120 0210 0220 1100 2100 1200 2200 0111 0121 0211 0112 \
0212 0221 0122 0222 1101 2101 1201 1102 1202 2201 2102 2202 1011 2011 1021 2021 \
1012 2012 1022 2022 1110 2110 1120 2120 1210 2210 1220 2220 1111 2111 1121 2121 \
1211 1112 1212 2211 1221 2112 1122 2221 2122 2212 1222 2222 1010 2020 0101 0202 \
3030 0303 0031 0013 0033 3001 1003 3003 0130 0310 0330 3100 1300 3300 0131 0313 \
0333 3101 1303 3303 3031 1013 3033 3130 1310 3330 3131 1313 3333 0011 1001 1100 \
0110 0000 0000 0000 1000 0100 0010 0001 2000 0200 0020 0002 1020 0102 2010 0201 ";

/// Left, up, right, down arms of a box-drawing code point.
fn arms(cp: u32) -> [Weight; 4] {
    let i = (cp - 0x2500) as usize * 5;
    let e = &ARMS[i..i + 4];
    [e[0], e[1], e[2], e[3]].map(Weight::from_digit)
}

fn thickness(w: Weight, thin: u32) -> u32 {
    match w {
        Weight::None => 0,
        Weight::Light => thin,
        Weight::Heavy => thin * 2,
        Weight::Double => thin * 3,
    }
}

/// A line of thickness `t` centered across `dim` pixels: `[start, end)`.
/// Every cell uses the same rule, so lines meet their neighbors exactly.
fn band(dim: u32, t: u32) -> (u32, u32) {
    let t = t.clamp(1, dim.max(1));
    let s = dim.saturating_sub(t) / 2;
    (s, s + t)
}

/// The two strands of a double line across `dim` pixels, near side first.
fn double_bands(dim: u32, thin: u32) -> [(u32, u32); 2] {
    let (s, e) = band(dim, thin * 3);
    let t = ((e - s) / 3).max(1);
    [(s, s + t), (e - t, e)]
}

fn box_drawing(m: &mut Mask, cp: u32, thin: u32) {
    let [l, u, r, d] = arms(cp);
    match cp {
        0x2504..=0x250B | 0x254C..=0x254F => {
            let n = match cp {
                0x2504..=0x2507 => 3,
                0x2508..=0x250B => 4,
                _ => 2,
            };
            dashes(m, l.present(), l.or(u), n, thin);
        }
        0x256D..=0x2570 => arc(m, r.present(), d.present(), thin),
        0x2571..=0x2573 => diagonals(m, cp != 0x2572, cp != 0x2571, thin),
        _ => {
            let (w, h) = (m.w, m.h);
            draw_axis(w, h, [l, r], [u, d], thin, |a0, a1, c0, c1| {
                m.fill(a0, c0, a1, c1)
            });
            draw_axis(h, w, [u, d], [l, r], thin, |a0, a1, c0, c1| {
                m.fill(c0, a0, c1, a1)
            });
        }
    }
}

/// Draw the arms along one axis. `along` is the cell size along the axis and
/// `cross` the size across it; `arms` are the near and far arms (left/right
/// or up/down) and `perp` the arms of the other axis, near side first. `put`
/// receives `[a0, a1) × [c0, c1)` in along/cross coordinates.
///
/// Where an arm stops depends on what it joins: light and heavy arms run to
/// the far side of the perpendicular line so corners close; double arms stop
/// at the strand that keeps the other axis's gap open.
fn draw_axis(
    along: u32,
    cross: u32,
    arms: [Weight; 2],
    perp: [Weight; 2],
    thin: u32,
    mut put: impl FnMut(u32, u32, u32, u32),
) {
    let perp_any = perp.iter().any(|w| w.present());
    let perp_double = perp.contains(&Weight::Double);
    let perp_both = perp.iter().all(|w| w.present());
    let x = double_bands(along, thin);
    // Extent of the non-double perpendicular lines.
    let (pmin, pmax) = perp
        .iter()
        .filter(|w| w.present() && **w != Weight::Double)
        .map(|w| band(along, thickness(*w, thin)))
        .fold((along, 0), |(s, e), (bs, be)| (s.min(bs), e.max(be)));

    for (side, &w) in arms.iter().enumerate() {
        if !w.present() {
            continue;
        }
        let far = side == 1;
        let opposite = arms[1 - side].present();
        // Returns (start, end) along the axis for one strand, given the
        // perpendicular arm on that strand's side (for double arms).
        let extent = |strand_side: Option<Weight>| -> (u32, u32) {
            let (near_x, far_x) = (x[0], x[1]);
            // `inner` is the strand of the perpendicular double line nearest
            // this arm's far end, `outer` the other one.
            let (inner, outer) = if far {
                (far_x, near_x)
            } else {
                (near_x, far_x)
            };
            let stop = |b: (u32, u32)| if far { b.0 } else { b.1 };
            let edge = if far { along } else { 0 };
            let reach = match strand_side {
                // Single (light/heavy) arm.
                None => {
                    if !perp_any {
                        stop(band(along, thickness(w, thin)))
                    } else if perp_double {
                        if perp_both && !opposite {
                            stop(inner)
                        } else {
                            stop(outer)
                        }
                    } else if far {
                        pmin
                    } else {
                        pmax
                    }
                }
                // One strand of a double arm.
                Some(side_arm) => {
                    if !perp_any {
                        stop(outer)
                    } else if perp_double {
                        if side_arm == Weight::Double {
                            stop(inner)
                        } else {
                            stop(outer)
                        }
                    } else if far {
                        pmin
                    } else {
                        pmax
                    }
                }
            };
            if far {
                (reach, edge)
            } else {
                (edge, reach)
            }
        };
        if w == Weight::Double {
            for (k, (c0, c1)) in double_bands(cross, thin).into_iter().enumerate() {
                let (a0, a1) = extent(Some(perp[k]));
                put(a0, a1, c0, c1);
            }
        } else {
            let (c0, c1) = band(cross, thickness(w, thin));
            let (a0, a1) = extent(None);
            put(a0, a1, c0, c1);
        }
    }
}

/// A dashed line of `n` dashes, horizontal or vertical. Each dash is centered
/// in its `1/n` of the cell, so the gaps line up across cells.
fn dashes(m: &mut Mask, horizontal: bool, w: Weight, n: u32, thin: u32) {
    let (along, cross) = if horizontal { (m.w, m.h) } else { (m.h, m.w) };
    let (c0, c1) = band(cross, thickness(w, thin));
    let seg = along as f32 / n as f32;
    let gap = (seg * 0.4).round().max(1.0);
    for i in 0..n {
        let a0 = (i as f32 * seg + gap / 2.0).round() as u32;
        let a1 = (((i + 1) as f32 * seg - gap / 2.0).round() as u32).max(a0 + 1);
        if horizontal {
            m.fill(a0, c0, a1, c1);
        } else {
            m.fill(c0, a0, c1, a1);
        }
    }
}

/// Distance from `(px, py)` to the segment `(ax, ay)–(bx, by)`.
fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (qx, qy) = (ax + t * dx - px, ay + t * dy - py);
    (qx * qx + qy * qy).sqrt()
}

/// A rounded corner (╭╮╯╰): a quarter circle joining the centers of the
/// light horizontal and vertical lines, with straight runs out to the edges.
fn arc(m: &mut Mask, right: bool, down: bool, thin: u32) {
    let (w, h) = (m.w as f32, m.h as f32);
    let (vx0, vx1) = band(m.w, thin);
    let (hy0, hy1) = band(m.h, thin);
    let cx = (vx0 + vx1) as f32 / 2.0;
    let cy = (hy0 + hy1) as f32 / 2.0;
    let half = (vx1 - vx0).min(hy1 - hy0) as f32 / 2.0;
    let (sx, ex) = if right { (1.0, w) } else { (-1.0, 0.0) };
    let (sy, ey) = if down { (1.0, h) } else { (-1.0, 0.0) };
    let r = (ex - cx).abs().min((ey - cy).abs());
    let (ccx, ccy) = (cx + sx * r, cy + sy * r);
    m.sample(|px, py| {
        let on_arc = (px - ccx) * sx <= 0.0 && (py - ccy) * sy <= 0.0 && {
            let d = ((px - ccx).powi(2) + (py - ccy).powi(2)).sqrt();
            (d - r).abs() <= half
        };
        on_arc
            || seg_dist(px, py, ccx, cy, ex, cy) <= half
            || seg_dist(px, py, cx, ccy, cx, ey) <= half
    });
}

/// Antialiased diagonals corner to corner: `rising` is ╱, `falling` is ╲.
fn diagonals(m: &mut Mask, rising: bool, falling: bool, thin: u32) {
    let (w, h) = (m.w as f32, m.h as f32);
    let len = (w * w + h * h).sqrt();
    let half = thin as f32 / 2.0;
    m.sample(|px, py| {
        (falling && (h * px - w * py).abs() / len <= half)
            || (rising && (h * px + w * py - w * h).abs() / len <= half)
    });
}

// ---------------------------------------------------------------------------
// Block elements (U+2580–U+259F)

fn block(m: &mut Mask, cp: u32) {
    let (w, h) = (m.w, m.h);
    // `k` eighths of `dim`, rounded.
    let eighth = |dim: u32, k: u32| ((dim * k) as f32 / 8.0).round() as u32;
    let (mx, my) = (w / 2, h / 2);
    // Quadrants: upper-left, upper-right, lower-left, lower-right.
    let quads = |m: &mut Mask, q: [bool; 4]| {
        let rects = [
            (0, 0, mx, my),
            (mx, 0, w, my),
            (0, my, mx, h),
            (mx, my, w, h),
        ];
        for (on, (x0, y0, x1, y1)) in q.into_iter().zip(rects) {
            if on {
                m.fill(x0, y0, x1, y1);
            }
        }
    };
    match cp {
        0x2580 => m.fill(0, 0, w, my),
        0x2581..=0x2587 => m.fill(0, h - eighth(h, cp - 0x2580), w, h),
        0x2588 => m.fill(0, 0, w, h),
        0x2589..=0x258F => m.fill(0, 0, eighth(w, 8 - (cp - 0x2588)), h),
        0x2590 => m.fill(mx, 0, w, h),
        0x2591 => m.fill_value(0, 0, w, h, 64),
        0x2592 => m.fill_value(0, 0, w, h, 128),
        0x2593 => m.fill_value(0, 0, w, h, 191),
        0x2594 => m.fill(0, 0, w, eighth(h, 1)),
        0x2595 => m.fill(w - eighth(w, 1), 0, w, h),
        0x2596 => quads(m, [false, false, true, false]),
        0x2597 => quads(m, [false, false, false, true]),
        0x2598 => quads(m, [true, false, false, false]),
        0x2599 => quads(m, [true, false, true, true]),
        0x259A => quads(m, [true, false, false, true]),
        0x259B => quads(m, [true, true, true, false]),
        0x259C => quads(m, [true, true, false, true]),
        0x259D => quads(m, [false, true, false, false]),
        0x259E => quads(m, [false, true, true, false]),
        _ => quads(m, [false, true, true, true]),
    }
}

// ---------------------------------------------------------------------------
// Powerline (U+E0B0–U+E0B3)

/// Solid and outline triangles pointing right (E0B0/E0B1) or left
/// (E0B2/E0B3), spanning the full cell height.
fn powerline(m: &mut Mask, cp: u32, thin: u32) {
    let (w, h) = (m.w as f32, m.h as f32);
    let mid = h / 2.0;
    let half = thin as f32 / 2.0;
    let pointing_right = cp <= 0xE0B1;
    let solid = cp == 0xE0B0 || cp == 0xE0B2;
    // Base edge at `bx`, tip at `tx`.
    let (bx, tx) = if pointing_right { (0.0, w) } else { (w, 0.0) };
    m.sample(|px, py| {
        if solid {
            // How far from the base the triangle reaches at this row.
            let reach = w * (1.0 - (py - mid).abs() / mid);
            (px - bx).abs() <= reach
        } else {
            seg_dist(px, py, bx, 0.0, tx, mid) <= half || seg_dist(px, py, tx, mid, bx, h) <= half
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_handled() -> impl Iterator<Item = char> {
        (0x2500..=0x259F)
            .chain(0xE0B0..=0xE0B3)
            .filter_map(char::from_u32)
    }

    fn rows_with_ink(mask: &[u8], w: u32) -> Vec<usize> {
        mask.chunks(w as usize)
            .enumerate()
            .filter(|(_, r)| r.iter().any(|&v| v > 0))
            .map(|(i, _)| i)
            .collect()
    }

    fn mean(mask: &[u8]) -> f32 {
        mask.iter().map(|&v| v as f32).sum::<f32>() / mask.len() as f32 / 255.0
    }

    #[test]
    fn handles_exactly_its_ranges() {
        assert!(handles('─') && handles('╿') && handles('▀') && handles('▟'));
        assert!(handles('\u{E0B0}') && handles('\u{E0B3}'));
        assert!(!handles('a') && !handles('\u{24FF}') && !handles('\u{25A0}'));
        assert!(!handles('\u{E0B4}'));
        assert_eq!(draw('a', 9, 18, 1), None);
    }

    #[test]
    fn arms_table_is_complete() {
        assert_eq!(ARMS.len(), 128 * 5);
        assert_eq!(
            arms(0x2500),
            [Weight::Light, Weight::None, Weight::Light, Weight::None]
        );
        assert_eq!(arms(0x254B), [Weight::Heavy; 4]);
        assert_eq!(arms(0x256C), [Weight::Double; 4]);
    }

    #[test]
    fn every_glyph_has_the_right_size_and_some_ink() {
        for (w, h, t) in [(9, 18, 1), (10, 20, 2), (7, 15, 1), (1, 1, 1), (16, 33, 3)] {
            for c in all_handled() {
                let m = draw(c, w, h, t).unwrap();
                assert_eq!(m.len(), (w * h) as usize, "{c:?} at {w}x{h}");
                if w > 2 {
                    assert!(m.iter().any(|&v| v > 0), "{c:?} at {w}x{h} is blank");
                }
            }
        }
        assert_eq!(draw('─', 0, 18, 1), Some(Vec::new()));
    }

    #[test]
    fn horizontal_line_is_centered_and_full_width() {
        let m = draw('─', 9, 18, 1).unwrap();
        assert_eq!(rows_with_ink(&m, 9), vec![8]);
        assert!(m[8 * 9..9 * 9].iter().all(|&v| v == 255));
        // Heavy shares the center.
        let m = draw('━', 9, 18, 1).unwrap();
        assert_eq!(rows_with_ink(&m, 9), vec![8, 9]);
    }

    #[test]
    fn vertical_line_spans_full_height() {
        let m = draw('│', 9, 18, 1).unwrap();
        for row in m.chunks(9) {
            assert_eq!(row, &[0, 0, 0, 0, 255, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn corners_and_tees_join_the_edges() {
        let (w, h) = (9u32, 18u32);
        let at = |m: &[u8], x: u32, y: u32| m[(y * w + x) as usize];
        let m = draw('┌', w, h, 1).unwrap();
        assert_eq!(at(&m, 8, 8), 255); // right edge
        assert_eq!(at(&m, 4, 17), 255); // bottom edge
        assert_eq!(at(&m, 0, 8), 0);
        assert_eq!(at(&m, 4, 0), 0);
        let m = draw('┼', w, h, 1).unwrap();
        assert!([(0, 8), (8, 8), (4, 0), (4, 17)]
            .iter()
            .all(|&(x, y)| at(&m, x, y) == 255));
    }

    #[test]
    fn double_lines_have_a_gap() {
        let m = draw('═', 9, 18, 1).unwrap();
        assert_eq!(rows_with_ink(&m, 9), vec![7, 9]);
        // ╔: the outer strands meet, the inner corner stays open.
        let m = draw('╔', 9, 18, 1).unwrap();
        let at = |x: u32, y: u32| m[(y * 9 + x) as usize];
        assert_eq!(at(3, 7), 255); // outer corner
        assert_eq!(at(5, 9), 255); // inner corner
        assert_eq!(at(4, 8), 0); // gap between them
        assert_eq!(at(3, 17), 255);
        assert_eq!(at(8, 7), 255);
        // ╬: nothing crosses the center.
        let m = draw('╬', 9, 18, 1).unwrap();
        assert_eq!(m[(8 * 9 + 4) as usize], 0);
    }

    #[test]
    fn dashes_count() {
        for (c, n) in [('┄', 3), ('┈', 4), ('╌', 2)] {
            let m = draw(c, 24, 18, 1).unwrap();
            let row = &m[8 * 24..9 * 24];
            let runs =
                row.windows(2).filter(|p| p[0] == 0 && p[1] > 0).count() + (row[0] > 0) as usize;
            assert_eq!(runs, n, "{c}");
        }
    }

    #[test]
    fn rounded_corner_meets_straight_lines() {
        let m = draw('╭', 9, 18, 1).unwrap();
        // Bottom edge: same column as │; right edge: same row as ─.
        assert_eq!(&m[17 * 9..18 * 9], &[0, 0, 0, 0, 255, 0, 0, 0, 0]);
        assert_eq!(m[8 * 9 + 8], 255);
        assert_eq!(m[0], 0);
    }

    #[test]
    fn diagonals_reach_the_corners() {
        let m = draw('╲', 10, 20, 1).unwrap();
        assert!(m[0] > 100 && m[199] > 100);
        assert_eq!(m[9], 0);
        let m = draw('╳', 10, 20, 1).unwrap();
        assert!(m[0] > 100 && m[9] > 100 && m[190] > 100 && m[199] > 100);
    }

    #[test]
    fn blocks() {
        assert!(draw('█', 9, 18, 1).unwrap().iter().all(|&v| v == 255));
        let m = draw('▀', 9, 18, 1).unwrap();
        assert_eq!(rows_with_ink(&m, 9), (0..9).collect::<Vec<_>>());
        assert!(m[..81].iter().all(|&v| v == 255));
        let top = draw('▀', 9, 17, 1).unwrap();
        let bottom = draw('▄', 9, 17, 1).unwrap();
        assert!(top.iter().zip(&bottom).all(|(a, b)| a ^ b == 255));
        assert!((mean(&draw('░', 9, 18, 1).unwrap()) - 0.25).abs() < 0.02);
        assert!((mean(&draw('▒', 9, 18, 1).unwrap()) - 0.5).abs() < 0.02);
        assert!((mean(&draw('▓', 9, 18, 1).unwrap()) - 0.75).abs() < 0.02);
        assert!((mean(&draw('▂', 8, 16, 1).unwrap()) - 0.25).abs() < 0.01);
        assert!((mean(&draw('▊', 8, 16, 1).unwrap()) - 0.75).abs() < 0.01);
        let m = draw('▚', 8, 16, 1).unwrap();
        assert_eq!((m[0], m[7], m[15 * 8], m[15 * 8 + 7]), (255, 0, 0, 255));
    }

    #[test]
    fn powerline_triangles() {
        let m = draw('\u{E0B0}', 10, 20, 1).unwrap();
        // Full on the left edge, tip at the right middle, empty corners.
        assert!(m.chunks(10).all(|r| r[0] > 0));
        assert!(m[9 * 10 + 9] > 0 && m[9] == 0 && m[199] == 0);
        assert!((mean(&m) - 0.5).abs() < 0.05);
        let m = draw('\u{E0B2}', 10, 20, 1).unwrap();
        assert!(m.chunks(10).all(|r| r[9] > 0));
        assert_eq!(m[0], 0);
        let m = draw('\u{E0B1}', 10, 20, 1).unwrap();
        assert!(mean(&m) < 0.25 && m[9 * 10 + 9] > 0);
    }
}
