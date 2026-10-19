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

    fn rasterize(&mut self, c: char, bold: bool, italic: bool) -> Option<RasterGlyph> {
        let mut tmp = [0u8; 4];
        let buf = self.shape(c.encode_utf8(&mut tmp), bold, italic);
        let run = buf.layout_runs().next()?;
        let glyph = run.glyphs.first()?;
        let physical = glyph.physical((0.0, 0.0), 1.0);
        let image = self
            .swash
            .get_image_uncached(&mut self.system, physical.cache_key)?;
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None;
        }
        let pixels = (w * h) as usize;
        let (rgba, is_color) = match image.content {
            SwashContent::Mask => {
                let mut rgba = Vec::with_capacity(pixels * 4);
                for &a in image.data.iter().take(pixels) {
                    rgba.extend_from_slice(&[255, 255, 255, a]);
                }
                (rgba, false)
            }
            SwashContent::SubpixelMask => {
                let mut rgba = Vec::with_capacity(pixels * 4);
                for px in image.data.as_chunks::<4>().0.iter().take(pixels) {
                    let a = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
                    rgba.extend_from_slice(&[255, 255, 255, a]);
                }
                (rgba, false)
            }
            SwashContent::Color => (image.data.clone(), true),
        };
        if rgba.len() < pixels * 4 {
            return None;
        }
        Some(RasterGlyph {
            offset: (
                physical.x + image.placement.left,
                (run.line_y.round() as i32) + physical.y - image.placement.top,
            ),
            width: w,
            height: h,
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
/// 2 = color glyph (emoji) drawn as-is.
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

fn rect(r: Rect, color: [f32; 4]) -> Instance {
    Instance::rect(r.x, r.y, r.w, r.h, color)
}

/// Quads by draw order.
#[derive(Default)]
struct Layers {
    backgrounds: Vec<Instance>,
    glyphs: Vec<Instance>,
    decorations: Vec<Instance>,
    overlays: Vec<Instance>,
}

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
            }],
            dividers: Vec::new(),
            tabs: Vec::new(),
            active_tab: 0,
            search: None,
            cursor_on: true,
            flash: false,
            settings: None,
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
            self.fonts.rasterize(key.c, key.bold, key.italic)
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
    fn build_instances(&mut self, frame: &Frame) -> Result<Vec<Instance>, AtlasFull> {
        let mut layers = Layers::default();
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
        if frame.flash {
            let flash = self.color(self.colors.foreground, 0.12);
            layers.overlays.push(rect(self.full_rect(), flash));
        }
        let mut out = layers.backgrounds;
        out.append(&mut layers.glyphs);
        out.append(&mut layers.decorations);
        out.append(&mut layers.overlays);
        Ok(out)
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
                let alpha = if cell.attr.has(Attr::DIM) { 0.6 } else { 1.0 };
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
                if cell.attr.has(Attr::UNDERLINE) || in_link {
                    let uy = (py + self.fonts.baseline + line_px).min(py + ch - line_px);
                    layers
                        .decorations
                        .push(Instance::rect(px, uy, cw * span, line_px, fg_color));
                }
                if cell.attr.has(Attr::STRIKE) {
                    let sy = py + (self.fonts.baseline * 0.65).round();
                    layers
                        .decorations
                        .push(Instance::rect(px, sy, cw * span, line_px, fg_color));
                }
            }
        }

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
        let text_y = ((h - ch as f32) / 2.0).round();
        let cw = self.fonts.cell_width as f32;
        // The strip, minus the active tab, which shows the pane background
        // so it reads as attached to the terminal below. With the settings
        // page up no tab is "attached".
        let active = if frame.settings.is_some() {
            None
        } else {
            strip.tabs.get(frame.active_tab).map(|t| t.0)
        };
        let bar_color = self.color(bar, alpha);
        let full = Rect {
            x: 0.0,
            y: 0.0,
            w: self.width as f32,
            h,
        };
        match active {
            Some(a) => {
                layers
                    .backgrounds
                    .push(Instance::rect(0.0, 0.0, a.x, h, bar_color));
                let right = a.x + a.w;
                layers.backgrounds.push(Instance::rect(
                    right,
                    0.0,
                    self.width as f32 - right,
                    h,
                    bar_color,
                ));
            }
            None => layers.backgrounds.push(rect(full, bar_color)),
        }
        let fg = self.colors.foreground;
        let dim = mix(fg, bar, 0.45);
        let accent = self.color(self.colors.palette[4], 1.0);
        let hover_bg = self.color(fg, 0.1);
        let hover = self.hover;
        for (i, (title, (tab, close))) in frame.tabs.iter().zip(&strip.tabs).enumerate() {
            let is_active = Some(*tab) == active;
            let color = self.color(if is_active { fg } else { dim }, 1.0);
            if is_active {
                let line = (2.0 * self.scale as f32).round();
                layers
                    .decorations
                    .push(Instance::rect(tab.x, 0.0, tab.w, line, accent));
            } else {
                if hover == Some(TabHit::Tab(i)) || hover == Some(TabHit::Close(i)) {
                    layers.backgrounds.push(rect(*tab, hover_bg));
                }
                if i + 1 != frame.active_tab && i + 1 < strip.tabs.len() {
                    // Separator between two inactive tabs.
                    let sep = self.color(dim, 0.5);
                    layers.decorations.push(Instance::rect(
                        tab.x + tab.w - 1.0,
                        h * 0.25,
                        1.0,
                        h * 0.5,
                        sep,
                    ));
                }
            }
            let max_w = close.x - tab.x - cw * 1.5;
            self.text(
                &mut layers.glyphs,
                (tab.x + cw, text_y),
                title,
                color,
                max_w,
            )?;
            if strip.tabs.len() > 1 {
                if hover == Some(TabHit::Close(i)) {
                    layers.backgrounds.push(rect(*close, hover_bg));
                }
                self.cross(layers, *close, (close.w * 0.5).round(), color);
            }
        }
        let c = self.color(dim, 1.0);
        let t = self.line_px().max(1.0) + (self.scale as f32 - 1.0).max(0.0);
        // "+" for a new tab.
        if hover == Some(TabHit::New) {
            layers.backgrounds.push(rect(strip.new, hover_bg));
        }
        let arm = (h * 0.3).round();
        let (cx, cy) = (strip.new.x + strip.new.w / 2.0, h / 2.0);
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
            layers.backgrounds.push(rect(s, hover_bg));
        }
        let gear_color = self.color(if frame.settings.is_some() { fg } else { dim }, 1.0);
        self.text(
            &mut layers.glyphs,
            ((s.x + (s.w - cw) / 2.0).round(), text_y),
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

        for (i, row, prev, value, next) in &l.rows {
            let r = &page.rows[*i];
            let selected = *i == page.selected;
            if selected {
                layers
                    .backgrounds
                    .push(rect(*row, self.color(self.colors.selection, 0.55)));
                layers.decorations.push(Instance::rect(
                    row.x,
                    row.y,
                    (line * 2.0).max(2.0),
                    row.h,
                    accent,
                ));
            } else if page.hover.is_some_and(|h| hit_row(h) == Some(*i)) {
                layers.backgrounds.push(rect(*row, faint));
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
                    layers.backgrounds.push(rect(pill, faint));
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
                    layers.backgrounds.push(Instance::rect(
                        track.x,
                        ty_track,
                        track.w,
                        th,
                        self.color(fg, 0.2),
                    ));
                    let filled = (track.w * t).round();
                    layers
                        .backgrounds
                        .push(Instance::rect(track.x, ty_track, filled, th, accent));
                    let knob = (ch * 0.7).round();
                    layers.decorations.push(Instance::rect(
                        (track.x + filled - knob / 2.0).round(),
                        (track.y - knob / 2.0).round(),
                        knob,
                        knob,
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
                        .push(Instance::rect(x0, by, x1 - x0, bh, faint));
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
                        .push(Instance::rect(px, py, pw, ph, track));
                    let k = ph - 2.0 * line.max(2.0);
                    let kx = if on {
                        px + pw - k - line.max(2.0)
                    } else {
                        px + line.max(2.0)
                    };
                    layers
                        .decorations
                        .push(Instance::rect(kx, py + line.max(2.0), k, k, fg_c));
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
            layers.backgrounds.push(rect(r, fill));
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
        };
        self.build_pane(layers, &view, cursor_on)
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
        let border = self.color(self.colors.palette[4], 1.0);
        let line = self.line_px();
        let o = &mut layers.overlays;
        o.push(Instance::rect(x, y, w, h, self.color(box_bg, 1.0)));
        o.push(Instance::rect(x, y, w, line, border));
        o.push(Instance::rect(x, y + h - line, w, line, border));
        o.push(Instance::rect(x, y, line, h, border));
        o.push(Instance::rect(x + w - line, y, line, h, border));

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
        let instances = match self.build_instances(frame) {
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
                pass.draw(0..4, 0..instances.len() as u32);
            }
        }
        self.queue.submit([encoder.finish()]);
        if let Some(f) = surface_frame {
            self.queue.present(f);
        }
        true
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
