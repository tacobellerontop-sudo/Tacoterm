//! GPU text renderer: a wgpu surface plus a glyph atlas fed by cosmic-text.
//!
//! Each frame walks the visible grid of every pane and emits one instanced
//! quad per background run, glyph and decoration, plus the tab strip and
//! find box, then draws them in a single call. Box-drawing and block
//! characters come from [`crate::boxdraw`] so they tile without gaps.
//! Glyphs are shaped and rasterized once by cosmic-text (which also does
//! font fallback for CJK, emoji, symbols…) and cached in an RGBA atlas
//! texture. Frames are only drawn when something changed; the app is
//! event-driven, so an idle terminal costs nothing.
//!
//! TODO: per-row damage tracking (rebuild only changed rows' instances).

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};
use winit::window::Window;

use crate::boxdraw;
use crate::config::{Config, CursorStyle, Rgb, Theme};
use crate::graphics::Image;
use crate::grid::{Attr, Color, Grid, Match, Point};
pub use crate::layout::Rect;

const ATLAS_SIZE: u32 = 1024;

/// Fallback monospace families, tried in order when the configured font
/// isn't installed.
const FALLBACK_FAMILIES: &[&str] = &[
    "Cascadia Mono",
    "Consolas",
    "Lucida Console",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Courier New",
];

// ---- fonts -------------------------------------------------------------

/// A rasterized glyph, ready to upload.
struct RasterGlyph {
    /// Offset of the bitmap's top-left from the cell's top-left, in pixels.
    offset: (i32, i32),
    width: u32,
    height: u32,
    /// RGBA8. Mask glyphs are white with coverage in alpha.
    rgba: Vec<u8>,
    is_color: bool,
}

/// Font loading, cell metrics and glyph rasterizing.
pub struct Fonts {
    system: FontSystem,
    swash: SwashCache,
    /// Resolved family name, or None for the generic monospace family.
    family: Option<String>,
    size: f32,
    pub cell_width: u32,
    pub cell_height: u32,
    baseline: f32,
}

impl Fonts {
    /// Load system fonts and measure the cell. `scale` is the window's DPI
    /// scale factor; `config.font_size` is in logical pixels.
    pub fn new(config: &Config, scale: f64) -> Fonts {
        let mut system = FontSystem::new();
        let has = |sys: &FontSystem, name: &str| {
            sys.db()
                .faces()
                .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
        };
        let family = std::iter::once(config.font_family.as_str())
            .chain(FALLBACK_FAMILIES.iter().copied())
            .find(|name| has(&system, name))
            .map(str::to_string);
        if family.is_none() {
            system.db_mut().set_monospace_family("Courier New");
        }

        let size = (config.font_size as f64 * scale).max(4.0) as f32;
        let line_height = (size * config.line_height.max(0.5)).round().max(1.0);
        let mut fonts = Fonts {
            system,
            swash: SwashCache::new(),
            family,
            size,
            cell_width: 1,
            cell_height: line_height as u32,
            baseline: 0.0,
        };

        // Measure with a representative glyph: advance gives the cell width,
        // the shaped line gives the baseline.
        let buf = fonts.shape("M", false, false);
        if let Some(run) = buf.layout_runs().next() {
            fonts.baseline = run.line_y;
            if let Some(g) = run.glyphs.first() {
                fonts.cell_width = g.w.round().max(1.0) as u32;
            }
        }
        fonts
    }

    /// Height of the tab strip in pixels.
    pub fn tab_bar_height(&self) -> u32 {
        (self.cell_height as f32 * 1.8).round() as u32
    }

    /// Installed monospace font families, sorted.
    pub fn monospace_families(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .system
            .db()
            .faces()
            .filter(|f| f.monospaced)
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .collect();
        out.sort_by_key(|n| n.to_lowercase());
        out.dedup();
        out
    }

    fn shape(&mut self, text: &str, bold: bool, italic: bool) -> Buffer {
        let metrics = Metrics::new(self.size, self.cell_height as f32);
        let mut buf = Buffer::new(&mut self.system, metrics);
        buf.set_size(&mut self.system, None, None);
        let family = self.family.clone();
        let mut attrs = Attrs::new().family(match &family {
            Some(name) => Family::Name(name),
            None => Family::Monospace,
        });
        if bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        if italic {
            attrs = attrs.style(Style::Italic);
        }
        buf.set_text(&mut self.system, text, attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.system, false);
        buf
    }

    /// Draw `text` (one character, or a cluster such as a letter with
    /// accents or an emoji sequence) as one bitmap.
    fn rasterize(&mut self, text: &str, bold: bool, italic: bool) -> Option<RasterGlyph> {
        let buf = self.shape(text, bold, italic);
        let run = buf.layout_runs().next()?;
        let line_y = run.line_y.round() as i32;
        // Each glyph's bitmap and where its top-left lands.
        let mut parts = Vec::new();
        for glyph in run.glyphs.iter() {
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let Some(image) = self
                .swash
                .get_image_uncached(&mut self.system, physical.cache_key)
            else {
                continue;
            };
            let (w, h) = (image.placement.width, image.placement.height);
            if w == 0 || h == 0 {
                continue;
            }
            let at = (
                physical.x + image.placement.left,
                line_y + physical.y - image.placement.top,
            );
            parts.push((at, w, h, image));
        }
        let x0 = parts.iter().map(|p| p.0 .0).min()?;
        let y0 = parts.iter().map(|p| p.0 .1).min()?;
        let x1 = parts.iter().map(|p| p.0 .0 + p.1 as i32).max()?;
        let y1 = parts.iter().map(|p| p.0 .1 + p.2 as i32).max()?;
        let (width, height) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let is_color = parts
            .iter()
            .any(|p| matches!(p.3.content, SwashContent::Color));
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for ((px, py), w, h, image) in &parts {
            let pixels = (*w * *h) as usize;
            for k in 0..pixels {
                let (sx, sy) = ((k as u32 % w) as i32, (k as u32 / w) as i32);
                let src = match image.content {
                    SwashContent::Mask => [255, 255, 255, *image.data.get(k)?],
                    SwashContent::SubpixelMask => {
                        let p = image.data.get(k * 4..k * 4 + 3)?;
                        let a = ((p[0] as u16 + p[1] as u16 + p[2] as u16) / 3) as u8;
                        [255, 255, 255, a]
                    }
                    SwashContent::Color => {
                        let p = image.data.get(k * 4..k * 4 + 4)?;
                        [p[0], p[1], p[2], p[3]]
                    }
                };
                if src[3] == 0 {
                    continue;
                }
                let (dx, dy) = ((px + sx - x0) as u32, (py + sy - y0) as u32);
                let d = ((dy * width + dx) * 4) as usize;
                // "Over": later glyphs (accents) on top of earlier ones.
                let a = src[3] as u32;
                for c in 0..3 {
                    rgba[d + c] =
                        ((src[c] as u32 * a + rgba[d + c] as u32 * (255 - a)) / 255) as u8;
                }
                rgba[d + 3] = (a + rgba[d + 3] as u32 * (255 - a) / 255) as u8;
            }
        }
        Some(RasterGlyph {
            offset: (x0, y0),
            width,
            height,
            rgba,
            is_color,
        })
    }
}

// ---- atlas -------------------------------------------------------------

/// Simple shelf packer over a square texture.
struct Atlas {
    x: u32,
    y: u32,
    shelf_height: u32,
}

impl Atlas {
    fn new() -> Atlas {
        Atlas {
            x: 1,
            y: 1,
            shelf_height: 0,
        }
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        const PAD: u32 = 1;
        if w + 2 * PAD > ATLAS_SIZE || h + 2 * PAD > ATLAS_SIZE {
            return None;
        }
        if self.x + w + PAD > ATLAS_SIZE {
            self.x = PAD;
            self.y += self.shelf_height + PAD;
            self.shelf_height = 0;
        }
        if self.y + h + PAD > ATLAS_SIZE {
            return None;
        }
        let at = (self.x, self.y);
        self.x += w + PAD;
        self.shelf_height = self.shelf_height.max(h);
        Some(at)
    }
}

#[derive(Clone, Copy)]
struct CachedGlyph {
    uv: [f32; 4],
    offset: (i32, i32),
    is_color: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    c: char,
    bold: bool,
    italic: bool,
    /// Drawn at this pixel size for the UI (close buttons) rather than at
    /// cell size; 0 for cells.
    ui: u16,
}

struct AtlasFull;

// ---- GPU data ----------------------------------------------------------

/// One quad. `kind`: 0 = solid fill, 1 = mask glyph tinted by `color`,
/// 2 = color glyph (emoji) drawn as-is, 3 = solid fill with corners
/// rounded by `uv_pos[0]` pixels, 4 = solid fill outside the circle at
/// `uv_pos` (in the quad) of radius `uv_size[0]`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 4],
    kind: u32,
}

impl Instance {
    fn rect(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Instance {
        Instance {
            pos: [x, y],
            size: [w, h],
            uv_pos: [0.0; 2],
            uv_size: [0.0; 2],
            color,
            kind: 0,
        }
    }
}

/// A filled rectangle with corners of radius `r`.
fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32, color: [f32; 4]) -> Instance {
    Instance {
        pos: [x, y],
        size: [w, h],
        uv_pos: [r, 0.0],
        uv_size: [0.0; 2],
        color,
        kind: 3,
    }
}

/// The outside of a rounded corner of radius `r` whose square is at
/// (x, y); `(cx, cy)` is the circle's centre in the square: (r, r) for a
/// top-left corner, (0, r) for a top-right one.
fn fillet(x: f32, y: f32, r: f32, (cx, cy): (f32, f32), color: [f32; 4]) -> Instance {
    Instance {
        pos: [x, y],
        size: [r, r],
        uv_pos: [cx, cy],
        uv_size: [r, 0.0],
        color,
        kind: 4,
    }
}

fn rect(r: Rect, color: [f32; 4]) -> Instance {
    Instance::rect(r.x, r.y, r.w, r.h, color)
}

/// Quads by draw order.
#[derive(Default)]
struct Layers {
    backgrounds: Vec<Instance>,
    /// Pictures, drawn over the backgrounds and under the text.
    images: Vec<ImageDraw>,
    glyphs: Vec<Instance>,
    decorations: Vec<Instance>,
    overlays: Vec<Instance>,
}

/// One picture to draw: its quad (uv in image pixels) and the pane it is
/// clipped to.
struct ImageDraw {
    image: Arc<Image>,
    quad: Instance,
    clip: Rect,
    /// Drawn under everything else: the background picture.
    under: bool,
}

/// A picture uploaded to the GPU.
struct GpuImage {
    bind_group: wgpu::BindGroup,
    /// Screen size and the texture's size, for the shader.
    uniforms: wgpu::Buffer,
    size: (u32, u32),
    /// The frame it was last drawn in.
    used: u64,
}

/// Commands the palette shows at once.
const PALETTE_ROWS: usize = 12;

const SHADER: &str = r#"
struct Uniforms {
    screen: vec2<f32>,
    atlas: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    /// Pixel position inside the quad, for rounded corners.
    @location(3) local: vec2<f32>,
    /// The quad's size.
    @location(4) @interpolate(flat) size: vec2<f32>,
    /// uv_pos and uv_size as given, which shapes use for their own ends.
    @location(5) @interpolate(flat) extra: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_pos: vec2<f32>,
    @location(3) uv_size: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) kind: u32,
) -> VsOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = pos + corner * size;
    var out: VsOut;
    out.pos = vec4<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0, 0.0, 1.0);
    out.uv = (uv_pos + corner * uv_size) / u.atlas;
    out.color = color;
    out.kind = kind;
    out.local = corner * size;
    out.size = size;
    out.extra = vec4<f32>(uv_pos, uv_size);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let t = textureSampleLevel(atlas_tex, atlas_smp, in.uv, 0.0);
    if (in.kind == 0u) {
        return in.color;
    }
    if (in.kind == 1u) {
        return vec4<f32>(in.color.rgb, in.color.a * t.a);
    }
    if (in.kind == 3u) {
        // A rounded rectangle: signed distance to its edge, antialiased
        // over one pixel.
        let half = in.size * 0.5;
        let r = min(in.extra.x, min(half.x, half.y));
        let q = abs(in.local - half) - half + vec2<f32>(r);
        let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        return vec4<f32>(in.color.rgb, in.color.a * clamp(0.5 - d, 0.0, 1.0));
    }
    if (in.kind == 4u) {
        // Everything outside a circle (centre extra.xy, radius extra.z):
        // the fill around a rounded corner.
        let d = length(in.local - in.extra.xy) - in.extra.z;
        return vec4<f32>(in.color.rgb, in.color.a * clamp(d + 0.5, 0.0, 1.0));
    }
    return t;
}
"#;

// ---- colors ------------------------------------------------------------

struct Colors {
    background: Rgb,
    foreground: Rgb,
    cursor: Rgb,
    selection: Rgb,
    palette: [Rgb; 256],
}

impl Colors {
    fn from_theme(theme: &Theme) -> Colors {
        let mut palette = [(0, 0, 0); 256];
        palette[..16].copy_from_slice(&theme.palette);
        // xterm 6×6×6 color cube, then the 24-step grayscale ramp.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        for i in 0..216 {
            palette[16 + i] = (LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]);
        }
        for i in 0..24 {
            let v = 8 + 10 * i as u8;
            palette[232 + i] = (v, v, v);
        }
        Colors {
            background: theme.background,
            foreground: theme.foreground,
            cursor: theme.cursor,
            selection: theme.selection,
            palette,
        }
    }

    fn resolve(&self, c: Color, is_fg: bool, bold: bool) -> Rgb {
        match c {
            Color::Default if is_fg => self.foreground,
            Color::Default => self.background,
            // Bold text in a dark ANSI color shows the bright variant, as
            // conhost and most terminals do.
            Color::Indexed(i) if is_fg && bold && i < 8 => self.palette[i as usize + 8],
            Color::Indexed(i) => self.palette[i as usize],
            Color::Rgb(r, g, b) => (r, g, b),
        }
    }
}

/// `a` moved toward `b` by `t` (0..=1).
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn luma(c: Rgb) -> f32 {
    0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// ---- frame description -----------------------------------------------

/// One pane to draw.
pub struct PaneView<'a> {
    pub grid: &'a Grid,
    /// The pane's area in window pixels; text is inset by the padding.
    pub rect: Rect,
    /// The pane that gets the keyboard in its tab.
    pub focused: bool,
    /// Search hits to highlight.
    pub matches: &'a [Match],
    pub current_match: Option<usize>,
    /// A link under the mouse to underline (inclusive).
    pub link: Option<(Point, Point)>,
    /// Quick-select targets with their labels.
    pub hints: &'a [Hint],
}

/// A quick-select target: a span of history and the keys that pick it.
#[derive(Clone, Debug)]
pub struct Hint {
    pub start: Point,
    pub end: Point,
    pub label: String,
}

/// The command palette: a filter box over a list of commands.
pub struct PaletteView<'a> {
    /// Shown in the box before anything is typed.
    pub placeholder: &'a str,
    pub query: &'a str,
    /// (label, key or detail shown on the right)
    pub items: Vec<(&'a str, &'a str)>,
    pub selected: usize,
}

/// The find box, drawn in the focused pane.
pub struct SearchBar<'a> {
    pub query: &'a str,
    pub current: Option<usize>,
    pub total: usize,
}

/// Everything on screen this frame.
pub struct Frame<'a> {
    pub panes: Vec<PaneView<'a>>,
    pub dividers: Vec<Rect>,
    /// Tab titles; empty hides the tab strip.
    pub tabs: Vec<String>,
    pub active_tab: usize,
    pub search: Option<SearchBar<'a>>,
    /// Blink phase: false hides the focused pane's cursor.
    pub cursor_on: bool,
    /// Visual bell.
    pub flash: bool,
    /// The settings page, drawn instead of the panes.
    pub settings: Option<SettingsPage<'a>>,
    /// The command palette, over everything.
    pub palette: Option<PaletteView<'a>>,
}

/// How a settings row is edited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    Choice,
    /// A slider at this position, 0..=1.
    Slider(f32),
    Toggle(bool),
    /// A text box; `presets` shows ‹ › to step through suggestions.
    Text {
        presets: bool,
    },
}

#[derive(Clone, Debug)]
pub struct SettingsRow {
    pub label: String,
    pub value: String,
    pub control: Control,
}

/// The settings page: a list of settings beside a live preview.
pub struct SettingsPage<'a> {
    /// The area below the tab strip, which the page fills.
    pub area: Rect,
    pub rows: Vec<SettingsRow>,
    pub selected: usize,
    /// First row shown (the list scrolls when it doesn't fit).
    pub scroll: usize,
    pub help: &'a str,
    /// A sample terminal drawn with the settings being edited.
    pub preview: &'a Grid,
    /// Unsaved changes: Save lights up.
    pub dirty: bool,
    pub hover: Option<SettingsHit>,
}

/// What a click on the settings page landed on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingsHit {
    Row(usize),
    Prev(usize),
    Next(usize),
    /// A slider, at this position 0..=1.
    Slider(usize, f32),
    Save,
    Cancel,
    EditFile,
}

/// Where the settings page puts things.
struct PageLayout {
    title: (f32, f32),
    /// (row index, whole row, prev arrow, value, next arrow)
    rows: Vec<(usize, Rect, Rect, Rect, Rect)>,
    help: (f32, f32),
    save: Rect,
    cancel: Rect,
    edit: Rect,
    preview: Rect,
    visible: usize,
}

impl<'a> Frame<'a> {
    /// A single full-window pane with nothing else on top.
    pub fn single(grid: &'a Grid, rect: Rect) -> Frame<'a> {
        Frame {
            panes: vec![PaneView {
                grid,
                rect,
                focused: true,
                matches: &[],
                current_match: None,
                link: None,
                hints: &[],
            }],
            dividers: Vec::new(),
            tabs: Vec::new(),
            active_tab: 0,
            search: None,
            cursor_on: true,
            flash: false,
            settings: None,
            palette: None,
        }
    }
}

/// What a click in the tab strip landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabHit {
    Tab(usize),
    Close(usize),
    New,
    Settings,
    Minimize,
    Maximize,
    CloseWindow,
    /// Bare strip: drags the window when the title bar is ours.
    Empty,
}

/// The tab strip's pieces.
struct Strip {
    /// (tab, its close button)
    tabs: Vec<(Rect, Rect)>,
    new: Rect,
    settings: Rect,
    /// Minimize, maximize, close, when tacoterm draws the title bar.
    buttons: Option<[Rect; 3]>,
}

fn hit_row(h: SettingsHit) -> Option<usize> {
    match h {
        SettingsHit::Row(i) | SettingsHit::Prev(i) | SettingsHit::Next(i) => Some(i),
        SettingsHit::Slider(i, _) => Some(i),
        _ => None,
    }
}

// ---- renderer ----------------------------------------------------------

enum Target {
    Window {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
    Offscreen {
        texture: wgpu::Texture,
    },
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target: Target,
    /// Whether the target format is sRGB (colors must then be linearized).
    srgb: bool,
    width: u32,
    height: u32,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    atlas_texture: wgpu::Texture,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    fonts: Fonts,
    font_config: Config,
    atlas: Atlas,
    glyphs: HashMap<GlyphKey, Option<CachedGlyph>>,
    colors: Colors,
    padding: u32,
    cursor_style: CursorStyle,
    /// DPI scale the fonts were built for.
    scale: f64,
    /// Solid cursor when focused, outlined when not.
    pub focused: bool,
    alpha_mode: wgpu::CompositeAlphaMode,
    opacity: f32,
    /// Presenting through DirectComposition (see [`Renderer::new`]).
    composition: bool,
    /// Draw minimize/maximize/close in the tab strip (custom title bar).
    pub window_buttons: bool,
    pub maximized: bool,
    /// The strip item under the mouse, highlighted.
    pub hover: Option<TabHit>,
    bind_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    images: HashMap<u64, GpuImage>,
    frame_count: u64,
    /// The config's background picture, its alpha already faded.
    backdrop: Option<Arc<Image>>,
}

/// The config's `background_image`, faded to `background_image_opacity`,
/// if it is set and loads.
fn load_backdrop(config: &Config) -> Option<Arc<Image>> {
    let path = config.background_image_path()?;
    let picture = image::open(path).ok()?.into_rgba8();
    let (w, h) = picture.dimensions();
    let fade = config.background_image_opacity.clamp(0.0, 1.0);
    let mut rgba = picture.into_raw();
    for a in rgba.iter_mut().skip(3).step_by(4) {
        *a = (*a as f32 * fade).round() as u8;
    }
    Some(Image::new(w, h, rgba))
}

impl Renderer {
    /// Create a renderer drawing into `window`.
    ///
    /// On Windows this prefers DX12 presenting through DirectComposition,
    /// the one path whose swapchain can be see-through (for `opacity` and
    /// acrylic/mica backdrops); anything else draws opaque.
    pub fn new(window: Arc<Window>, fonts: Fonts, config: &Config) -> Result<Renderer, String> {
        let size = window.inner_size();
        let scale = window.scale_factor();
        let mut attempts = Vec::new();
        if cfg!(windows) {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
            desc.backends = wgpu::Backends::DX12;
            desc.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
            attempts.push(desc);
        }
        attempts.push(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let mut last_error = String::new();
        for (attempt, desc) in attempts.into_iter().enumerate() {
            match Self::open_window(window.clone(), desc) {
                Ok((device, queue, surface, caps)) => {
                    let format = caps
                        .formats
                        .iter()
                        .copied()
                        .find(|f| !f.is_srgb())
                        .or_else(|| caps.formats.first().copied())
                        .ok_or("GPU surface supports no formats")?;
                    let alpha_mode = [
                        wgpu::CompositeAlphaMode::PreMultiplied,
                        wgpu::CompositeAlphaMode::PostMultiplied,
                    ]
                    .into_iter()
                    .find(|m| caps.alpha_modes.contains(m))
                    .unwrap_or(caps.alpha_modes[0]);
                    let surface_config = wgpu::SurfaceConfiguration {
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        format,
                        color_space: wgpu::SurfaceColorSpace::Auto,
                        width: size.width.max(1),
                        height: size.height.max(1),
                        present_mode: wgpu::PresentMode::Fifo,
                        desired_maximum_frame_latency: 2,
                        alpha_mode,
                        view_formats: vec![],
                    };
                    surface.configure(&device, &surface_config);
                    let target = Target::Window {
                        surface,
                        config: surface_config,
                    };
                    let mut r = Renderer::build(
                        device,
                        queue,
                        target,
                        format,
                        (size.width.max(1), size.height.max(1)),
                        fonts,
                        config,
                        scale,
                    );
                    r.alpha_mode = alpha_mode;
                    r.composition = cfg!(windows) && attempt == 0;
                    return Ok(r);
                }
                Err(e) => last_error = e,
            }
        }
        Err(last_error)
    }

    #[allow(clippy::type_complexity)]
    fn open_window(
        window: Arc<Window>,
        desc: wgpu::InstanceDescriptor,
    ) -> Result<
        (
            wgpu::Device,
            wgpu::Queue,
            wgpu::Surface<'static>,
            wgpu::SurfaceCapabilities,
        ),
        String,
    > {
        let instance = wgpu::Instance::new(desc);
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("cannot create GPU surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no compatible GPU adapter found: {e}"))?;
        let (device, queue) = request_device(&adapter)?;
        let caps = surface.get_capabilities(&adapter);
        if caps.formats.is_empty() || caps.alpha_modes.is_empty() {
            return Err("GPU surface supports no formats".to_string());
        }
        Ok((device, queue, surface, caps))
    }

    /// Create a renderer drawing into an offscreen texture (for tests and
    /// screenshots). Fails when no GPU adapter (not even a software one) is
    /// available.
    pub fn headless(
        width: u32,
        height: u32,
        fonts: Fonts,
        config: &Config,
    ) -> Result<Renderer, String> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU adapter found: {e}"))?;
        let (device, queue) = request_device(&adapter)?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Renderer::build(
            device,
            queue,
            Target::Offscreen { texture },
            format,
            (width, height),
            fonts,
            config,
            1.0,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        target: Target,
        format: wgpu::TextureFormat,
        (width, height): (u32, u32),
        fonts: Fonts,
        config: &Config,
        scale: f64,
    ) -> Renderer {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacoterm"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        // Glyphs land on whole pixels at their native size: nearest keeps
        // them crisp.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacoterm"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tacoterm"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        // Pictures shrunk to their cells are smoothed; enlarged ones keep
        // their pixels sharp (icons, pixel art).
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacoterm"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacoterm"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x2,
                        3 => Float32x2,
                        4 => Float32x4,
                        5 => Uint32,
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let instance_capacity = 4096;
        let instances = Self::instance_buffer(&device, instance_capacity);
        Renderer {
            device,
            queue,
            target,
            srgb: format.is_srgb(),
            width,
            height,
            pipeline,
            bind_group,
            uniforms,
            atlas_texture,
            instances,
            instance_capacity,
            fonts,
            font_config: config.clone(),
            atlas: Atlas::new(),
            glyphs: HashMap::new(),
            colors: Colors::from_theme(&config.theme().0),
            cursor_style: config.cursor_style(),
            scale,
            padding: (config.padding as f64 * scale).round() as u32,
            focused: true,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            opacity: config.opacity.clamp(0.0, 1.0),
            composition: false,
            window_buttons: false,
            maximized: false,
            hover: None,
            bind_layout: layout,
            image_sampler,
            images: HashMap::new(),
            frame_count: 0,
            backdrop: load_backdrop(config),
        }
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn cell_size(&self) -> (u32, u32) {
        (self.fonts.cell_width, self.fonts.cell_height)
    }

    pub fn padding(&self) -> u32 {
        self.padding
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        if let Target::Window { surface, config } = &mut self.target {
            config.width = self.width;
            config.height = self.height;
            surface.configure(&self.device, config);
        }
    }

    /// The DPI scale changed (window moved to another monitor): re-measure
    /// fonts and drop cached glyphs.
    pub fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
        self.reload_fonts();
    }

    fn reload_fonts(&mut self) {
        self.fonts = Fonts::new(&self.font_config, self.scale);
        self.padding = (self.font_config.padding as f64 * self.scale).round() as u32;
        self.glyphs.clear();
        self.atlas = Atlas::new();
    }

    /// Apply a changed config: colors and cursor right away, and fonts when
    /// a font setting changed. Returns true when the cell size or padding
    /// may have changed, so the grid needs refitting.
    pub fn set_config(&mut self, config: &Config) -> bool {
        self.set_theme(&config.theme().0);
        self.cursor_style = config.cursor_style();
        self.set_opacity(config.opacity);
        let old = &self.font_config;
        if old.background_image != config.background_image
            || old.background_image_opacity != config.background_image_opacity
        {
            self.backdrop = load_backdrop(config);
        }
        let fonts_changed = old.font_family != config.font_family
            || old.font_size != config.font_size
            || old.line_height != config.line_height
            || old.padding != config.padding;
        self.font_config = config.clone();
        if fonts_changed {
            self.reload_fonts();
        }
        fonts_changed
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.colors = Colors::from_theme(theme);
    }

    pub fn background(&self) -> Rgb {
        self.colors.background
    }

    /// The window size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How see-through the default background is, 0.0..=1.0. Only takes
    /// effect when the surface can composite with alpha.
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
    }

    /// Whether the window can actually be see-through.
    pub fn supports_transparency(&self) -> bool {
        matches!(
            self.alpha_mode,
            wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::PostMultiplied
        )
    }

    /// Height of the tab strip in pixels.
    pub fn tab_bar_height(&self) -> u32 {
        self.fonts.tab_bar_height()
    }

    /// Space between split panes, in pixels.
    /// Physical pixels per logical pixel.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn divider_width(&self) -> u32 {
        (2.0 * self.scale).round().max(1.0) as u32
    }

    /// (cols, rows) that fit a pane rect at the current cell metrics.
    pub fn pane_grid_size(&self, rect: Rect) -> (u16, u16) {
        let (cw, ch) = self.cell_size();
        let w = (rect.w as u32).saturating_sub(2 * self.padding);
        let h = (rect.h as u32).saturating_sub(2 * self.padding);
        (
            (w / cw).clamp(1, u16::MAX as u32) as u16,
            (h / ch).clamp(1, u16::MAX as u32) as u16,
        )
    }

    /// (cols, rows) that fit the whole surface: one pane, no tab strip.
    pub fn grid_size(&self) -> (u16, u16) {
        self.pane_grid_size(self.full_rect())
    }

    fn full_rect(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: self.width as f32,
            h: self.height as f32,
        }
    }

    /// The viewport cell of a pane under a pixel position (clamped).
    pub fn cell_in(&self, rect: Rect, x: f64, y: f64, grid: &Grid) -> (usize, usize) {
        let (cw, ch) = self.cell_size();
        let p = self.padding as f64;
        let col = ((x - rect.x as f64 - p) / cw as f64).floor().max(0.0) as usize;
        let row = ((y - rect.y as f64 - p) / ch as f64).floor().max(0.0) as usize;
        (col.min(grid.cols - 1), row.min(grid.rows - 1))
    }

    /// The viewport cell under a pixel position for a full-window grid.
    pub fn cell_at(&self, x: f64, y: f64, grid: &Grid) -> (usize, usize) {
        self.cell_in(self.full_rect(), x, y, grid)
    }

    /// Whether frames go through DirectComposition, which lets the window
    /// have no GDI surface of its own (see the app's window setup).
    pub fn uses_composition(&self) -> bool {
        self.composition
    }

    /// Installed monospace font families, sorted.
    pub fn monospace_families(&self) -> Vec<String> {
        self.fonts.monospace_families()
    }

    /// The tab strip's layout for `n` tabs.
    fn strip(&self, n: usize) -> Strip {
        let h = self.tab_bar_height() as f32;
        let cw = self.fonts.cell_width as f32;
        let w = self.width as f32;
        let n = n.max(1);
        let button_w = (h * 1.45).round();
        let buttons = self.window_buttons.then(|| {
            [3.0, 2.0, 1.0].map(|k| Rect {
                x: w - k * button_w,
                y: 0.0,
                w: button_w,
                h,
            })
        });
        let right = if buttons.is_some() {
            // Keep some bare strip to drag the window by.
            3.0 * button_w + cw * 6.0
        } else {
            0.0
        };
        let room = (w - right - 2.0 * h).max(cw * 4.0);
        let tab_w = (room / n as f32).clamp(cw * 6.0, cw * 28.0).floor();
        let close = (h * 0.55).round();
        let tabs = (0..n)
            .map(|i| {
                let tab = Rect {
                    x: i as f32 * tab_w,
                    y: 0.0,
                    w: tab_w,
                    h,
                };
                let c = Rect {
                    x: tab.x + tab_w - close - (h - close) / 2.0,
                    y: ((h - close) / 2.0).round(),
                    w: close,
                    h: close,
                };
                (tab, c)
            })
            .collect();
        let new = Rect {
            x: n as f32 * tab_w,
            y: 0.0,
            w: h,
            h,
        };
        let settings = Rect {
            x: match buttons {
                Some(b) => b[0].x - h,
                None => w - h,
            },
            y: 0.0,
            w: h,
            h,
        };
        Strip {
            tabs,
            new,
            settings,
            buttons,
        }
    }

    /// What's under a point in the tab strip.
    pub fn tab_hit(&self, x: f64, y: f64, n: usize) -> Option<TabHit> {
        let (x, y) = (x as f32, y as f32);
        if y >= self.tab_bar_height() as f32 || y < 0.0 {
            return None;
        }
        let strip = self.strip(n);
        if let Some([min, max, close]) = strip.buttons {
            if min.contains(x, y) {
                return Some(TabHit::Minimize);
            }
            if max.contains(x, y) {
                return Some(TabHit::Maximize);
            }
            if close.contains(x, y) {
                return Some(TabHit::CloseWindow);
            }
        }
        if strip.settings.contains(x, y) {
            return Some(TabHit::Settings);
        }
        if strip.new.contains(x, y) {
            return Some(TabHit::New);
        }
        let tab = strip.tabs.iter().enumerate().find_map(|(i, (tab, close))| {
            if close.contains(x, y) && n > 1 {
                Some(TabHit::Close(i))
            } else if tab.contains(x, y) {
                Some(TabHit::Tab(i))
            } else {
                None
            }
        });
        Some(tab.unwrap_or(TabHit::Empty))
    }

    fn color(&self, rgb: Rgb, alpha: f32) -> [f32; 4] {
        if self.srgb {
            [
                srgb_to_linear(rgb.0),
                srgb_to_linear(rgb.1),
                srgb_to_linear(rgb.2),
                alpha,
            ]
        } else {
            [
                rgb.0 as f32 / 255.0,
                rgb.1 as f32 / 255.0,
                rgb.2 as f32 / 255.0,
                alpha,
            ]
        }
    }

    /// Alpha for surfaces that should let the backdrop through.
    fn see_through(&self) -> f32 {
        if self.supports_transparency() {
            self.opacity
        } else {
            1.0
        }
    }

    fn line_px(&self) -> f32 {
        (self.fonts.size / 14.0).round().max(1.0)
    }

    fn glyph(&mut self, key: GlyphKey) -> Result<Option<CachedGlyph>, AtlasFull> {
        if let Some(g) = self.glyphs.get(&key) {
            return Ok(*g);
        }
        let raster = if boxdraw::handles(key.c) {
            let (w, h) = self.cell_size();
            boxdraw::draw(key.c, w, h, self.line_px() as u32).map(|mask| RasterGlyph {
                offset: (0, 0),
                width: w,
                height: h,
                rgba: mask.iter().flat_map(|&a| [255, 255, 255, a]).collect(),
                is_color: false,
            })
        } else {
            let text = crate::grid::cluster_text(key.c).unwrap_or_else(|| key.c.to_string());
            self.fonts.rasterize(&text, key.bold, key.italic)
        };
        let Some(raster) = raster else {
            self.glyphs.insert(key, None);
            return Ok(None);
        };
        let Some((x, y)) = self.atlas.alloc(raster.width, raster.height) else {
            if raster.width + 2 > ATLAS_SIZE || raster.height + 2 > ATLAS_SIZE {
                // Too big to ever fit: draw nothing for it.
                self.glyphs.insert(key, None);
                return Ok(None);
            }
            return Err(AtlasFull);
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &raster.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(raster.width * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: raster.width,
                height: raster.height,
                depth_or_array_layers: 1,
            },
        );
        let cached = CachedGlyph {
            uv: [
                x as f32,
                y as f32,
                raster.width as f32,
                raster.height as f32,
            ],
            offset: raster.offset,
            is_color: raster.is_color,
        };
        self.glyphs.insert(key, Some(cached));
        Ok(Some(cached))
    }

    fn glyph_instance(g: CachedGlyph, x: f32, y: f32, color: [f32; 4]) -> Instance {
        Instance {
            pos: [x + g.offset.0 as f32, y + g.offset.1 as f32],
            size: [g.uv[2], g.uv[3]],
            uv_pos: [g.uv[0], g.uv[1]],
            uv_size: [g.uv[2], g.uv[3]],
            color,
            kind: if g.is_color { 2 } else { 1 },
        }
    }

    /// Lay `text` out on the cell grid starting at (x, y), at most `max_w`
    /// pixels wide (cut with an ellipsis). Returns the width used.
    fn text(
        &mut self,
        out: &mut Vec<Instance>,
        (x, y): (f32, f32),
        text: &str,
        color: [f32; 4],
        max_w: f32,
    ) -> Result<f32, AtlasFull> {
        let cw = self.fonts.cell_width as f32;
        let max_cols = (max_w / cw).floor().max(0.0) as usize;
        let width = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        let total: usize = text.chars().map(width).sum();
        let mut chars: Vec<char> = text.chars().collect();
        if total > max_cols {
            let mut used = 0;
            let keep = chars
                .iter()
                .take_while(|&&c| {
                    used += width(c);
                    used < max_cols
                })
                .count();
            chars.truncate(keep);
            if max_cols > 0 {
                chars.push('…');
            }
        }
        let mut col = 0;
        for c in chars {
            let w = width(c);
            if w == 0 {
                continue;
            }
            if c != ' ' {
                let key = GlyphKey {
                    c,
                    bold: false,
                    italic: false,
                    ui: 0,
                };
                if let Some(g) = self.glyph(key)? {
                    out.push(Self::glyph_instance(g, x + col as f32 * cw, y, color));
                }
            }
            col += w;
        }
        Ok(col as f32 * cw)
    }

    /// Build this frame's quads. Within each layer nothing paints over a
    /// neighbor's glyph: backgrounds, then glyphs, then decorations, then
    /// overlays (dimming, search box, bell flash).
    /// The frame's quads in draw order, how many of them are backgrounds
    /// (pictures go between those and the rest), and the pictures.
    fn build_instances(
        &mut self,
        frame: &Frame,
    ) -> Result<(Vec<Instance>, usize, Vec<ImageDraw>), AtlasFull> {
        let mut layers = Layers::default();
        if let (Some(picture), None) = (&self.backdrop, &frame.settings) {
            // Cover the area below the tab strip, cropping the picture's
            // overflow evenly.
            let top = if frame.tabs.is_empty() {
                0.0
            } else {
                self.tab_bar_height() as f32
            };
            let area = Rect::new(0.0, top, self.width as f32, self.height as f32 - top);
            let (iw, ih) = (picture.width as f32, picture.height as f32);
            let k = (area.w / iw).max(area.h / ih);
            let (sw, sh) = (area.w / k, area.h / k);
            layers.images.push(ImageDraw {
                image: picture.clone(),
                quad: Instance {
                    pos: [area.x, area.y],
                    size: [area.w, area.h],
                    uv_pos: [(iw - sw) / 2.0, (ih - sh) / 2.0],
                    uv_size: [sw, sh],
                    color: [1.0; 4],
                    kind: 2,
                },
                clip: area,
                under: true,
            });
        }
        if let Some(page) = &frame.settings {
            self.build_settings(&mut layers, page, frame.cursor_on)?;
        }
        let multi = frame.panes.len() > 1;
        for pane in &frame.panes {
            self.build_pane(&mut layers, pane, frame.cursor_on)?;
            if multi && !pane.focused {
                let dim = self.color(self.colors.background, 0.35);
                layers.overlays.push(rect(pane.rect, dim));
            }
        }
        let divider = self.color(self.colors.palette[8], 1.0);
        for d in &frame.dividers {
            layers.backgrounds.push(rect(*d, divider));
        }
        if !frame.tabs.is_empty() {
            self.build_tab_bar(&mut layers, frame)?;
        }
        if let (Some(search), Some(pane)) = (&frame.search, frame.panes.iter().find(|p| p.focused))
        {
            self.build_search_bar(&mut layers, search, pane.rect)?;
        }
        if let Some(palette) = &frame.palette {
            self.build_palette(&mut layers, palette)?;
        }
        if frame.flash {
            let flash = self.color(self.colors.foreground, 0.12);
            layers.overlays.push(rect(self.full_rect(), flash));
        }
        let mut out = layers.backgrounds;
        let backgrounds = out.len();
        out.append(&mut layers.glyphs);
        out.append(&mut layers.decorations);
        out.append(&mut layers.overlays);
        Ok((out, backgrounds, layers.images))
    }

    fn build_pane(
        &mut self,
        layers: &mut Layers,
        pane: &PaneView,
        cursor_on: bool,
    ) -> Result<(), AtlasFull> {
        let grid = pane.grid;
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let pad = self.padding as f32;
        let (ox, oy) = (pane.rect.x + pad, pane.rect.y + pad);
        let line_px = self.line_px();

        let style = grid
            .modes
            .cursor_shape
            .map_or(self.cursor_style, |(s, _)| s);
        let show_cursor = grid.display_offset == 0
            && grid.modes.cursor_visible
            && (cursor_on || !pane.focused || !self.focused);
        // Solid only in the pane that has the keyboard.
        let active = pane.focused && self.focused;

        // Search hits that are on screen, as viewport-independent points.
        let top = grid.viewport_point(0, 0).line;
        let bottom = top + grid.rows;
        let hits: Vec<(usize, &Match)> = pane
            .matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.end.line >= top && m.start.line < bottom)
            .collect();
        let match_bg = mix(self.colors.background, self.colors.palette[3], 0.45);
        let current_bg = self.colors.palette[11];
        let hints: Vec<&Hint> = pane
            .hints
            .iter()
            .filter(|h| h.end.line >= top && h.start.line < bottom)
            .collect();
        let quick = !pane.hints.is_empty();
        let hint_bg = mix(self.colors.background, self.colors.palette[4], 0.4);

        for (p, y) in grid.visible_images() {
            let [sx, sy, sw, sh] = p.src;
            let (dw, dh) = if p.fill {
                (p.cols as f32 * cw, p.rows as f32 * ch)
            } else if let Some((w, h)) = p.draw_size {
                (w as f32, h as f32)
            } else {
                (sw as f32, sh as f32)
            };
            layers.images.push(ImageDraw {
                image: p.image.clone(),
                quad: Instance {
                    pos: [
                        ox + p.col as f32 * cw + p.offset.0 as f32,
                        oy + y as f32 * ch + p.offset.1 as f32,
                    ],
                    size: [dw, dh],
                    uv_pos: [sx as f32, sy as f32],
                    uv_size: [sw as f32, sh as f32],
                    color: [1.0; 4],
                    kind: 2,
                },
                clip: Rect {
                    x: ox,
                    y: oy,
                    w: grid.cols as f32 * cw,
                    h: grid.rows as f32 * ch,
                },
                under: false,
            });
        }

        for y in 0..grid.rows {
            let row = grid.visible_row(y);
            let py = oy + y as f32 * ch;
            for x in 0..grid.cols {
                let cell = row.cells.get(x).copied().unwrap_or_default();
                if cell.attr.has(Attr::WIDE_SPACER) {
                    continue;
                }
                let span = if cell.attr.has(Attr::WIDE) { 2.0 } else { 1.0 };
                let px = ox + x as f32 * cw;
                let bold = cell.attr.has(Attr::BOLD);
                let mut fg = self.colors.resolve(cell.fg, true, bold);
                let mut bg = self.colors.resolve(cell.bg, false, false);
                if cell.attr.has(Attr::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let p = grid.viewport_point(x, y);
                if let Some((i, _)) = hits.iter().find(|(_, m)| p >= m.start && p <= m.end) {
                    if pane.current_match == Some(*i) {
                        bg = current_bg;
                        fg = self.colors.background;
                    } else {
                        bg = match_bg;
                    }
                }
                let in_hint = quick && hints.iter().any(|h| p >= h.start && p <= h.end);
                if in_hint {
                    bg = hint_bg;
                }
                if grid.is_selected(x, y) {
                    bg = self.colors.selection;
                }
                let cursor_here = show_cursor && x == grid.cursor_x && y == grid.cursor_y;
                let solid_block = cursor_here && active && style == CursorStyle::Block;
                if solid_block {
                    bg = self.colors.cursor;
                    fg = self.colors.background;
                }
                if bg != self.colors.background {
                    layers.backgrounds.push(Instance::rect(
                        px,
                        py,
                        cw * span,
                        ch,
                        self.color(bg, 1.0),
                    ));
                }
                if cursor_here && !solid_block {
                    let c = self.color(self.colors.cursor, 1.0);
                    let (w, h) = (cw * span, ch);
                    let thick = (line_px * 2.0).max(2.0);
                    let d = &mut layers.decorations;
                    match style {
                        CursorStyle::Bar => d.push(Instance::rect(px, py, thick, h, c)),
                        CursorStyle::Underline => {
                            d.push(Instance::rect(px, py + h - thick, w, thick, c))
                        }
                        CursorStyle::Block => {
                            d.push(Instance::rect(px, py, w, line_px, c));
                            d.push(Instance::rect(px, py + h - line_px, w, line_px, c));
                            d.push(Instance::rect(px, py, line_px, h, c));
                            d.push(Instance::rect(px + w - line_px, py, line_px, h, c));
                        }
                    }
                }
                // Quick select fades everything but its targets.
                let alpha = if quick && !in_hint {
                    0.4
                } else if cell.attr.has(Attr::DIM) {
                    0.6
                } else {
                    1.0
                };
                let fg_color = self.color(fg, alpha);
                if cell.c != ' ' && !cell.attr.has(Attr::HIDDEN) {
                    let key = GlyphKey {
                        c: cell.c,
                        bold,
                        italic: cell.attr.has(Attr::ITALIC),
                        ui: 0,
                    };
                    if let Some(g) = self.glyph(key)? {
                        layers
                            .glyphs
                            .push(Self::glyph_instance(g, px, py, fg_color));
                    }
                }
                let in_link = pane.link.is_some_and(|(a, b)| p >= a && p <= b);
                let uy = (py + self.fonts.baseline + line_px).min(py + ch - line_px);
                let ul_color = match cell.ul {
                    Color::Default => fg_color,
                    c => self.color(self.colors.resolve(c, true, false), alpha),
                };
                let w = cw * span;
                if in_link {
                    layers
                        .decorations
                        .push(Instance::rect(px, uy, w, line_px, fg_color));
                } else if cell.attr.has(Attr::UNDERLINE) {
                    self.underline(layers, cell.attr, (px, uy, w), ch - (uy - py), ul_color);
                } else if cell.link != 0 {
                    // An OSC 8 link: a faint dashed line until hovered.
                    let faint = self.color(fg, alpha * 0.5);
                    self.underline(
                        layers,
                        Attr(Attr::DASHED_UNDERLINE),
                        (px, uy, w),
                        0.0,
                        faint,
                    );
                }
                if cell.attr.has(Attr::STRIKE) {
                    let sy = py + (self.fonts.baseline * 0.65).round();
                    layers
                        .decorations
                        .push(Instance::rect(px, sy, cw * span, line_px, fg_color));
                }
            }
        }

        // Quick-select labels over the start of each target.
        let label_bg = self.color(self.colors.palette[11], 1.0);
        let label_fg = self.color((16, 16, 16), 1.0);
        let radius = (3.0 * self.scale as f32).round();
        let mut label_glyphs = Vec::new();
        for h in &hints {
            let Some(vy) = h.start.line.checked_sub(top) else {
                continue;
            };
            let lx = ox + h.start.col as f32 * cw;
            let ly = oy + vy as f32 * ch;
            let lw = h.label.chars().count() as f32 * cw;
            layers
                .overlays
                .push(rounded(lx - 2.0, ly, lw + 4.0, ch, radius, label_bg));
            self.text(&mut label_glyphs, (lx, ly), &h.label, label_fg, lw + 1.0)?;
        }
        layers.overlays.append(&mut label_glyphs);

        // A scroll thumb while looking back through history.
        if grid.display_offset > 0 {
            let total = (grid.scrollback_len() + grid.rows) as f32;
            let track = pane.rect.h - 2.0 * pad;
            let h = (grid.rows as f32 / total * track).max(ch);
            let first = (grid.scrollback_len() - grid.display_offset) as f32;
            let y = pane.rect.y + pad + first / total * (track - h);
            let w = (3.0 * self.scale as f32).round();
            let x = pane.rect.x + pane.rect.w - w - 2.0;
            let c = self.color(self.colors.foreground, 0.4);
            layers.overlays.push(Instance::rect(x, y, w, h, c));
        }
        Ok(())
    }

    /// An underline in `attr`'s style across `w` pixels at (x, y); `room`
    /// is the space left below y in the cell.
    fn underline(
        &self,
        layers: &mut Layers,
        attr: Attr,
        (x, y, w): (f32, f32, f32),
        room: f32,
        color: [f32; 4],
    ) {
        let t = self.line_px();
        let d = &mut layers.decorations;
        if attr.has(Attr::DOUBLE_UNDERLINE) {
            // Two lines a line apart; nudged up if the cell runs out.
            let y = (y - (3.0 * t - room).max(0.0)).round();
            d.push(Instance::rect(x, y, w, t, color));
            d.push(Instance::rect(x, y + 2.0 * t, w, t, color));
        } else if attr.has(Attr::CURLY_UNDERLINE) {
            // A wave a few pixels tall, one column at a time, phased by x
            // so it runs on across cells.
            let amp = (t * 1.5).max(1.5);
            let period = (self.fonts.cell_width as f32).max(4.0);
            let base = y - amp / 2.0;
            let mut cx = x;
            while cx < x + w {
                let phase = (cx / period) * std::f32::consts::TAU;
                let cy = base + amp * (0.5 - 0.5 * phase.sin());
                d.push(Instance::rect(cx, cy.round(), 1.0, t, color));
                cx += 1.0;
            }
        } else if attr.has(Attr::DOTTED_UNDERLINE) {
            let step = t * 2.0;
            let mut cx = (x / step).ceil() * step;
            while cx < x + w {
                d.push(Instance::rect(cx, y, t, t, color));
                cx += step;
            }
        } else if attr.has(Attr::DASHED_UNDERLINE) {
            let dash = (self.fonts.cell_width as f32 / 2.0).round().max(2.0);
            let mut cx = x;
            while cx < x + w {
                d.push(Instance::rect(
                    cx,
                    y,
                    (dash * 0.6).round().min(x + w - cx),
                    t,
                    color,
                ));
                cx += dash;
            }
        } else {
            d.push(Instance::rect(x, y, w, t, color));
        }
    }

    fn build_tab_bar(&mut self, layers: &mut Layers, frame: &Frame) -> Result<(), AtlasFull> {
        let alpha = self.see_through();
        let bg = self.colors.background;
        let bar = if luma(bg) < 128.0 {
            mix(bg, (0, 0, 0), 0.35)
        } else {
            mix(bg, (0, 0, 0), 0.08)
        };
        let strip = self.strip(frame.tabs.len());
        let h = self.tab_bar_height() as f32;
        let (_, ch) = self.cell_size();
        let cw = self.fonts.cell_width as f32;
        let scale = self.scale as f32;
        // Tabs are cards that start a little below the top of the strip.
        let top = (5.0 * scale).round();
        let inset = (3.0 * scale).round();
        let radius = (7.0 * scale).round().min(h / 3.0);
        let text_y = ((top + h - ch as f32) / 2.0).round();
        // The active tab shows the pane background so it reads as part of
        // the terminal below. With the settings page up no tab is.
        let active = if frame.settings.is_some() {
            None
        } else {
            strip.tabs.get(frame.active_tab).map(|t| t.0)
        };
        let bar_color = self.color(bar, alpha);
        let b = &mut layers.backgrounds;
        let full_w = self.width as f32;
        match active {
            Some(a) => {
                // The strip around the card, with its top corners rounded.
                let (ax, aw) = (a.x + inset, a.w - 2.0 * inset);
                b.push(Instance::rect(0.0, 0.0, full_w, top, bar_color));
                b.push(Instance::rect(0.0, top, ax, h - top, bar_color));
                b.push(Instance::rect(
                    ax + aw,
                    top,
                    full_w - ax - aw,
                    h - top,
                    bar_color,
                ));
                b.push(fillet(ax, top, radius, (radius, radius), bar_color));
                b.push(fillet(
                    ax + aw - radius,
                    top,
                    radius,
                    (0.0, radius),
                    bar_color,
                ));
            }
            None => b.push(Instance::rect(0.0, 0.0, full_w, h, bar_color)),
        }
        let fg = self.colors.foreground;
        let dim = mix(fg, bar, 0.45);
        let hover_bg = self.color(fg, 0.08);
        let hover = self.hover;
        // Each tab gets a colored dot, so tabs are easy to tell apart.
        const DOTS: [usize; 6] = [12, 13, 14, 10, 11, 9];
        let dot = (ch as f32 * 0.4).round().max(4.0);
        for (i, (title, (tab, close))) in frame.tabs.iter().zip(&strip.tabs).enumerate() {
            let is_active = Some(*tab) == active;
            let color = self.color(if is_active { fg } else { dim }, 1.0);
            let card = Rect::new(tab.x + inset, top, tab.w - 2.0 * inset, h - top - inset);
            if !is_active {
                if hover == Some(TabHit::Tab(i)) || hover == Some(TabHit::Close(i)) {
                    layers
                        .backgrounds
                        .push(rounded(card.x, card.y, card.w, card.h, radius, hover_bg));
                } else if i + 1 != frame.active_tab && i + 1 < strip.tabs.len() {
                    // Separator between two inactive tabs.
                    let sep = self.color(dim, 0.4);
                    layers.decorations.push(Instance::rect(
                        tab.x + tab.w - 1.0,
                        top + (h - top) * 0.25,
                        1.0,
                        (h - top) * 0.45,
                        sep,
                    ));
                }
            }
            let dot_rgb = self.colors.palette[DOTS[i % DOTS.len()]];
            let dot_color = self.color(dot_rgb, if is_active { 1.0 } else { 0.55 });
            let dot_x = (tab.x + inset + cw).round();
            let dot_y = ((top + h - dot) / 2.0).round();
            layers
                .decorations
                .push(rounded(dot_x, dot_y, dot, dot, dot / 2.0, dot_color));
            let text_x = dot_x + dot + (cw * 0.8).round();
            let max_w = close.x - text_x - cw * 0.5;
            self.text(&mut layers.glyphs, (text_x, text_y), title, color, max_w)?;
            if strip.tabs.len() > 1 {
                if hover == Some(TabHit::Close(i)) {
                    layers.backgrounds.push(rounded(
                        close.x,
                        close.y + top / 2.0,
                        close.w,
                        close.h,
                        close.w / 2.0,
                        self.color(fg, 0.15),
                    ));
                }
                let mut c = *close;
                c.y += top / 2.0;
                // Inactive tabs show their × only on hover, like browsers.
                if is_active || hover == Some(TabHit::Tab(i)) || hover == Some(TabHit::Close(i)) {
                    self.cross(layers, c, (close.w * 0.45).round(), color);
                }
            }
        }
        let c = self.color(dim, 1.0);
        let t = self.line_px().max(1.0) + (scale - 1.0).max(0.0);
        // "+" for a new tab.
        let n = strip.new;
        let new_y = top / 2.0;
        if hover == Some(TabHit::New) {
            let pad = (h - top) * 0.15;
            layers.backgrounds.push(rounded(
                n.x + pad,
                top + pad - inset / 2.0,
                n.w - 2.0 * pad,
                h - top - 2.0 * pad,
                radius,
                hover_bg,
            ));
        }
        let arm = (h * 0.3).round();
        let (cx, cy) = (n.x + n.w / 2.0, h / 2.0 + new_y - inset / 2.0);
        layers.decorations.push(Instance::rect(
            (cx - arm / 2.0).round(),
            (cy - t / 2.0).round(),
            arm,
            t,
            c,
        ));
        layers.decorations.push(Instance::rect(
            (cx - t / 2.0).round(),
            (cy - arm / 2.0).round(),
            t,
            arm,
            c,
        ));
        // Settings gear.
        let s = strip.settings;
        if hover == Some(TabHit::Settings) || frame.settings.is_some() {
            let pad = (h - top) * 0.15;
            layers.backgrounds.push(rounded(
                s.x + pad,
                top + pad - inset / 2.0,
                s.w - 2.0 * pad,
                h - top - 2.0 * pad,
                radius,
                hover_bg,
            ));
        }
        let gear_color = self.color(if frame.settings.is_some() { fg } else { dim }, 1.0);
        self.text(
            &mut layers.glyphs,
            (
                (s.x + (s.w - cw) / 2.0).round(),
                text_y - (inset / 2.0).round(),
            ),
            "⚙",
            gear_color,
            cw * 2.0,
        )?;
        // Window buttons, Windows-style: a wide hover, red for close.
        if let Some([min, max, close]) = strip.buttons {
            let size = (10.0 * self.scale as f32).round();
            let icon = self.color(fg, 1.0);
            for (r, hit) in [
                (min, TabHit::Minimize),
                (max, TabHit::Maximize),
                (close, TabHit::CloseWindow),
            ] {
                if hover == Some(hit) {
                    let hl = if hit == TabHit::CloseWindow {
                        [0.77, 0.17, 0.11, 1.0]
                    } else {
                        hover_bg
                    };
                    layers.backgrounds.push(rect(r, hl));
                }
            }
            let (mx, my) = ((min.x + (min.w - size) / 2.0).round(), (h / 2.0).round());
            layers
                .decorations
                .push(Instance::rect(mx, my, size, t, icon));
            let (bx, by) = (
                (max.x + (max.w - size) / 2.0).round(),
                ((h - size) / 2.0).round(),
            );
            let square = |x: f32, y: f32, s: f32, d: &mut Vec<Instance>| {
                d.push(Instance::rect(x, y, s, t, icon));
                d.push(Instance::rect(x, y + s - t, s, t, icon));
                d.push(Instance::rect(x, y, t, s, icon));
                d.push(Instance::rect(x + s - t, y, t, s, icon));
            };
            if self.maximized {
                // Two overlapping windows: "restore".
                let off = (2.0 * self.scale as f32).round();
                let small = size - off;
                square(bx, by + off, small, &mut layers.decorations);
                layers
                    .decorations
                    .push(Instance::rect(bx + off, by, small, t, icon));
                layers
                    .decorations
                    .push(Instance::rect(bx + size - t, by, t, small, icon));
            } else {
                square(bx, by, size, &mut layers.decorations);
            }
            let x_color = if hover == Some(TabHit::CloseWindow) {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                icon
            };
            self.cross(layers, close, size, x_color);
        }
        Ok(())
    }

    /// A × of `size` pixels centered in `r`, drawn with the box-drawing
    /// diagonal.
    fn cross(&mut self, layers: &mut Layers, r: Rect, size: f32, color: [f32; 4]) {
        let size = size.max(4.0) as u32;
        let key = GlyphKey {
            c: '\u{2573}',
            bold: false,
            italic: false,
            ui: size as u16,
        };
        if let Some(g) = self.cached_cross(key, size) {
            let x = (r.x + (r.w - size as f32) / 2.0).round();
            let y = (r.y + (r.h - size as f32) / 2.0).round();
            layers.glyphs.push(Self::glyph_instance(g, x, y, color));
        }
    }

    fn cached_cross(&mut self, key: GlyphKey, size: u32) -> Option<CachedGlyph> {
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        let mask = boxdraw::draw(key.c, size, size, self.line_px() as u32)?;
        let (x, y) = self.atlas.alloc(size, size)?;
        let rgba: Vec<u8> = mask.iter().flat_map(|&a| [255, 255, 255, a]).collect();
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );
        let g = CachedGlyph {
            uv: [x as f32, y as f32, size as f32, size as f32],
            offset: (0, 0),
            is_color: false,
        };
        self.glyphs.insert(key, Some(g));
        Some(g)
    }

    // ---- settings page --------------------------------------------------

    fn page_layout(&self, area: Rect, rows: usize, scroll: usize) -> PageLayout {
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let margin = 2.0 * cw;
        let rh = (ch * 1.7).round();
        let x0 = area.x + margin;
        let title = (x0, area.y + ch);
        let list_top = area.y + ch * 3.0;
        let bottom = area.y + area.h - margin * 0.5;
        let buttons_y = bottom - rh;
        let help = (x0, buttons_y - ch * 1.6);
        let list_w = 46.0 * cw;
        let side_by_side = area.w >= list_w + 40.0 * cw;
        let list_bottom_full = help.1 - ch * 0.5;
        let (list_bottom, preview) = if side_by_side {
            let x = x0 + list_w + margin;
            (
                list_bottom_full,
                Rect {
                    x,
                    y: list_top + ch * 1.5,
                    w: (area.x + area.w - margin - x).max(cw),
                    h: (list_bottom_full - list_top - ch * 1.5).max(ch),
                },
            )
        } else {
            let ph = ((list_bottom_full - list_top) * 0.45).max(ch * 4.0);
            let y = list_bottom_full - ph;
            (
                y - ch * 2.0,
                Rect {
                    x: x0,
                    y,
                    w: (area.w - 2.0 * margin).max(cw),
                    h: ph,
                },
            )
        };
        let visible = (((list_bottom - list_top) / rh).floor().max(1.0)) as usize;
        let label_w = 18.0 * cw;
        let control_w = list_w - label_w - margin;
        let arrow = 2.5 * cw;
        let rows = (scroll..rows.min(scroll + visible))
            .enumerate()
            .map(|(k, i)| {
                let y = list_top + k as f32 * rh;
                let row = Rect {
                    x: x0 - cw,
                    y,
                    w: list_w,
                    h: rh,
                };
                let cx = x0 + label_w;
                let prev = Rect {
                    x: cx,
                    y,
                    w: arrow,
                    h: rh,
                };
                let next = Rect {
                    x: cx + control_w - arrow,
                    y,
                    w: arrow,
                    h: rh,
                };
                let value = Rect {
                    x: cx + arrow,
                    y,
                    w: control_w - 2.0 * arrow,
                    h: rh,
                };
                (i, row, prev, value, next)
            })
            .collect();
        let button = |x: f32, cols: f32| Rect {
            x,
            y: buttons_y,
            w: cols * cw,
            h: rh,
        };
        let save = button(x0, 8.0);
        let cancel = button(save.x + save.w + cw, 8.0);
        let edit = button(cancel.x + cancel.w + cw, 13.0);
        PageLayout {
            title,
            rows,
            help,
            save,
            cancel,
            edit,
            preview,
            visible,
        }
    }

    /// How many settings rows fit in `area`.
    pub fn settings_visible_rows(&self, area: Rect) -> usize {
        self.page_layout(area, 0, 0).visible
    }

    /// Where the preview terminal goes (its grid should fit it).
    pub fn settings_preview_rect(&self, area: Rect) -> Rect {
        self.page_layout(area, 0, 0).preview
    }

    /// What's under a point on the settings page.
    pub fn settings_hit(
        &self,
        area: Rect,
        rows: &[SettingsRow],
        scroll: usize,
        x: f64,
        y: f64,
    ) -> Option<SettingsHit> {
        let (x, y) = (x as f32, y as f32);
        let l = self.page_layout(area, rows.len(), scroll);
        if l.save.contains(x, y) {
            return Some(SettingsHit::Save);
        }
        if l.cancel.contains(x, y) {
            return Some(SettingsHit::Cancel);
        }
        if l.edit.contains(x, y) {
            return Some(SettingsHit::EditFile);
        }
        for (i, row, prev, value, next) in l.rows {
            if !row.contains(x, y) {
                continue;
            }
            if let Control::Slider(_) = rows[i].control {
                let track = self.slider_track(prev, value, next);
                if x >= track.x - 4.0 && x <= track.x + track.w + 4.0 {
                    let t = ((x - track.x) / track.w).clamp(0.0, 1.0);
                    return Some(SettingsHit::Slider(i, t));
                }
            }
            match rows[i].control {
                Control::Text { presets: false } => return Some(SettingsHit::Row(i)),
                Control::Text { presets: true } if value.contains(x, y) => {
                    return Some(SettingsHit::Row(i))
                }
                _ => {}
            }
            if prev.contains(x, y) {
                return Some(SettingsHit::Prev(i));
            }
            if next.contains(x, y) || value.contains(x, y) {
                return Some(SettingsHit::Next(i));
            }
            return Some(SettingsHit::Row(i));
        }
        None
    }

    /// Where along row `i`'s slider the mouse's `x` falls, for dragging
    /// once the mouse has left the row.
    pub fn settings_slider_at(
        &self,
        area: Rect,
        rows: &[SettingsRow],
        scroll: usize,
        i: usize,
        x: f64,
    ) -> Option<f32> {
        let l = self.page_layout(area, rows.len(), scroll);
        let (_, _, prev, value, next) = l.rows.into_iter().find(|r| r.0 == i)?;
        let track = self.slider_track(prev, value, next);
        Some(((x as f32 - track.x) / track.w).clamp(0.0, 1.0))
    }

    /// A slider's track: the whole control minus room for its value.
    fn slider_track(&self, prev: Rect, _value: Rect, next: Rect) -> Rect {
        let cw = self.fonts.cell_width as f32;
        let x = prev.x + cw * 0.5;
        Rect {
            x,
            y: prev.y + prev.h / 2.0,
            w: (next.x + next.w - 7.0 * cw - x).max(cw),
            h: 0.0,
        }
    }

    fn build_settings(
        &mut self,
        layers: &mut Layers,
        page: &SettingsPage,
        cursor_on: bool,
    ) -> Result<(), AtlasFull> {
        let area = page.area;
        let l = self.page_layout(area, page.rows.len(), page.scroll);
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let fg = self.colors.foreground;
        let bg = self.colors.background;
        let fg_c = self.color(fg, 1.0);
        let dim = self.color(mix(fg, bg, 0.45), 1.0);
        let accent_rgb = self.colors.palette[4];
        let accent = self.color(accent_rgb, 1.0);
        let faint = self.color(fg, 0.07);
        let line = self.line_px();
        let text_dy = |r: Rect| (r.y + (r.h - ch) / 2.0).round();

        let w = self.text(&mut layers.glyphs, l.title, "Settings", fg_c, 20.0 * cw)?;
        self.text(
            &mut layers.glyphs,
            (l.title.0 + w + 2.0 * cw, l.title.1),
            "changes show right away · Save keeps them",
            dim,
            60.0 * cw,
        )?;

        let rr = (5.0 * self.scale as f32).round();
        for (i, row, prev, value, next) in &l.rows {
            let r = &page.rows[*i];
            let selected = *i == page.selected;
            if selected {
                layers.backgrounds.push(rounded(
                    row.x,
                    row.y,
                    row.w,
                    row.h,
                    rr,
                    self.color(self.colors.selection, 0.55),
                ));
                let bar = (line * 3.0).max(3.0);
                layers.decorations.push(rounded(
                    row.x + bar,
                    row.y + row.h * 0.25,
                    bar,
                    row.h * 0.5,
                    bar / 2.0,
                    accent,
                ));
            } else if page.hover.is_some_and(|h| hit_row(h) == Some(*i)) {
                layers
                    .backgrounds
                    .push(rounded(row.x, row.y, row.w, row.h, rr, faint));
            }
            let ty = text_dy(*row);
            self.text(
                &mut layers.glyphs,
                (row.x + cw, ty),
                &r.label,
                if selected { fg_c } else { dim },
                17.0 * cw,
            )?;
            match r.control {
                Control::Choice => {
                    let pill = Rect {
                        x: prev.x,
                        y: row.y + row.h * 0.15,
                        w: next.x + next.w - prev.x,
                        h: row.h * 0.7,
                    };
                    layers
                        .backgrounds
                        .push(rounded(pill.x, pill.y, pill.w, pill.h, rr, faint));
                    let arrow = |h: SettingsHit| {
                        if page.hover == Some(h) {
                            fg_c
                        } else {
                            dim
                        }
                    };
                    self.text(
                        &mut layers.glyphs,
                        (prev.x + cw * 0.75, ty),
                        "‹",
                        arrow(SettingsHit::Prev(*i)),
                        cw * 2.0,
                    )?;
                    self.text(
                        &mut layers.glyphs,
                        (next.x + cw * 0.75, ty),
                        "›",
                        arrow(SettingsHit::Next(*i)),
                        cw * 2.0,
                    )?;
                    let cols = unicode_width::UnicodeWidthStr::width(r.value.as_str()) as f32;
                    let x = value.x + ((value.w - cols * cw) / 2.0).max(0.0);
                    self.text(&mut layers.glyphs, (x.round(), ty), &r.value, fg_c, value.w)?;
                }
                Control::Slider(t) => {
                    let track = self.slider_track(*prev, *value, *next);
                    let th = (line * 2.0).max(2.0);
                    let ty_track = (track.y - th / 2.0).round();
                    layers.backgrounds.push(rounded(
                        track.x,
                        ty_track,
                        track.w,
                        th,
                        th / 2.0,
                        self.color(fg, 0.2),
                    ));
                    let filled = (track.w * t).round();
                    layers.backgrounds.push(rounded(
                        track.x,
                        ty_track,
                        filled,
                        th,
                        th / 2.0,
                        accent,
                    ));
                    let knob = (ch * 0.7).round();
                    layers.decorations.push(rounded(
                        (track.x + filled - knob / 2.0).round(),
                        (track.y - knob / 2.0).round(),
                        knob,
                        knob,
                        knob / 2.0,
                        fg_c,
                    ));
                    self.text(
                        &mut layers.glyphs,
                        (track.x + track.w + 1.5 * cw, ty),
                        &r.value,
                        fg_c,
                        6.0 * cw,
                    )?;
                }
                Control::Text { presets } => {
                    let (x0, x1) = if presets {
                        (value.x, value.x + value.w)
                    } else {
                        (prev.x, next.x + next.w)
                    };
                    let bh = (row.h * 0.7).round();
                    let by = (row.y + (row.h - bh) / 2.0).round();
                    layers
                        .backgrounds
                        .push(rounded(x0, by, x1 - x0, bh, rr, faint));
                    // An underline marks it as a box to type in.
                    layers.decorations.push(Instance::rect(
                        x0,
                        by + bh - line.max(1.0),
                        x1 - x0,
                        line.max(1.0),
                        if selected {
                            accent
                        } else {
                            self.color(fg, 0.3)
                        },
                    ));
                    if presets {
                        let arrow = |h: SettingsHit| {
                            if page.hover == Some(h) {
                                fg_c
                            } else {
                                dim
                            }
                        };
                        self.text(
                            &mut layers.glyphs,
                            (prev.x + cw * 0.75, ty),
                            "‹",
                            arrow(SettingsHit::Prev(*i)),
                            cw * 2.0,
                        )?;
                        self.text(
                            &mut layers.glyphs,
                            (next.x + cw * 0.75, ty),
                            "›",
                            arrow(SettingsHit::Next(*i)),
                            cw * 2.0,
                        )?;
                    }
                    let tx = x0 + cw * 0.5;
                    let w = if r.value.is_empty() {
                        if !selected {
                            self.text(&mut layers.glyphs, (tx, ty), "none", dim, x1 - tx)?;
                        }
                        0.0
                    } else {
                        self.text(&mut layers.glyphs, (tx, ty), &r.value, fg_c, x1 - tx)?
                    };
                    if selected && cursor_on {
                        layers.decorations.push(Instance::rect(
                            (tx + w + 1.0).round(),
                            by + bh * 0.15,
                            (line * 2.0).max(2.0),
                            bh * 0.7,
                            fg_c,
                        ));
                    }
                }
                Control::Toggle(on) => {
                    let pw = 3.5 * cw;
                    let ph = (ch * 0.75).round();
                    let px = prev.x;
                    let py = (row.y + (row.h - ph) / 2.0).round();
                    let track = if on { accent } else { self.color(fg, 0.25) };
                    layers
                        .backgrounds
                        .push(rounded(px, py, pw, ph, ph / 2.0, track));
                    let k = ph - 2.0 * line.max(2.0);
                    let kx = if on {
                        px + pw - k - line.max(2.0)
                    } else {
                        px + line.max(2.0)
                    };
                    layers
                        .decorations
                        .push(rounded(kx, py + line.max(2.0), k, k, k / 2.0, fg_c));
                    self.text(
                        &mut layers.glyphs,
                        (px + pw + cw, ty),
                        &r.value,
                        fg_c,
                        6.0 * cw,
                    )?;
                }
            }
        }
        // More rows than fit: a hint at the scroll.
        if page.scroll > 0 || page.scroll + l.visible < page.rows.len() {
            let more = format!(
                "{}–{} of {}  (scroll or ↑↓)",
                page.scroll + 1,
                (page.scroll + l.visible).min(page.rows.len()),
                page.rows.len()
            );
            let y = l.rows.last().map_or(l.help.1, |r| r.1.y + r.1.h);
            self.text(&mut layers.glyphs, (l.help.0, y), &more, dim, 40.0 * cw)?;
        }

        self.text(
            &mut layers.glyphs,
            l.help,
            page.help,
            dim,
            area.w - 4.0 * cw,
        )?;

        for (r, label, hit, primary) in [
            (l.save, "Save", SettingsHit::Save, page.dirty),
            (l.cancel, "Cancel", SettingsHit::Cancel, false),
            (l.edit, "Edit file…", SettingsHit::EditFile, false),
        ] {
            let hovered = page.hover == Some(hit);
            let fill = match (primary, hovered) {
                (true, true) => self.color(mix(accent_rgb, fg, 0.2), 1.0),
                (true, false) => accent,
                (false, true) => self.color(fg, 0.14),
                (false, false) => faint,
            };
            layers.backgrounds.push(rounded(
                r.x,
                r.y,
                r.w,
                r.h,
                (6.0 * self.scale as f32).round(),
                fill,
            ));
            let text = if primary { self.color(bg, 1.0) } else { fg_c };
            let cols = label.chars().count() as f32;
            self.text(
                &mut layers.glyphs,
                ((r.x + (r.w - cols * cw) / 2.0).round(), text_dy(r)),
                label,
                text,
                r.w,
            )?;
        }

        // The preview: a sample terminal in the edited settings.
        let p = l.preview;
        self.text(
            &mut layers.glyphs,
            (p.x, p.y - ch * 1.4),
            "Preview",
            dim,
            20.0 * cw,
        )?;
        let border = self.color(self.colors.palette[8], 1.0);
        let d = &mut layers.decorations;
        d.push(Instance::rect(p.x, p.y, p.w, line, border));
        d.push(Instance::rect(p.x, p.y + p.h - line, p.w, line, border));
        d.push(Instance::rect(p.x, p.y, line, p.h, border));
        d.push(Instance::rect(p.x + p.w - line, p.y, line, p.h, border));
        let view = PaneView {
            grid: page.preview,
            rect: p,
            focused: true,
            matches: &[],
            current_match: None,
            link: None,
            hints: &[],
        };
        self.build_pane(layers, &view, cursor_on)
    }

    /// The command palette: a floating card near the top with a filter box
    /// and the matching commands, the selected one lit.
    fn build_palette(&mut self, layers: &mut Layers, view: &PaletteView) -> Result<(), AtlasFull> {
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let scale = self.scale as f32;
        let full = self.full_rect();
        // Fade what's behind so the card stands out.
        let shade = self.color((0, 0, 0), 0.3);
        layers.overlays.push(rect(full, shade));

        let row_h = (ch * 1.5).round();
        let pad = (cw * 1.2).round();
        let shown = view.items.len().min(PALETTE_ROWS);
        let w = (64.0 * cw).min(full.w - 4.0 * cw).max(20.0 * cw);
        let h = row_h * (shown as f32 + 1.0) + pad * 1.5 + if shown == 0 { 0.0 } else { pad * 0.5 };
        let x = ((full.w - w) / 2.0).round();
        let y = (self.tab_bar_height() as f32 + 2.0 * ch)
            .min(full.h / 4.0)
            .round();
        let bg = self.colors.background;
        let card = if luma(bg) < 128.0 {
            mix(bg, (255, 255, 255), 0.08)
        } else {
            mix(bg, (255, 255, 255), 0.6)
        };
        let accent = self.colors.palette[12];
        let radius = (10.0 * scale).round();
        let o = &mut layers.overlays;
        // A soft shadow: a few widening, fading rounded rects.
        for i in (1..=4).rev() {
            let g = i as f32 * 3.0 * scale;
            o.push(rounded(
                x - g,
                y - g + 4.0 * scale,
                w + 2.0 * g,
                h + 2.0 * g,
                radius + g,
                self.color((0, 0, 0), 0.07),
            ));
        }
        let border = mix(card, self.colors.foreground, 0.15);
        let line = self.line_px();
        o.push(rounded(
            x - line,
            y - line,
            w + 2.0 * line,
            h + 2.0 * line,
            radius + line,
            self.color(border, 1.0),
        ));
        o.push(rounded(x, y, w, h, radius, self.color(card, 1.0)));

        // The filter box.
        let fg = self.color(self.colors.foreground, 1.0);
        let dim_rgb = mix(self.colors.foreground, card, 0.45);
        let dim = self.color(dim_rgb, 1.0);
        let field = mix(card, bg, 0.6);
        let (fx, fy, fw) = (x + pad * 0.5, y + pad * 0.5, w - pad);
        o.push(rounded(
            fx,
            fy,
            fw,
            row_h,
            (6.0 * scale).round(),
            self.color(field, 1.0),
        ));
        let ty = |row_y: f32| (row_y + (row_h - ch) / 2.0).round();
        let mut glyphs = Vec::new();
        let tx = fx + pad * 0.5;
        let used = self.text(&mut glyphs, (tx, ty(fy)), "› ", self.color(accent, 1.0), fw)?;
        if view.query.is_empty() {
            self.text(
                &mut glyphs,
                (tx + used, ty(fy)),
                view.placeholder,
                dim,
                fw - used - pad,
            )?;
            glyphs.push(Instance::rect(
                tx + used,
                ty(fy),
                (line * 2.0).max(2.0),
                ch,
                fg,
            ));
        } else {
            let qw = self.text(
                &mut glyphs,
                (tx + used, ty(fy)),
                view.query,
                fg,
                fw - used - pad,
            )?;
            glyphs.push(Instance::rect(
                tx + used + qw,
                ty(fy),
                (line * 2.0).max(2.0),
                ch,
                fg,
            ));
        }

        // The list, scrolled so the selection stays in view.
        let first = view.selected.saturating_sub(shown.saturating_sub(1));
        let list_y = fy + row_h + pad * 0.5;
        if view.items.is_empty() {
            self.text(
                &mut glyphs,
                (tx, ty(list_y)),
                "No matching commands",
                dim,
                fw,
            )?;
        }
        for (k, (label, hint)) in view.items.iter().enumerate().skip(first).take(shown) {
            let ry = list_y + (k - first) as f32 * row_h;
            let selected = k == view.selected;
            if selected {
                let lit = mix(card, accent, 0.35);
                layers.overlays.push(rounded(
                    fx,
                    ry,
                    fw,
                    row_h,
                    (6.0 * scale).round(),
                    self.color(lit, 1.0),
                ));
                layers.overlays.push(rounded(
                    fx,
                    ry + row_h * 0.2,
                    3.0 * scale,
                    row_h * 0.6,
                    1.5 * scale,
                    self.color(accent, 1.0),
                ));
            }
            let hint_w = unicode_width::UnicodeWidthStr::width(*hint) as f32 * cw;
            let label_room = fw - pad * 1.5 - hint_w - cw;
            self.text(&mut glyphs, (tx, ty(ry)), label, fg, label_room)?;
            if !hint.is_empty() {
                let hx = fx + fw - pad * 0.5 - hint_w;
                self.text(&mut glyphs, (hx, ty(ry)), hint, dim, hint_w + 1.0)?;
            }
        }
        if view.items.len() > shown {
            let more = format!("{}/{}", view.selected + 1, view.items.len());
            let mw = more.chars().count() as f32 * cw;
            self.text(
                &mut glyphs,
                (fx + fw - pad * 0.5 - mw, ty(fy)),
                &more,
                dim,
                mw + 1.0,
            )?;
        }
        layers.overlays.append(&mut glyphs);
        Ok(())
    }

    fn build_search_bar(
        &mut self,
        layers: &mut Layers,
        search: &SearchBar,
        pane: Rect,
    ) -> Result<(), AtlasFull> {
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let count = match (search.total, search.current) {
            (0, _) if search.query.is_empty() => String::new(),
            (0, _) => "no matches".to_string(),
            (n, Some(i)) => format!("{}/{n}", i + 1),
            (n, None) => format!("{n}"),
        };
        let label = "Find: ";
        let cols = 28.0f32.min((pane.w / cw - 4.0).max(10.0));
        let w = cols * cw + 2.0 * cw;
        let h = ch * 1.6;
        let x = (pane.x + pane.w - w - cw).max(pane.x);
        let y = pane.y + cw;
        let bg = self.colors.background;
        let box_bg = if luma(bg) < 128.0 {
            mix(bg, (255, 255, 255), 0.1)
        } else {
            mix(bg, (0, 0, 0), 0.06)
        };
        let border = self.color(self.colors.palette[12], 1.0);
        let line = self.line_px();
        let o = &mut layers.overlays;
        let r = (6.0 * self.scale as f32).round();
        o.push(rounded(
            x,
            y + 2.0 * line,
            w,
            h,
            r,
            self.color((0, 0, 0), 0.25),
        ));
        o.push(rounded(
            x - line,
            y - line,
            w + 2.0 * line,
            h + 2.0 * line,
            r + line,
            border,
        ));
        o.push(rounded(x, y, w, h, r, self.color(box_bg, 1.0)));

        // Overlay glyphs must land after the overlay boxes.
        let mut glyphs = Vec::new();
        let ty = (y + (h - ch) / 2.0).round();
        let fg = self.color(self.colors.foreground, 1.0);
        let dim = self.color(mix(self.colors.foreground, box_bg, 0.45), 1.0);
        let tx = x + cw;
        let used = self.text(&mut glyphs, (tx, ty), label, dim, w)?;
        let count_w = count.chars().count() as f32 * cw;
        let room = w - 2.0 * cw - used - count_w - cw;
        // Show the end of a long query, where the typing happens.
        let q: String = {
            let max = (room / cw).floor().max(1.0) as usize;
            let chars: Vec<char> = search.query.chars().collect();
            chars[chars.len().saturating_sub(max)..].iter().collect()
        };
        let qw = self.text(&mut glyphs, (tx + used, ty), &q, fg, room)?;
        // Text caret.
        glyphs.push(Instance::rect(
            tx + used + qw,
            ty,
            (line * 2.0).max(2.0),
            ch,
            fg,
        ));
        self.text(
            &mut glyphs,
            (x + w - cw - count_w, ty),
            &count,
            dim,
            count_w + 1.0,
        )?;
        layers.overlays.append(&mut glyphs);
        Ok(())
    }

    /// Draw a frame. Returns false when it was skipped (surface lost or
    /// outdated); the caller should request another redraw.
    pub fn render(&mut self, frame: &Frame) -> bool {
        let (mut instances, backgrounds, images) = match self.build_instances(frame) {
            Ok(i) => i,
            Err(AtlasFull) => {
                // Start the atlas over with only this frame's glyphs.
                self.glyphs.clear();
                self.atlas = Atlas::new();
                match self.build_instances(frame) {
                    Ok(i) => i,
                    Err(AtlasFull) => return true, // more glyphs on screen than fit; give up
                }
            }
        };

        let surface_frame = match &self.target {
            Target::Window { surface, config } => match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => Some(f),
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                    surface.configure(&self.device, config);
                    Some(f)
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return false
                }
                _ => {
                    surface.configure(&self.device, config);
                    return false;
                }
            },
            Target::Offscreen { .. } => None,
        };
        let view = match (&surface_frame, &self.target) {
            (Some(f), _) => f
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
            (None, Target::Offscreen { texture }) => {
                texture.create_view(&wgpu::TextureViewDescriptor::default())
            }
            (None, Target::Window { .. }) => unreachable!(),
        };

        // Pictures' quads go after everything else in the buffer; each is
        // drawn on its own with its texture bound.
        self.frame_count += 1;
        let main = instances.len();
        let mut drawn = Vec::new();
        for d in &images {
            if self.upload_image(&d.image) {
                drawn.push((d.image.uid, d.clip, d.under));
                instances.push(d.quad);
            }
        }
        if instances.len() > self.instance_capacity {
            self.instance_capacity = instances.len().next_power_of_two();
            self.instances = Self::instance_buffer(&self.device, self.instance_capacity);
        }
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }
        let uniforms = [
            self.width as f32,
            self.height as f32,
            ATLAS_SIZE as f32,
            ATLAS_SIZE as f32,
        ];
        self.queue
            .write_buffer(&self.uniforms, 0, bytemuck::cast_slice(&uniforms));

        let bg = self.color(self.colors.background, 1.0);
        let a = self.see_through() as f64;
        // Premultiplied surfaces want the color scaled by its alpha.
        let k = if self.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied {
            a
        } else {
            1.0
        };
        let clear = wgpu::Color {
            r: bg[0] as f64 * k,
            g: bg[1] as f64 * k,
            b: bg[2] as f64 * k,
            a,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tacoterm"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                // The background picture, then cell backgrounds, then
                // pictures in the text, then the rest.
                for layer in [true, false] {
                    let mut any = false;
                    for (k, (uid, clip, under)) in drawn.iter().enumerate() {
                        if *under != layer {
                            continue;
                        }
                        let Some(gpu) = self.images.get(uid) else {
                            continue;
                        };
                        let Some((x, y, w, h)) = self.scissor(*clip) else {
                            continue;
                        };
                        pass.set_scissor_rect(x, y, w, h);
                        pass.set_bind_group(0, &gpu.bind_group, &[]);
                        let i = (main + k) as u32;
                        pass.draw(0..4, i..i + 1);
                        any = true;
                    }
                    if any {
                        pass.set_scissor_rect(0, 0, self.width, self.height);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                    }
                    if layer {
                        pass.draw(0..4, 0..backgrounds as u32);
                    }
                }
                pass.draw(0..4, backgrounds as u32..main as u32);
            }
        }
        self.queue.submit([encoder.finish()]);
        // Let go of textures for pictures no longer on screen.
        let now = self.frame_count;
        self.images.retain(|_, g| g.used + 300 > now);
        if let Some(f) = surface_frame {
            self.queue.present(f);
        }
        true
    }

    /// Make sure `image` has a GPU texture, and point its uniforms at this
    /// frame. False if it can't be shown.
    fn upload_image(&mut self, image: &Arc<Image>) -> bool {
        let screen = [self.width as f32, self.height as f32];
        if let Some(g) = self.images.get_mut(&image.uid) {
            g.used = self.frame_count;
            let u = [screen[0], screen[1], g.size.0 as f32, g.size.1 as f32];
            self.queue
                .write_buffer(&g.uniforms, 0, bytemuck::cast_slice(&u));
            return true;
        }
        if image.width == 0 || image.height == 0 {
            return false;
        }
        // Bigger than the GPU takes: shrink to fit. uv stays in the
        // original's pixels, so the uniforms carry the original size.
        let max = self.device.limits().max_texture_dimension_2d;
        let shrunk;
        let (w, h, pixels) = if image.width > max || image.height > max {
            let s = max as f32 / image.width.max(image.height) as f32;
            let (w, h) = (
                ((image.width as f32 * s) as u32).max(1),
                ((image.height as f32 * s) as u32).max(1),
            );
            let Some(src) =
                image::RgbaImage::from_raw(image.width, image.height, image.rgba.clone())
            else {
                return false;
            };
            shrunk = image::imageops::resize(&src, w, h, image::imageops::FilterType::Triangle)
                .into_raw();
            (w, h, &shrunk[..])
        } else {
            (image.width, image.height, &image.rgba[..])
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("picture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uniforms = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("picture uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let size = (image.width, image.height);
        let u = [screen[0], screen[1], size.0 as f32, size.1 as f32];
        self.queue
            .write_buffer(&uniforms, 0, bytemuck::cast_slice(&u));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("picture"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.image_sampler),
                },
            ],
        });
        self.images.insert(
            image.uid,
            GpuImage {
                bind_group,
                uniforms,
                size,
                used: self.frame_count,
            },
        );
        true
    }

    /// `r` as a scissor rectangle inside the target, if any of it is.
    fn scissor(&self, r: Rect) -> Option<(u32, u32, u32, u32)> {
        let x0 = r.x.max(0.0).floor() as u32;
        let y0 = r.y.max(0.0).floor() as u32;
        let x1 = ((r.x + r.w).ceil().max(0.0) as u32).min(self.width);
        let y1 = ((r.y + r.h).ceil().max(0.0) as u32).min(self.height);
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
    }

    /// Read back the offscreen target as tightly packed RGBA8 rows.
    /// Only meaningful for a [`Renderer::headless`] renderer.
    pub fn read_pixels(&self) -> Option<Vec<u8>> {
        let Target::Offscreen { texture } = &self.target else {
            return None;
        };
        let unpadded = self.width * 4;
        let padded = unpadded.div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * self.height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok()?;
        let data = slice.get_mapped_range().ok()?;
        let mut out = Vec::with_capacity((unpadded * self.height) as usize);
        for row in data.chunks(padded as usize) {
            out.extend_from_slice(&row[..unpadded as usize]);
        }
        Some(out)
    }
}

fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("tacoterm"),
        required_limits:
            wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    }))
    .map_err(|e| format!("cannot open GPU device: {e}"))
}
