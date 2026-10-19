//! GPU text renderer: a wgpu surface plus a glyph atlas fed by cosmic-text.
//!
//! Each frame walks the visible grid and emits one instanced quad per
//! background run, glyph and decoration, then draws them in a single call.
//! Glyphs are shaped and rasterized once by cosmic-text (which also does
//! font fallback for CJK, emoji, symbols…) and cached in an RGBA atlas
//! texture. Frames are only drawn when something changed; the app is
//! event-driven, so an idle terminal costs nothing.
//!
//! TODO: per-row damage tracking (rebuild only changed rows' instances) and
//! built-in box-drawing glyphs that tile seamlessly.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};
use winit::window::Window;

use crate::config::{parse_hex, Config};
use crate::grid::{Attr, Color, Grid};

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
                for px in image.data.chunks_exact(4).take(pixels) {
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

type Rgb = (u8, u8, u8);

struct Colors {
    background: Rgb,
    foreground: Rgb,
    cursor: Rgb,
    selection: Rgb,
    palette: [Rgb; 256],
}

impl Colors {
    fn from_config(config: &Config) -> Colors {
        let defaults = crate::config::Theme::default();
        let theme = &config.theme;
        let pick = |s: &str, d: &str| parse_hex(s).or_else(|| parse_hex(d)).unwrap_or((0, 0, 0));
        let mut palette = [(0, 0, 0); 256];
        for (i, slot) in palette.iter_mut().enumerate().take(16) {
            let s = theme.palette.get(i).map(String::as_str).unwrap_or("");
            *slot = pick(s, &defaults.palette[i]);
        }
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
            background: pick(&theme.background, &defaults.background),
            foreground: pick(&theme.foreground, &defaults.foreground),
            cursor: pick(&theme.cursor, &defaults.cursor),
            selection: pick(&theme.selection, &defaults.selection),
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

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
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
    /// Hollow cursor when unfocused, block when focused.
    pub focused: bool,
}

impl Renderer {
    /// Create a renderer drawing into `window`.
    pub fn new(window: Arc<Window>, fonts: Fonts, config: &Config) -> Result<Renderer, String> {
        let size = window.inner_size();
        let scale = window.scale_factor();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("cannot create GPU surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or("no compatible GPU adapter found")?;
        let (device, queue) = request_device(&adapter)?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or("GPU surface supports no formats")?;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);
        let target = Target::Window {
            surface,
            config: surface_config,
        };
        Ok(Renderer::build(
            device,
            queue,
            target,
            format,
            (size.width.max(1), size.height.max(1)),
            fonts,
            config,
            (config.padding as f64 * scale).round() as u32,
        ))
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
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or("no GPU adapter found")?;
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
            config.padding,
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
        padding: u32,
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
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tacoterm"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
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
                }],
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
            multiview: None,
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
            colors: Colors::from_config(config),
            padding,
            focused: true,
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
        self.fonts = Fonts::new(&self.font_config, scale);
        self.padding = (self.font_config.padding as f64 * scale).round() as u32;
        self.glyphs.clear();
        self.atlas = Atlas::new();
    }

    /// (cols, rows) that fit the surface at the current cell metrics.
    /// The app uses this to resize the grid and ConPTY together.
    pub fn grid_size(&self) -> (u16, u16) {
        let (cw, ch) = self.cell_size();
        let w = self.width.saturating_sub(2 * self.padding);
        let h = self.height.saturating_sub(2 * self.padding);
        (
            (w / cw).clamp(1, u16::MAX as u32) as u16,
            (h / ch).clamp(1, u16::MAX as u32) as u16,
        )
    }

    /// The viewport cell under a pixel position (clamped to the grid).
    pub fn cell_at(&self, x: f64, y: f64, grid: &Grid) -> (usize, usize) {
        let (cw, ch) = self.cell_size();
        let p = self.padding as f64;
        let col = ((x - p) / cw as f64).floor().max(0.0) as usize;
        let row = ((y - p) / ch as f64).floor().max(0.0) as usize;
        (col.min(grid.cols - 1), row.min(grid.rows - 1))
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

    fn glyph(&mut self, key: GlyphKey) -> Result<Option<CachedGlyph>, AtlasFull> {
        if let Some(g) = self.glyphs.get(&key) {
            return Ok(*g);
        }
        let Some(raster) = self.fonts.rasterize(key.c, key.bold, key.italic) else {
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

    /// Build this frame's quads: backgrounds, then glyphs, then decorations
    /// and the cursor, so nothing paints over a neighbor's glyph.
    fn build_instances(&mut self, grid: &Grid) -> Result<Vec<Instance>, AtlasFull> {
        let (cw, ch) = self.cell_size();
        let (cw, ch) = (cw as f32, ch as f32);
        let pad = self.padding as f32;
        let mut backgrounds = Vec::new();
        let mut glyphs = Vec::new();
        let mut decorations = Vec::new();
        let rect = |x: f32, y: f32, w: f32, h: f32, color: [f32; 4]| Instance {
            pos: [x, y],
            size: [w, h],
            uv_pos: [0.0; 2],
            uv_size: [0.0; 2],
            color,
            kind: 0,
        };

        let show_cursor = grid.display_offset == 0 && grid.modes.cursor_visible;
        let line_px = (self.fonts.size / 14.0).round().max(1.0);
        for y in 0..grid.rows {
            let row = grid.visible_row(y);
            let py = pad + y as f32 * ch;
            for x in 0..grid.cols {
                let cell = row.cells.get(x).copied().unwrap_or_default();
                if cell.attr.has(Attr::WIDE_SPACER) {
                    continue;
                }
                let span = if cell.attr.has(Attr::WIDE) { 2.0 } else { 1.0 };
                let px = pad + x as f32 * cw;
                let bold = cell.attr.has(Attr::BOLD);
                let mut fg = self.colors.resolve(cell.fg, true, bold);
                let mut bg = self.colors.resolve(cell.bg, false, false);
                if cell.attr.has(Attr::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if grid.is_selected(x, y) {
                    bg = self.colors.selection;
                }
                let cursor_here = show_cursor && x == grid.cursor_x && y == grid.cursor_y;
                if cursor_here && self.focused {
                    bg = self.colors.cursor;
                    fg = self.colors.background;
                }
                if bg != self.colors.background {
                    backgrounds.push(rect(px, py, cw * span, ch, self.color(bg, 1.0)));
                }
                if cursor_here && !self.focused {
                    let c = self.color(self.colors.cursor, 1.0);
                    let (w, h) = (cw * span, ch);
                    decorations.push(rect(px, py, w, line_px, c));
                    decorations.push(rect(px, py + h - line_px, w, line_px, c));
                    decorations.push(rect(px, py, line_px, h, c));
                    decorations.push(rect(px + w - line_px, py, line_px, h, c));
                }
                let alpha = if cell.attr.has(Attr::DIM) { 0.6 } else { 1.0 };
                let fg_color = self.color(fg, alpha);
                if cell.c != ' ' && !cell.attr.has(Attr::HIDDEN) {
                    let key = GlyphKey {
                        c: cell.c,
                        bold,
                        italic: cell.attr.has(Attr::ITALIC),
                    };
                    if let Some(g) = self.glyph(key)? {
                        glyphs.push(Instance {
                            pos: [px + g.offset.0 as f32, py + g.offset.1 as f32],
                            size: [g.uv[2], g.uv[3]],
                            uv_pos: [g.uv[0], g.uv[1]],
                            uv_size: [g.uv[2], g.uv[3]],
                            color: fg_color,
                            kind: if g.is_color { 2 } else { 1 },
                        });
                    }
                }
                if cell.attr.has(Attr::UNDERLINE) {
                    let uy = (py + self.fonts.baseline + line_px).min(py + ch - line_px);
                    decorations.push(rect(px, uy, cw * span, line_px, fg_color));
                }
                if cell.attr.has(Attr::STRIKE) {
                    let sy = py + (self.fonts.baseline * 0.65).round();
                    decorations.push(rect(px, sy, cw * span, line_px, fg_color));
                }
            }
        }
        backgrounds.append(&mut glyphs);
        backgrounds.append(&mut decorations);
        Ok(backgrounds)
    }

    /// Draw the grid. Returns false when the frame was skipped (surface
    /// lost or outdated); the caller should request another redraw.
    pub fn render(&mut self, grid: &Grid) -> bool {
        let instances = match self.build_instances(grid) {
            Ok(i) => i,
            Err(AtlasFull) => {
                // Start the atlas over with only this frame's glyphs.
                self.glyphs.clear();
                self.atlas = Atlas::new();
                match self.build_instances(grid) {
                    Ok(i) => i,
                    Err(AtlasFull) => return true, // more glyphs on screen than fit; give up
                }
            }
        };

        let frame = match &self.target {
            Target::Window { surface, config } => match surface.get_current_texture() {
                Ok(f) => Some(f),
                Err(wgpu::SurfaceError::Timeout) => return false,
                Err(_) => {
                    surface.configure(&self.device, config);
                    return false;
                }
            },
            Target::Offscreen { .. } => None,
        };
        let view = match (&frame, &self.target) {
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
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tacoterm"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..4, 0..instances.len() as u32);
            }
        }
        self.queue.submit([encoder.finish()]);
        if let Some(f) = frame {
            f.present();
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
        self.device.poll(wgpu::Maintain::Wait);
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded * self.height) as usize);
        for row in data.chunks(padded as usize) {
            out.extend_from_slice(&row[..unpadded as usize]);
        }
        Some(out)
    }
}

fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("tacoterm"),
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
        },
        None,
    ))
    .map_err(|e| format!("cannot open GPU device: {e}"))
}
