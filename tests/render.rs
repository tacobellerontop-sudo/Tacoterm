//! Renders a grid offscreen and checks the pixels. Needs a GPU adapter (a
//! software one such as lavapipe or WARP is fine); skips when none exists.
//! Set TACOTERM_SNAPSHOT=path.png (or TACOTERM_SNAPSHOT_TABS for the tabs
//! and splits frame) to save the frame for a look.

use tacoterm::config::Config;
use tacoterm::grid::Grid;
use tacoterm::layout::{Layout, Rect, Split};
use tacoterm::renderer::{
    Control, Fonts, Frame, PaneView, Renderer, SearchBar, SettingsPage, SettingsRow, TabHit,
};
use tacoterm::settings::{preview_text, Kind, Settings};
use tacoterm::vt::Vt;

#[test]
fn renders_text_colors_cursor_and_selection() {
    let config = Config {
        color_scheme: "campbell".into(),
        padding: 4,
        ..Config::default()
    };
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
    let full = Rect::new(0.0, 0.0, w as f32, h as f32);
    assert!(renderer.render(&Frame::single(&grid, full)));
    let pixels = renderer.read_pixels().unwrap();
    assert_eq!(pixels.len(), (w * h * 4) as usize);

    save_snapshot("TACOTERM_SNAPSHOT", &pixels, w, h);

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

#[test]
fn renders_tabs_splits_search_and_box_drawing() {
    let config = Config {
        color_scheme: "campbell".into(),
        padding: 2,
        ..Config::default()
    };
    let fonts = Fonts::new(&config, 1.0);
    let (cw, ch) = (fonts.cell_width, fonts.cell_height);
    let bar = fonts.tab_bar_height();
    let (w, h) = (60 * cw, 10 * ch + bar);
    let mut renderer = match Renderer::headless(w, h, fonts, &config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let area = Rect::new(0.0, bar as f32, w as f32, (h - bar) as f32);
    let mut layout = Layout::new(1);
    assert!(layout.split(1, Split::Right, 2));
    let gap = renderer.divider_width() as f32;
    let rects = layout.rects(area, gap);
    let mut grids: Vec<Grid> = rects
        .iter()
        .map(|(_, r)| {
            let (c, rows) = renderer.pane_grid_size(*r);
            Grid::new(c as usize, rows as usize, 100)
        })
        .collect();
    let mut vt = Vt::new();
    vt.advance(&mut grids[0], "──────\r\nfind me here\r\n".as_bytes());
    vt.advance(&mut grids[1], "right pane".as_bytes());
    let matches = grids[0].search("me", false);
    assert_eq!(matches.len(), 1);
    let frame = Frame {
        panes: rects
            .iter()
            .zip(&grids)
            .enumerate()
            .map(|(i, ((_, r), g))| PaneView {
                grid: g,
                rect: *r,
                focused: i == 0,
                matches: if i == 0 { &matches } else { &[] },
                current_match: Some(0),
                link: None,
            })
            .collect(),
        dividers: layout.dividers(area, gap),
        tabs: vec!["PowerShell".into(), "Command Prompt".into()],
        active_tab: 0,
        search: Some(SearchBar {
            query: "me",
            current: Some(0),
            total: 1,
        }),
        cursor_on: true,
        flash: false,
        settings: None,
    };
    assert!(renderer.render(&frame));
    let pixels = renderer.read_pixels().unwrap();
    save_snapshot("TACOTERM_SNAPSHOT_TABS", &pixels, w, h);
    let px = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    let bg = (0x0c, 0x0c, 0x0c);
    // The box-drawing line is unbroken across the cell boundary.
    let pad = 2;
    let y0 = bar + pad;
    let lit_row = (y0..y0 + ch)
        .find(|&y| px(pad + cw / 2, y) != bg)
        .expect("line drawn");
    for x in pad..pad + 6 * cw {
        assert_ne!(px(x, lit_row), bg, "gap in ─ line at x={x}");
    }
    // The strip right of the tabs is darker than the active tab...
    let strip = px(w - 2, bar / 2);
    assert!(
        strip.0 <= bg.0 && strip != px(w / 4, bar / 2),
        "strip {strip:?}"
    );
    // ...and the divider between the panes is drawn.
    let d = layout.dividers(area, gap)[0];
    assert_ne!(px(d.x as u32, (d.y + d.h / 2.0) as u32), bg);
}

#[test]
fn renders_settings_page_and_window_buttons() {
    let config = Config {
        padding: 4,
        ..Config::default()
    };
    let fonts = Fonts::new(&config, 1.0);
    let (w, h) = (1100u32, 640u32);
    let mut renderer = match Renderer::headless(w, h, fonts, &config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    renderer.window_buttons = true;
    renderer.hover = Some(TabHit::CloseWindow);
    let bar = renderer.tab_bar_height() as f32;
    let area = Rect::new(0.0, bar, w as f32, h as f32 - bar);

    let mut model = Settings::new(&config, vec!["Cascadia Mono".into()], Vec::new());
    let name = model
        .fields
        .iter()
        .position(|f| f.key == "prompt_name")
        .unwrap();
    model.type_text(name, "LX");
    model.selected = name;
    let rows: Vec<SettingsRow> = model
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| SettingsRow {
            label: f.label.to_string(),
            value: model.value_text(i),
            control: match f.kind {
                Kind::Choice(_) => Control::Choice,
                Kind::Number { .. } => Control::Slider(model.fraction(i).unwrap_or(0.0)),
                Kind::Toggle => Control::Toggle(model.toggle_state(i)),
                Kind::Text { .. } => Control::Text {
                    presets: model.has_presets(i),
                },
            },
        })
        .collect();
    let prect = renderer.settings_preview_rect(area);
    let (pc, pr) = renderer.pane_grid_size(prect);
    let mut preview = Grid::new(pc as usize, pr as usize, 0);
    Vt::new().advance(&mut preview, preview_text(&model.draft).as_bytes());

    // Clicks find the right controls.
    let n = rows.len();
    let page = |rows: Vec<SettingsRow>| SettingsPage {
        area,
        rows,
        selected: model.selected,
        scroll: 0,
        help: model.fields[model.selected].help,
        preview: &preview,
        dirty: true,
        hover: None,
    };
    let frame = Frame {
        panes: Vec::new(),
        dividers: Vec::new(),
        tabs: vec!["PowerShell".into()],
        active_tab: 0,
        search: None,
        cursor_on: true,
        flash: false,
        settings: Some(page(rows.clone())),
    };
    assert!(renderer.render(&frame));
    let pixels = renderer.read_pixels().unwrap();
    save_snapshot("TACOTERM_SNAPSHOT_SETTINGS", &pixels, w, h);

    assert_eq!(
        renderer.tab_hit(w as f64 - 5.0, 5.0, 1),
        Some(TabHit::CloseWindow)
    );
    let mut found_row = false;
    for y in (bar as u32..h).step_by(4) {
        if let Some(tacoterm::renderer::SettingsHit::Row(_)) =
            renderer.settings_hit(area, &rows, 0, 40.0, y as f64)
        {
            found_row = true;
        }
    }
    assert!(found_row && n > 5);
    // The hovered close button is red.
    let i = ((5 * w + (w - 5)) * 4) as usize;
    assert!(
        pixels[i] > 150 && pixels[i + 1] < 90,
        "{:?}",
        &pixels[i..i + 3]
    );
}

fn save_snapshot(var: &str, pixels: &[u8], w: u32, h: u32) {
    let Ok(path) = std::env::var(var) else { return };
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}
