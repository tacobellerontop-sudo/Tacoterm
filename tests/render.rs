//! Renders a grid offscreen and checks the pixels. Needs a GPU adapter (a
//! software one such as lavapipe or WARP is fine); skips when none exists.
//! Set TACOTERM_SNAPSHOT=path.png to save the frame for a look.

use tacoterm::config::Config;
use tacoterm::grid::Grid;
use tacoterm::renderer::{Fonts, Renderer};
use tacoterm::vt::Vt;

#[test]
fn renders_text_colors_cursor_and_selection() {
    let config = Config::default();
    let fonts = Fonts::new(&config, 1.0);
    let (cw, ch) = (fonts.cell_width, fonts.cell_height);
    let (cols, rows) = (40u32, 8u32);
    let pad = config.padding;
    let (w, h) = (cols * cw + 2 * pad, rows * ch + 2 * pad);
    let mut renderer = match Renderer::headless(w, h, fonts, &config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    assert_eq!(renderer.grid_size(), (cols as u16, rows as u16));

    let mut grid = Grid::new(cols as usize, rows as usize, 100);
    let mut vt = Vt::new();
    vt.advance(
        &mut grid,
        "C:\\> tacoterm\r\n\x1b[1;32mbold green\x1b[0m \x1b[41;97m red bg \x1b[0m \x1b[4munderline\x1b[0m\r\n\
         \x1b[38;2;255;128;0mtruecolor\x1b[0m 漢字 ✓ é\r\nselect me\r\n> "
            .as_bytes(),
    );
    grid.start_selection(grid.viewport_point(0, 3));
    grid.extend_selection(grid.viewport_point(8, 3));
    assert!(renderer.render(&grid));
    let pixels = renderer.read_pixels().unwrap();
    assert_eq!(pixels.len(), (w * h * 4) as usize);

    if let Ok(path) = std::env::var("TACOTERM_SNAPSHOT") {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
    }

    let px = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    // Cell centre in pixels.
    let at = |col: u32, row: u32| (pad + col * cw + cw / 2, pad + row * ch + ch / 2);
    let in_cell = |col: u32, row: u32| {
        (0..cw)
            .flat_map(move |dx| (0..ch).map(move |dy| (pad + col * cw + dx, pad + row * ch + dy)))
    };

    // Background is the theme background.
    assert_eq!(px(0, 0), (0x0c, 0x0c, 0x0c));
    // The prompt text has foreground-colored glyph pixels.
    assert!(
        in_cell(0, 0).any(|(x, y)| px(x, y).0 > 0x80),
        "no glyph pixels for 'C'"
    );
    // "bold green" uses bright green (palette 10) because it's bold.
    assert!(in_cell(0, 1).any(|(x, y)| px(x, y) == (0x16, 0xc6, 0x0c)));
    // " red bg " cell 11 has the red (palette 1) background.
    let (x, y) = at(11, 1);
    assert_eq!(px(x, pad + ch + 1), (0xc5, 0x0f, 0x1f), "red bg at {x},{y}");
    // Selection highlight on "select me".
    let (x, _) = at(0, 3);
    assert_eq!(px(x, pad + 3 * ch + 1), (0x26, 0x4f, 0x78));
    // Block cursor after "> " on row 4.
    let (x, y) = at(2, 4);
    assert_eq!(px(x, y), (0xcc, 0xcc, 0xcc));
}
