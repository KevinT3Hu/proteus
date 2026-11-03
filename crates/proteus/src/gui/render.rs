//! wgpu renderer: one instanced pipeline drawing solid rectangles, glyphs from
//! a coverage atlas and a color (emoji) atlas, and curly underlines.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use swash::text::Codepoint;

use bytemuck::{Pod, Zeroable};
use proteus_core::cell::CellFlags;
use proteus_core::color::Rgb;
use proteus_core::style::{Attrs, Underline};
use proteus_core::{CursorShape, Point, TermMode, Terminal};

use super::font::{self, BOLD, Fonts, GlyphRef, ITALIC, Metrics};
use super::images::{self, Layer, TextureCache};
use super::ime::Preedit;
use super::rows::{Row, Rows};

/// Initial glyph atlas size; an atlas that fills up grows to `MAX_ATLAS`
/// (as far as the device allows) before glyphs are evicted.
const ATLAS: u32 = 2048;
const MAX_ATLAS: u32 = 4096;
/// GPU memory for image textures before the least recently drawn are freed.
pub const DEFAULT_TEXTURE_BUDGET: usize = 512 << 20;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    kind: u32,
    /// Image texture slot (0: none); not a vertex attribute, it splits draws.
    slot: u32,
}

const SOLID: u32 = 0;
const MASK: u32 = 1;
const COLOR: u32 = 2;
const CURLY: u32 = 3;
const IMAGE: u32 = 4;
/// Image sampled without smoothing.
const IMAGE_SHARP: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen: [f32; 2],
    linear: u32,
    _pad: u32,
}

/// Shelf packer for one atlas texture.
struct Shelves {
    shelves: Vec<(u32, u32, u32)>, // (y, height, used width)
    next_y: u32,
    size: u32,
}

impl Shelves {
    fn new(size: u32) -> Self {
        Self { shelves: Vec::new(), next_y: 0, size }
    }
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (w, h) = (w + 1, h + 1); // 1px gutter against bleeding
        let size = self.size;
        if w > size || h > size {
            return None;
        }
        for s in &mut self.shelves {
            if h <= s.1 && h * 4 >= s.1 * 3 && s.2 + w <= size {
                let x = s.2;
                s.2 += w;
                return Some((x, s.0));
            }
        }
        if self.next_y + h > size {
            return None;
        }
        let y = self.next_y;
        self.next_y += h;
        self.shelves.push((y, h, w));
        Some((0, y))
    }
}

/// The instances one grid row produced, reused while everything they were
/// built from hashes to `key` (cell content, colors, cursor, selection,
/// marks, geometry, glyph atlas).
#[derive(Default)]
struct CachedRow {
    key: u64,
    /// Frame that last drew it; rows not drawn in a frame are dropped.
    used: u64,
    bg: Vec<Instance>,
    fg: Vec<Instance>,
}

#[derive(Clone, Copy)]
struct AtlasGlyph {
    color: bool,
    uv: [f32; 4],
    left: i32,
    top: i32,
    w: u32,
    h: u32,
}

/// One terminal grid to draw, with its top-left corner in physical pixels.
pub struct PaneFrame<'a> {
    pub term: &'a Terminal,
    pub x: f32,
    pub y: f32,
    /// Has keyboard focus: solid (blinking) cursor; others get a hollow one.
    pub focused: bool,
    /// Input method composition, drawn at the cursor.
    pub preedit: Option<Preedit>,
    /// Scroll-mode cursor (column, viewport row); hides the terminal cursor.
    pub copy_cursor: Option<(usize, usize)>,
    /// Search matches to highlight (absolute points, inclusive).
    pub marks: Vec<Mark>,
    /// Viewport rows hidden at the top: the grid is drawn this many rows
    /// higher, so its last row clears an overlay at the bottom.
    pub skip: usize,
    /// Drawn over the tiled panes, in its own layer.
    pub floating: bool,
    /// Drawn under the grid: floating frames, the block gutter.
    pub backdrop: Vec<([f32; 4], Rgb)>,
    /// Floating panes: frame text, drawn over the grid.
    pub labels: Vec<Label>,
    /// What each row shows (see [`Rows`]).
    pub rows: Rows,
    /// Visual bell: tint the pane.
    pub flash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Match,
    /// The match the search is on.
    Current,
    /// Text of a hint (hint mode).
    Hint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub start: Point,
    pub end: Point,
    pub kind: MarkKind,
}

impl Mark {
    fn contains(&self, p: Point) -> bool {
        p >= self.start && p <= self.end
    }
}

/// Text in the window chrome (tab bar, hint bar), on the cell grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub x: f32,
    pub y: f32,
    /// Width in cells; the background fills it and text is truncated to it.
    pub cols: usize,
    pub text: String,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

/// Chrome colors: configured overrides with focused-pane palette defaults.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub colors: crate::theme::Ui,
    pub cursor_shape: Option<CursorShape>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub accent: Rgb,
    /// Tab/hint bar background.
    pub bar: Rgb,
    /// Inactive text and pane dividers.
    pub dim: Rgb,
}

/// Everything the renderer draws in one frame.
pub struct FrameInfo<'a> {
    pub panes: Vec<PaneFrame<'a>>,
    /// Window padding in physical pixels (indent of the overlay text).
    pub pad: f32,
    pub pad_y: f32,
    pub theme: Theme,
    pub cursor_blink_off: bool,
    /// Shape runs of text so fonts can apply ligatures.
    pub ligatures: bool,
    /// Dividers and title rows of tiled panes (under the floating panes).
    pub tiled_rects: Vec<([f32; 4], Rgb)>,
    pub tiled_labels: Vec<Label>,
    /// Solid chrome rectangles (x, y, w, h), drawn over everything.
    pub rects: Vec<([f32; 4], Rgb)>,
    /// Chrome text, drawn over `rects`.
    pub labels: Vec<Label>,
    /// Prompt or notice drawn over the bottom rows.
    pub overlay: Option<Overlay>,
    /// The image viewer, over the panes (under the bars).
    pub viewer: Option<super::viewer::ViewerFrame<'a>>,
}

/// A message bar at the bottom of the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    pub title: String,
    /// Optional second line (e.g. a paste preview), dimmed.
    pub detail: Option<String>,
    /// Key hints.
    pub keys: String,
}

/// First column of a preedit of `width` columns at cursor column `x`:
/// shifted left so it fits the line when possible.
pub fn preedit_start(x: usize, width: usize, cols: usize) -> usize {
    if x + width <= cols { x } else { cols.saturating_sub(width) }
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub lost: Arc<AtomicBool>,
    queue: wgpu::Queue,
    surface: Arc<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    mask: wgpu::Texture,
    color: wgpu::Texture,
    instances: wgpu::Buffer,
    capacity: usize,
    linear: bool,
    mask_shelves: Shelves,
    color_shelves: Shelves,
    /// Largest atlas side the device takes (capped at `MAX_ATLAS`).
    max_atlas: u32,
    atlas_layout: wgpu::BindGroupLayout,
    samplers: [wgpu::Sampler; 2],
    /// Changes whenever atlas positions of cached glyphs become invalid.
    glyph_epoch: u64,
    /// Instances of grid rows from earlier frames, by (terminal, viewport row).
    row_cache: HashMap<(usize, usize), CachedRow>,
    row_hasher: foldhash::fast::RandomState,
    frame_no: u64,
    glyphs: HashMap<GlyphRef, Option<AtlasGlyph>>,
    scratch: Vec<Instance>,
    /// Glyphs drawn after backgrounds.
    fg: Vec<Instance>,
    /// Finished layers, in drawing order.
    out: Vec<Instance>,
    pub adapter_info: String,
    /// An atlas filled up mid-frame: draw again so every glyph is valid.
    pub redraw_needed: bool,
    pub images: TextureCache,
    /// See [`Renderer::set_opacity`].
    opacity: f32,
    alpha_modes: Vec<wgpu::CompositeAlphaMode>,
}

fn rgba(c: Rgb, a: f32) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, a]
}

fn atlas_texture(device: &wgpu::Device, label: &str, format: wgpu::TextureFormat, size: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn atlas_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    mask: &wgpu::Texture,
    color: &wgpu::Texture,
    [nearest, smooth]: &[wgpu::Sampler; 2],
) -> wgpu::BindGroup {
    let mask_view = mask.create_view(&Default::default());
    let color_view = color.create_view(&Default::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&mask_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(nearest) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(smooth) },
        ],
    })
}

impl Renderer {
    pub fn new(
        instance: &wgpu::Instance,
        surface: Arc<wgpu::Surface<'static>>,
        size: (u32, u32),
    ) -> Result<Renderer, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no suitable GPU adapter: {e}"))?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("proteus"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("cannot create GPU device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format: text blended in gamma space looks like
        // every other terminal (and like the colors apps asked for).
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb() && matches!(f, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm))
            .unwrap_or(caps.formats[0]);
        let alpha_modes = caps.alpha_modes.clone();
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("proteus shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let mask = atlas_texture(&device, "mask atlas", wgpu::TextureFormat::R8Unorm, ATLAS);
        let color = atlas_texture(&device, "color atlas", wgpu::TextureFormat::Rgba8Unorm, ATLAS);
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };
        let nearest = sampler(wgpu::FilterMode::Nearest);
        let smooth = sampler(wgpu::FilterMode::Linear);
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let texture_ty = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, texture_ty),
                entry(2, texture_ty),
                entry(3, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                entry(4, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let samplers = [nearest, smooth];
        let bind_group = atlas_bind_group(&device, &layout, &globals, &mask, &color, &samplers);
        let images = TextureCache::new(&device, &queue, DEFAULT_TEXTURE_BUDGET);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout), Some(images.layout())],
            immediate_size: 0,
        });
        let attrs =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Uint32];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("proteus"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity = 4096;
        let instances = Self::instance_buffer(&device, capacity);
        Ok(Renderer {
            lost: Arc::new(AtomicBool::new(false)),
            device,
            queue,
            surface,
            linear: format.is_srgb(),
            config,
            pipeline,
            bind_group,
            globals,
            mask,
            color,
            instances,
            capacity,
            mask_shelves: Shelves::new(ATLAS),
            color_shelves: Shelves::new(ATLAS),
            max_atlas: adapter.limits().max_texture_dimension_2d.min(MAX_ATLAS),
            atlas_layout: layout,
            samplers,
            glyph_epoch: 0,
            row_cache: HashMap::default(),
            row_hasher: Default::default(),
            frame_no: 0,
            glyphs: HashMap::new(),
            scratch: Vec::new(),
            fg: Vec::new(),
            out: Vec::new(),
            adapter_info,
            redraw_needed: false,
            images,
            opacity: 1.0,
            alpha_modes,
        })
    }

    /// Opacity of the default background. Below 1 the surface is presented
    /// with premultiplied alpha, if the compositor supports it; returns
    /// whether it does.
    pub fn set_opacity(&mut self, opacity: f32) -> bool {
        let opacity = opacity.clamp(0.0, 1.0);
        let wanted = if opacity < 1.0 {
            [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::Inherit]
                .into_iter()
                .find(|m| self.alpha_modes.contains(m))
        } else {
            self.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque).then_some(wgpu::CompositeAlphaMode::Opaque)
        };
        let Some(mode) = wanted else {
            self.opacity = 1.0;
            return opacity >= 1.0;
        };
        self.opacity = opacity;
        if mode != self.config.alpha_mode {
            self.config.alpha_mode = mode;
            if !self.lost.load(Ordering::Acquire) {
                self.surface.configure(&self.device, &self.config);
            }
            self.redraw_needed = true;
        }
        true
    }

    /// Reconfigure the existing native surface with a fresh device. Creating a
    /// second native surface for the same wl_surface duplicates explicit-sync objects.
    pub fn recover(&self, instance: &wgpu::Instance, size: (u32, u32)) -> Result<Self, String> {
        Self::new(instance, self.surface.clone(), size)
    }

    fn instance_buffer(device: &wgpu::Device, n: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (n * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) != (self.config.width, self.config.height) {
            self.config.width = w;
            self.config.height = h;
            if !self.lost.load(Ordering::Acquire) {
                self.surface.configure(&self.device, &self.config);
            }
        }
    }

    /// Forget all cached glyphs (font or size changed, or atlas full).
    pub fn clear_glyphs(&mut self) {
        self.glyphs.clear();
        self.mask_shelves = Shelves::new(self.mask_shelves.size);
        self.color_shelves = Shelves::new(self.color_shelves.size);
        self.glyph_epoch += 1;
    }

    /// The mask or color atlas is full: grow it if the device allows,
    /// else start it over. Either way its glyphs re-rasterize as needed.
    fn atlas_full(&mut self, color: bool) {
        self.redraw_needed = true;
        self.glyph_epoch += 1;
        self.glyphs.retain(|_, v| v.is_none_or(|a| a.color != color));
        let size = if color { self.color_shelves.size } else { self.mask_shelves.size };
        let size = if size < self.max_atlas { size * 2 } else { size };
        if color {
            self.color_shelves = Shelves::new(size);
            if size != self.color.width() {
                self.color = atlas_texture(&self.device, "color atlas", wgpu::TextureFormat::Rgba8Unorm, size);
            }
        } else {
            self.mask_shelves = Shelves::new(size);
            if size != self.mask.width() {
                self.mask = atlas_texture(&self.device, "mask atlas", wgpu::TextureFormat::R8Unorm, size);
            }
        }
        self.bind_group =
            atlas_bind_group(&self.device, &self.atlas_layout, &self.globals, &self.mask, &self.color, &self.samplers);
    }

    fn glyph(&mut self, fonts: &mut Fonts, g: GlyphRef) -> Option<AtlasGlyph> {
        if let Some(&a) = self.glyphs.get(&g) {
            return a;
        }
        let entry = self.upload_glyph(fonts, g);
        self.glyphs.insert(g, entry);
        entry
    }

    fn upload_glyph(&mut self, fonts: &mut Fonts, g: GlyphRef) -> Option<AtlasGlyph> {
        let r = fonts.rasterize(g)?;
        let shelves = if r.color { &mut self.color_shelves } else { &mut self.mask_shelves };
        let (x, y) = match shelves.alloc(r.width, r.height) {
            Some(p) => p,
            None => {
                self.atlas_full(r.color);
                let shelves = if r.color { &mut self.color_shelves } else { &mut self.mask_shelves };
                shelves.alloc(r.width, r.height)?
            }
        };
        let (tex, bpp) = if r.color { (&self.color, 4) } else { (&self.mask, 1) };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &r.data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(r.width * bpp), rows_per_image: None },
            wgpu::Extent3d { width: r.width, height: r.height, depth_or_array_layers: 1 },
        );
        let a = tex.width() as f32;
        Some(AtlasGlyph {
            color: r.color,
            uv: [x as f32 / a, y as f32 / a, (x + r.width) as f32 / a, (y + r.height) as f32 / a],
            left: r.left,
            top: r.top,
            w: r.width,
            h: r.height,
        })
    }

    /// Render one frame. Returns false if the surface was not available
    /// (the caller should retry on the next frame).
    pub fn draw(&mut self, fonts: &mut Fonts, info: &FrameInfo) -> bool {
        if self.lost.load(Ordering::Acquire) {
            return false;
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        let enc = self.encode(fonts, info, &view);
        self.queue.submit([enc]);
        self.queue.present(frame);
        true
    }

    /// Render the current state offscreen and return it as RGBA8.
    pub fn snapshot(&mut self, fonts: &mut Fonts, info: &FrameInfo) -> (u32, u32, Vec<u8>) {
        let (w, h) = (self.config.width, self.config.height);
        let format = self.config.format;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("snapshot"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let draw = self.encode(fonts, info, &view);
        let stride = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("snapshot readback"),
            size: (stride * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([draw, enc.finish()]);
        buf.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = buf.get_mapped_range(..).map(|v| v.to_vec()).unwrap_or_default();
        let bgra = matches!(format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in data.chunks(stride as usize).take(h as usize) {
            for px in row[..(w * 4) as usize].as_chunks::<4>().0 {
                if bgra {
                    out.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        (w, h, out)
    }

    /// Build instances and record the render pass into `view`.
    fn encode(&mut self, fonts: &mut Fonts, info: &FrameInfo, view: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let m = fonts.metrics();
        let bg_default = info.theme.bg;
        self.scratch.clear();
        self.fg.clear();
        self.out.clear();
        self.images.begin();
        self.build(fonts, info, m);
        let frame = self.frame_no;
        self.row_cache.retain(|_, c| c.used == frame);
        self.frame_no += 1;
        self.flush_layer();
        let mut all = std::mem::take(&mut self.out);
        if all.len() > self.capacity {
            self.capacity = all.len().next_power_of_two();
            self.instances = Self::instance_buffer(&self.device, self.capacity);
        }
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&all));
        let globals = Globals {
            screen: [self.config.width as f32, self.config.height as f32],
            linear: self.linear as u32,
            _pad: 0,
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut c = rgba(bg_default, 1.0).map(|v| v as f64);
            if self.linear {
                for v in &mut c[..3] {
                    *v = if *v <= 0.04045 { *v / 12.92 } else { ((*v + 0.055) / 1.055).powf(2.4) };
                }
            }
            let a = self.opacity as f64;
            let (r, g, b) = (c[0] * a, c[1] * a, c[2] * a);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !all.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                // One draw per run of instances sampling the same image.
                let size = std::mem::size_of::<Instance>() as u64;
                let mut i = 0;
                while i < all.len() {
                    let slot = all[i].slot;
                    let n = all[i..].iter().take_while(|x| x.slot == slot).count();
                    pass.set_bind_group(1, self.images.bind(slot), &[]);
                    pass.set_vertex_buffer(0, self.instances.slice(i as u64 * size..(i + n) as u64 * size));
                    pass.draw(0..4, 0..n as u32);
                    i += n;
                }
            }
        }
        self.images.trim();
        all.clear();
        self.out = all;
        enc.finish()
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) {
        self.scratch.push(Instance { pos: [x, y], size: [w, h], uv: [0.0; 4], color: c, kind: SOLID, slot: 0 });
    }

    /// Build all instances: grids, then chrome, then the overlay.
    /// Backgrounds, then glyphs of what was built so far, become one layer
    /// over the previous ones.
    fn flush_layer(&mut self) {
        self.out.append(&mut self.scratch);
        self.out.append(&mut self.fg);
    }

    fn chrome(&mut self, fonts: &mut Fonts, rects: &[([f32; 4], Rgb)], labels: &[Label], m: Metrics) {
        let cw = m.cell_w as f32;
        for &([x, y, w, h], c) in rects {
            self.fg.push(solid(x, y, w, h, rgba(c, 1.0)));
        }
        for l in labels {
            if let Some(bg) = l.bg {
                self.fg.push(solid(l.x, l.y, l.cols as f32 * cw, m.cell_h as f32, rgba(bg, 1.0)));
            }
            let style = if l.bold { BOLD } else { 0 };
            self.text_line(fonts, &l.text, l.x, l.y, l.cols, style, rgba(l.fg, 1.0), m, cw);
        }
    }

    /// Tiled panes, their chrome, floating panes (each its own layer, with
    /// its frame), the window chrome, and the overlay.
    fn build(&mut self, fonts: &mut Fonts, info: &FrameInfo, m: Metrics) {
        for p in info.panes.iter().filter(|p| !p.floating) {
            for &([x, y, w, h], c) in &p.backdrop {
                self.scratch.push(solid(x, y, w, h, rgba(c, 1.0)));
            }
            self.build_pane(fonts, p, info, m);
        }
        self.chrome(fonts, &info.tiled_rects, &info.tiled_labels, m);
        self.flush_layer();
        for p in info.panes.iter().filter(|p| p.floating) {
            for &([x, y, w, h], c) in &p.backdrop {
                self.scratch.push(solid(x, y, w, h, rgba(c, 1.0)));
            }
            self.build_pane(fonts, p, info, m);
            self.chrome(fonts, &[], &p.labels, m);
            self.flush_layer();
        }
        if let Some(v) = &info.viewer {
            let (w, h) = (self.config.width as f32, self.config.height as f32);
            self.fg.push(solid(0.0, 0.0, w, h, [0.0, 0.0, 0.0, 0.92]));
            let slot = self.images.slot(&self.device, &self.queue, v.key, 1, v.size, Some(v.pixels));
            if let Some(slot) = slot {
                let [x, y, w, h] = v.rect;
                let kind = if v.sharp { IMAGE_SHARP } else { IMAGE };
                self.fg.push(Instance {
                    pos: [x, y],
                    size: [w, h],
                    uv: [0.0, 0.0, 1.0, 1.0],
                    color: [1.0; 4],
                    kind,
                    slot,
                });
            }
            self.flush_layer();
        }
        self.chrome(fonts, &info.rects, &info.labels, m);
        self.flush_layer();
        if let Some(o) = &info.overlay {
            self.build_overlay(fonts, o, info, m);
        }
    }

    /// Build the instances of one terminal grid.
    fn build_pane(&mut self, fonts: &mut Fonts, p: &PaneFrame, info: &FrameInfo, m: Metrics) {
        let t = p.term;
        let pal = t.palette();
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let skip = p.skip.min(t.lines() - 1);
        let (px, py) = (p.x, p.y - skip as f32 * ch);
        let default_bg = pal.background();
        // Each pane owns its background, independently of chrome and other panes' OSC colors.
        // Paint it before images so default cells can omit redundant background runs.
        // A translucent window shows the cleared background instead, so the
        // alpha does not compound where the colors agree.
        if self.opacity >= 1.0 || default_bg != info.theme.bg {
            self.rect(p.x, p.y, t.cols() as f32 * cw, (t.lines() - skip) as f32 * ch, rgba(default_bg, self.opacity));
        }
        let reverse = t.mode().contains(TermMode::REVERSE_VIDEO);
        let selection = t.selection_range();
        let cursor = t.cursor();
        let cursor_shape = info.theme.cursor_shape.unwrap_or_else(|| t.cursor_shape());
        let blink_off = info.cursor_blink_off && p.focused;
        let show_cursor = t.cursor_visible() && !blink_off && p.preedit.is_none() && p.copy_cursor.is_none();
        let grid = t.grid();
        let first = (0..p.rows.0.len()).find_map(|y| p.rows.line(y)).unwrap_or(0);
        let last = p.rows.last_line().unwrap_or(0);
        let marks: Vec<Mark> =
            p.marks.iter().copied().filter(|m| m.end.line >= first && m.start.line <= last).collect();
        let cursor_row = p.rows.row_of(grid.abs_line(cursor.y)).filter(|_| show_cursor || p.preedit.is_some());
        let cols = t.cols();
        let cell = (m.cell_w, m.cell_h);
        let views =
            images::place(t.visible_images(), &p.rows, grid.viewport_line(0), cols, cell, t.cell_pixel_size(), skip);
        self.pane_images(fonts, &views, Layer::UnderBackground, (px, py), pal.foreground(), m);
        self.pane_images(fonts, &views, Layer::UnderText, (px, py), pal.foreground(), m);
        // Everything besides a row's own content that its instances depend on.
        let context = {
            let mut h = self.row_hasher.build_hasher();
            (self.glyph_epoch, t.style_generation(), pal, reverse, show_cursor, p.focused).hash(&mut h);
            (std::mem::discriminant(&cursor_shape), cursor.x, cols, px.to_bits(), py.to_bits()).hash(&mut h);
            (m.cell_w, m.cell_h, m.baseline, m.offset_x.to_bits(), m.underline_y, m.underline_h, m.strike_y)
                .hash(&mut h);
            let c = &info.theme.colors;
            (info.ligatures, info.theme.accent, info.theme.bg, c.selection_foreground, c.selection_background)
                .hash(&mut h);
            (c.search_current_background, c.search_background, c.search_foreground, c.cursor_text).hash(&mut h);
            h.finish()
        };
        for (y, r) in p.rows.0.iter().enumerate().skip(skip) {
            let top = py + y as f32 * ch;
            let line = match *r {
                Row::Line(l) => l,
                Row::Empty => continue,
                Row::Fold { start, end, .. } => {
                    let n = end - start;
                    let text = format!("▸ {} line{}", group_digits(n), if n == 1 { "" } else { "s" });
                    let fg = pal.foreground();
                    self.rect(px, top, cols as f32 * cw, ch, rgba(super::chrome::mix(default_bg, fg, 0.08), 1.0));
                    self.text_line(fonts, &text, px, top, cols, ITALIC, rgba(fg, 0.6), m, cw);
                    continue;
                }
            };
            let Some(row) = grid.abs_row(line) else { continue };
            let is_cursor_row = cursor_row == Some(y);
            let layout = t.visual_row(line);
            let key = {
                let mut h = self.row_hasher.build_hasher();
                (context, y, line, &row.cells, is_cursor_row).hash(&mut h);
                if let Some(r) = selection.filter(|r| r.start.line <= line && line <= r.end.line) {
                    (r.start, r.end, r.rect).hash(&mut h);
                }
                for m in marks.iter().filter(|m| m.start.line <= line && line <= m.end.line) {
                    (m.start, m.end, m.kind as u8).hash(&mut h);
                }
                if let Some((cx, cy)) = p.copy_cursor
                    && cy == y
                {
                    cx.hash(&mut h);
                }
                if layout.has_rtl() {
                    for x in 0..cols {
                        (layout.visual(x), layout.rtl(x)).hash(&mut h);
                    }
                }
                h.finish()
            };
            let slot = (std::ptr::from_ref(t) as usize, y);
            if let Some(c) = self.row_cache.get_mut(&slot)
                && c.key == key
            {
                c.used = self.frame_no;
                self.scratch.extend_from_slice(&c.bg);
                self.fg.extend_from_slice(&c.fg);
                continue;
            }
            let (bg_start, fg_start, epoch) = (self.scratch.len(), self.fg.len(), self.glyph_epoch);
            let mut run = Run::default();
            let mut cell_buf = String::new();
            // Run-length merge of backgrounds on this row.
            let mut bg_run: Option<(usize, Rgb)> = None;
            let mut x = 0;
            // History rows do not store trailing blanks.
            while x < cols {
                let cell = row.cells.get(x).unwrap_or(&BLANK_CELL);
                let flags = cell.flags();
                let width = if flags.contains(CellFlags::WIDE) { 2 } else { 1 };
                if flags.contains(CellFlags::WIDE_SPACER) {
                    x += 1;
                    continue;
                }
                let vx = layout.visual(x);
                let rtl = layout.rtl(x);
                let style = t.style(cell.style);
                let selected = selection.is_some_and(|r| {
                    if r.rect {
                        line >= r.start.line && line <= r.end.line && vx <= r.end.col && vx + width > r.start.col
                    } else {
                        r.contains(Point::new(line, x))
                    }
                });
                let mut fg = pal.resolve(style.fg, true);
                let mut bg = pal.resolve(style.bg, false);
                if style.attrs.contains(Attrs::BOLD)
                    && let proteus_core::color::Color::Indexed(i @ 0..8) = style.fg
                {
                    fg = pal.indexed(i + 8);
                }
                if style.attrs.contains(Attrs::INVERSE) ^ reverse ^ selected {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if selected {
                    fg = info.theme.colors.selection_foreground.unwrap_or(fg);
                    bg = info.theme.colors.selection_background.unwrap_or(bg);
                }
                if let Some(m) = marks.iter().find(|m| m.contains(Point::new(line, x))) {
                    match m.kind {
                        MarkKind::Match | MarkKind::Current => {
                            bg = if m.kind == MarkKind::Current {
                                info.theme.colors.search_current_background.unwrap_or_else(|| pal.indexed(208))
                            } else {
                                info.theme.colors.search_background.unwrap_or_else(|| pal.indexed(3))
                            };
                            fg = info.theme.colors.search_foreground.unwrap_or_else(|| pal.indexed(0));
                        }
                        MarkKind::Hint => fg = info.theme.accent,
                    }
                }
                if p.copy_cursor == Some((x, y)) {
                    bg = info.theme.accent;
                    fg = info.theme.bg;
                }
                let is_cursor = show_cursor && is_cursor_row && x == cursor.x && p.focused;
                let is_cursor_block = is_cursor && cursor_shape == CursorShape::Block;
                if is_cursor_block {
                    let cc = pal.cursor().unwrap_or(fg);
                    fg = info.theme.colors.cursor_text.unwrap_or(bg);
                    bg = cc;
                }
                let alpha = if style.attrs.contains(Attrs::DIM) { 0.6 } else { 1.0 };

                // Backgrounds.
                let cell_bg = if bg == default_bg { None } else { Some(bg) };
                if layout.has_rtl() {
                    if let Some(bg) = cell_bg {
                        self.rect(px + vx as f32 * cw, top, width as f32 * cw, ch, rgba(bg, 1.0));
                    }
                } else {
                    match (bg_run, cell_bg) {
                        (Some((_, c)), Some(b)) if c == b => {}
                        _ => {
                            if let Some((start, c)) = bg_run.take() {
                                self.rect(px + start as f32 * cw, top, (x - start) as f32 * cw, ch, rgba(c, 1.0));
                            }
                            bg_run = cell_bg.map(|b| (x, b));
                        }
                    }
                }
                // Glyph. Single-cell clusters the primary face covers are
                // collected into runs and shaped together (ligatures, mark
                // positioning); other clusters (wide, emoji, fallback fonts)
                // are shaped on their own.
                let hidden = style.attrs.contains(Attrs::HIDDEN);
                let base = cell.ch();
                let blank = base == ' ' || flags.contains(CellFlags::LEADING_WIDE_SPACER);
                let color = rgba(fg, alpha);
                let st = (style.attrs.contains(Attrs::BOLD) as u8 * BOLD)
                    | (style.attrs.contains(Attrs::ITALIC) as u8 * ITALIC);
                let drawable = !hidden && !blank && base != proteus_core::graphics::placeholder::PLACEHOLDER;
                let text = cell_text(cell, &mut cell_buf);
                let wide = width == 2;
                let emoji = drawable && font::emoji_presentation(text, wide);
                let shapable = drawable && !wide && !emoji && (rtl || fonts.in_primary(text, st));
                let script = font::script_tag(text);
                if !(shapable && (info.ligatures || rtl) && run.continues(x, st, color, script, rtl)) {
                    self.flush_run(fonts, &mut run, m, cw);
                }
                if shapable {
                    if run.text.is_empty() {
                        run.start = x;
                        run.top = top;
                        run.left = px;
                        run.style = st;
                        run.color = color;
                        run.script = None;
                        run.rtl = rtl;
                        run.ligatures = info.ligatures;
                    }
                    run.script = run.script.or(script);
                    run.positions.push(vx);
                    run.colors.push(color);
                    run.starts.push(run.text.len() as u32);
                    run.text.push_str(text);
                } else if drawable {
                    let left = px + vx as f32 * cw;
                    let box_w = width as f32 * cw;
                    if cell.extra().is_none() && !emoji {
                        if let Some(g) = fonts.glyph(if rtl { base.mirror().unwrap_or(base) } else { base }, st) {
                            self.place_glyph(fonts, g, left, top, box_w, 0.0, 0.0, m, color);
                        }
                    } else {
                        let mirrored;
                        let text = if rtl {
                            mirrored = text.chars().map(|c| c.mirror().unwrap_or(c)).collect::<String>();
                            &mirrored
                        } else {
                            text
                        };
                        let glyphs = fonts.shape_cluster(text, st, wide);
                        for g in glyphs.iter() {
                            self.place_glyph(fonts, g.glyph, left, top, box_w, g.x, g.y, m, color);
                        }
                    }
                }

                // Decorations.
                let w = width as f32 * cw;
                let left = px + vx as f32 * cw;
                let uc = match style.underline_color {
                    proteus_core::color::Color::Default => fg,
                    c => pal.resolve(c, true),
                };
                let th = m.underline_h as f32;
                let uy = top + m.underline_y as f32;
                let link = style.hyperlink.is_some() && style.underline == Underline::None;
                match style.underline {
                    _ if hidden => {}
                    Underline::None if !link => {}
                    Underline::None | Underline::Dotted => {
                        let mut dx = 0.0;
                        while dx < w {
                            self.fg.push(solid(left + dx, uy, th.min(w - dx), th, rgba(uc, alpha)));
                            dx += th * 2.0;
                        }
                    }
                    Underline::Single => self.fg.push(solid(left, uy, w, th, rgba(uc, alpha))),
                    Underline::Double => {
                        let y2 = (uy - th * 2.0).max(top);
                        self.fg.push(solid(left, uy, w, th, rgba(uc, alpha)));
                        self.fg.push(solid(left, y2, w, th, rgba(uc, alpha)));
                    }
                    Underline::Dashed => {
                        let dash = (cw / 2.0).max(2.0);
                        let mut dx = 0.0;
                        while dx < w {
                            self.fg.push(solid(left + dx, uy, (dash * 0.7).min(w - dx), th, rgba(uc, alpha)));
                            dx += dash;
                        }
                    }
                    Underline::Curly => {
                        let h = (th * 3.0).max(3.0);
                        let y0 = (uy - h / 2.0).min(top + ch - h);
                        self.fg.push(Instance {
                            pos: [left, y0],
                            size: [w, h],
                            uv: [th, cw.max(4.0), 0.0, h],
                            color: rgba(uc, alpha),
                            kind: CURLY,
                            slot: 0,
                        });
                    }
                }
                if style.attrs.contains(Attrs::STRIKE) && !hidden {
                    self.fg.push(solid(left, top + m.strike_y as f32, w, th, rgba(fg, alpha)));
                }
                if style.attrs.contains(Attrs::OVERLINE) && !hidden {
                    self.fg.push(solid(left, top, w, th, rgba(fg, alpha)));
                }

                // Non-block cursors.
                if show_cursor && is_cursor_row && x == cursor.x && !is_cursor_block {
                    let cc = pal.cursor().unwrap_or(pal.resolve(style.fg, true));
                    let c = rgba(cc, 1.0);
                    let thick = (cw / 8.0).round().max(1.0);
                    if !p.focused {
                        self.fg.push(solid(left, top, w, thick, c));
                        self.fg.push(solid(left, top + ch - thick, w, thick, c));
                        self.fg.push(solid(left, top, thick, ch, c));
                        self.fg.push(solid(left + w - thick, top, thick, ch, c));
                    } else if cursor_shape == CursorShape::Bar {
                        self.fg.push(solid(left, top, thick, ch, c));
                    } else {
                        self.fg.push(solid(left, top + ch - thick, w, thick, c));
                    }
                }
                x += width;
            }
            self.flush_run(fonts, &mut run, m, cw);
            if let Some((start, c)) = bg_run {
                self.rect(px + start as f32 * cw, top, (cols - start) as f32 * cw, ch, rgba(c, 1.0));
            }
            // A glyph atlas reset mid-row leaves stale positions: not reusable.
            if self.glyph_epoch == epoch {
                let c = self.row_cache.entry(slot).or_default();
                c.key = key;
                c.used = self.frame_no;
                c.bg.clear();
                c.bg.extend_from_slice(&self.scratch[bg_start..]);
                c.fg.clear();
                c.fg.extend_from_slice(&self.fg[fg_start..]);
            }
        }
        self.pane_images(fonts, &views, Layer::OverText, (px, py), pal.foreground(), m);
        if let (Some(pre), Some(row)) = (&p.preedit, cursor_row) {
            self.build_preedit(t, fonts, pre, (px, py), row, m);
        }
        if p.flash {
            // Premultiplied: a light veil of the foreground color.
            let a = 0.18;
            let c = rgba(info.theme.colors.bell.unwrap_or_else(|| pal.foreground()), 1.0);
            let veil = [c[0] * a, c[1] * a, c[2] * a, a];
            self.fg.push(solid(px, p.y, cols as f32 * cw, (t.lines() - skip) as f32 * ch, veil));
        }
    }

    /// Draw the images of `layer` (see [`images::place`]) with the grid's
    /// viewport row 0 at `origin`. Images under the backgrounds go before
    /// them; the others are drawn in order with the glyphs.
    fn pane_images(
        &mut self,
        fonts: &mut Fonts,
        views: &[proteus_core::ImageView],
        layer: Layer,
        origin: (f32, f32),
        fg: Rgb,
        m: Metrics,
    ) {
        for v in views.iter().filter(|v| Layer::of(v.z) == layer && !v.loading) {
            let d = v.dest;
            let (x, y, w, h) = (origin.0 + d.x as f32, origin.1 + d.y as f32, d.w as f32, d.h as f32);
            let slot = self.images.slot(&self.device, &self.queue, v.key, v.generation, (v.width, v.height), v.pixels);
            let quad = match slot {
                Some(slot) => {
                    Instance { pos: [x, y], size: [w, h], uv: images::uv(v), color: [1.0; 4], kind: IMAGE, slot }
                }
                None => {
                    self.evicted_placeholder(fonts, (x, y, w, h), fg, m);
                    continue;
                }
            };
            if layer == Layer::UnderBackground { self.scratch.push(quad) } else { self.fg.push(quad) }
        }
    }

    /// Where an image whose pixels were dropped (memory quota) would be: a
    /// faint box with a note.
    fn evicted_placeholder(&mut self, fonts: &mut Fonts, (x, y, w, h): (f32, f32, f32, f32), fg: Rgb, m: Metrics) {
        let veil = |a: f32| {
            let c = rgba(fg, 1.0);
            [c[0] * a, c[1] * a, c[2] * a, a]
        };
        let t = 1.0f32.max((m.cell_w as f32 / 10.0).round());
        self.fg.push(solid(x, y, w, h, veil(0.05)));
        for (bx, by, bw, bh) in [(x, y, w, t), (x, y + h - t, w, t), (x, y, t, h), (x + w - t, y, t, h)] {
            self.fg.push(solid(bx, by, bw, bh, veil(0.25)));
        }
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let cols = ((w - 2.0 * cw) / cw).floor();
        if cols >= 3.0 && h >= ch {
            let text_y = y + ((h - ch) / 2.0).floor();
            self.text_line(fonts, "image evicted", x + cw, text_y, cols as usize, ITALIC, rgba(fg, 0.5), m, cw);
        }
    }

    /// Message bar over the last rows, in inverted colors with a top rule.
    fn build_overlay(&mut self, fonts: &mut Fonts, o: &Overlay, info: &FrameInfo, m: Metrics) {
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let px = info.pad;
        let (fg, bg) = (info.theme.bg, info.theme.fg);
        let width = self.config.width as f32;
        let height = self.config.height as f32;
        let cols = ((width - 2.0 * px) / cw).max(1.0) as usize;
        let max_rows = ((height / ch) as usize).max(1);
        let mut lines: Vec<(&str, f32)> = vec![(o.title.as_str(), 1.0)];
        if let Some(d) = &o.detail {
            lines.push((d.as_str(), 0.7));
        }
        lines.push((o.keys.as_str(), 0.85));
        let rows = lines.len().min(max_rows);
        let top = (height - info.pad_y - rows as f32 * ch).max(0.0);
        // Cover the padding too so the bar reaches the window edges.
        self.fg.push(solid(0.0, top, width, height - top, rgba(bg, 1.0)));
        self.fg.push(solid(0.0, top, width, (m.underline_h as f32).max(1.0), rgba(info.theme.accent, 1.0)));
        for (i, (text, alpha)) in lines.into_iter().take(rows).enumerate() {
            let y = top + i as f32 * ch;
            let style = if i == 0 { BOLD } else { 0 };
            self.text_line(fonts, text, px, y, cols, style, rgba(fg, alpha), m, cw);
        }
    }

    /// Lay out `text` grapheme by grapheme on the grid, truncated with `…`
    /// to `cols` columns.
    #[allow(clippy::too_many_arguments)]
    fn text_line(
        &mut self,
        fonts: &mut Fonts,
        text: &str,
        left: f32,
        top: f32,
        cols: usize,
        style: u8,
        color: [f32; 4],
        m: Metrics,
        cw: f32,
    ) {
        let layout = proteus_core::bidi::TextLayout::new(text, cols);
        let mut run = Run::default();
        for cell in layout.cells {
            let emoji = font::emoji_presentation(&cell.text, cell.width == 2);
            let join = cell.rtl && cell.width == 1 && !emoji && cell.text != " ";
            let script = font::script_tag(&cell.text);
            if !join || !run.continues(cell.logical, style, color, script, true) {
                self.flush_run(fonts, &mut run, m, cw);
            }
            if join {
                if run.text.is_empty() {
                    run.start = cell.logical;
                    run.left = left;
                    run.top = top;
                    run.style = style;
                    run.color = color;
                    run.rtl = true;
                    run.ligatures = true;
                    run.script = None;
                }
                run.script = run.script.or(script);
                run.starts.push(run.text.len() as u32);
                run.positions.push(cell.visual);
                run.colors.push(color);
                run.text.push_str(&cell.text);
            } else if cell.text != " " {
                let text =
                    if cell.rtl { cell.text.chars().map(|c| c.mirror().unwrap_or(c)).collect() } else { cell.text };
                for sg in fonts.shape_cluster(&text, style, cell.width == 2).iter() {
                    self.place_glyph(
                        fonts,
                        sg.glyph,
                        left + cell.visual as f32 * cw,
                        top,
                        cell.width as f32 * cw,
                        sg.x,
                        sg.y,
                        m,
                        color,
                    );
                }
            }
        }
        self.flush_run(fonts, &mut run, m, cw);
    }

    /// Draw the input method's composition over the grid at the cursor:
    /// opaque background, underline, thicker underline under the selected
    /// segment, and a bar cursor.
    #[allow(clippy::too_many_arguments)]
    fn build_preedit(
        &mut self,
        t: &Terminal,
        fonts: &mut Fonts,
        p: &Preedit,
        origin: (f32, f32),
        row: usize,
        m: Metrics,
    ) {
        let pal = t.palette();
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let (px, py) = origin;
        let cursor = t.cursor();
        let x = t.visual_row(t.grid().abs_line(cursor.y)).visual(cursor.x);
        let start = preedit_start(x, p.width(), t.cols());
        let top = py + row as f32 * ch;
        let left = px + start as f32 * cw;
        let fg = rgba(pal.foreground(), 1.0);
        let th = m.underline_h as f32;
        let uy = top + m.underline_y as f32;
        let cells = p.cells();
        let width = (p.width().min(t.cols() - start)) as f32 * cw;
        // In `fg` so it covers the glyphs of the cells underneath.
        self.fg.push(solid(left, top, width, ch, rgba(pal.background(), 1.0)));
        self.fg.push(solid(left, uy, width, th, fg));
        let layout = proteus_core::bidi::TextLayout::new(&p.text, t.cols() - start);
        self.text_line(fonts, &p.text, left, top, t.cols() - start, 0, fg, m, cw);
        for c in &cells {
            if let Some((b, e)) = p.cursor
                && b < e
                && c.bytes.0 < e
                && c.bytes.1 > b
                && let Some(placed) = layout.cells.iter().find(|s| s.start == c.bytes.0)
            {
                let x = left + placed.visual as f32 * cw;
                let thick = (th * 2.0).max(2.0);
                self.fg.push(solid(x, (uy + th - thick).max(top), placed.width as f32 * cw, thick, fg));
            }
        }
        if let Some(col) = p.cursor.filter(|(b, e)| b == e).map(|(b, _)| layout.caret(b)) {
            let x = left + col.min(t.cols() - start) as f32 * cw;
            let thick = (cw / 8.0).round().max(1.0);
            let c = rgba(pal.cursor().unwrap_or(pal.foreground()), 1.0);
            self.fg.push(solid(x.min(px + t.cols() as f32 * cw - thick), top, thick, ch, c));
        }
    }

    /// Shape and emit a pending run of cells, leaving it empty.
    fn flush_run(&mut self, fonts: &mut Fonts, run: &mut Run, m: Metrics, cw: f32) {
        if run.text.is_empty() {
            return;
        }
        let glyphs = fonts.shape_directional(&run.text, run.style, run.rtl, run.ligatures);
        // Fallback fonts can have taller cursive/mark extents than the primary
        // monospace face. Fit the whole connected run uniformly into its row.
        let mut vertical_scale = 1.0;
        let mut vertical_shift = 0.0;
        if run.rtl {
            let mut top = f32::INFINITY;
            let mut bottom = f32::NEG_INFINITY;
            for g in glyphs.iter() {
                if let Some(a) = self.glyph(fonts, g.glyph) {
                    let y = m.baseline as f32 - g.y - a.top as f32;
                    top = top.min(y);
                    bottom = bottom.max(y + a.h as f32);
                }
            }
            if top.is_finite() && (top < 0.0 || bottom > m.cell_h as f32) {
                let height = (bottom - top).max(1.0);
                vertical_scale = ((m.cell_h as f32 - 2.0).max(1.0) / height).min(1.0);
                vertical_shift = (m.cell_h as f32 - height * vertical_scale) / 2.0 - top * vertical_scale;
            }
        }
        for g in glyphs.iter() {
            let cell = run.starts.partition_point(|&s| s <= g.src).saturating_sub(1);
            if !run.rtl {
                let left = run.left + run.positions[cell] as f32 * cw;
                self.place_glyph(fonts, g.glyph, left, run.top, cw, g.x, g.y, m, run.colors[cell]);
                continue;
            }
            let end = run.starts.partition_point(|&s| s < g.end).max(cell + 1).min(run.positions.len());
            let column = run.positions[cell..end].iter().copied().min().unwrap_or(run.positions[cell]);
            let box_w = (end - cell) as f32 * cw;
            let scale = if g.advance > 0.0 { box_w / g.advance } else { 1.0 };
            let before = self.fg.len();
            self.place_scaled_glyph(
                fonts,
                g.glyph,
                run.left + column as f32 * cw,
                run.top,
                box_w,
                g.x,
                g.y,
                m,
                run.colors[cell],
                scale,
                vertical_scale,
                vertical_shift,
            );
            if self.fg.len() > before {
                let glyph = self.fg.pop().unwrap();
                // A ligature can cover differently selected cells. Color each visual slice.
                for i in cell..end {
                    let x = run.left + run.positions[i] as f32 * cw;
                    let l = glyph.pos[0].max(x);
                    let r = (glyph.pos[0] + glyph.size[0]).min(x + cw);
                    if r <= l {
                        continue;
                    }
                    let mut part = glyph;
                    let u = glyph.uv[2] - glyph.uv[0];
                    part.uv[0] = glyph.uv[0] + u * (l - glyph.pos[0]) / glyph.size[0];
                    part.uv[2] = glyph.uv[0] + u * (r - glyph.pos[0]) / glyph.size[0];
                    part.pos[0] = l;
                    part.size[0] = r - l;
                    part.color = run.colors[i];
                    self.fg.push(part);
                }
            }
        }
        run.text.clear();
        run.starts.clear();
        run.positions.clear();
        run.colors.clear();
    }

    /// Draw a glyph in the cell box starting at `left`, offset by (`dx`, `dy`)
    /// from the pen position (`dy` is up).
    #[allow(clippy::too_many_arguments)]
    fn place_glyph(
        &mut self,
        fonts: &mut Fonts,
        g: GlyphRef,
        left: f32,
        top: f32,
        box_w: f32,
        dx: f32,
        dy: f32,
        m: Metrics,
        color: [f32; 4],
    ) {
        self.place_scaled_glyph(fonts, g, left, top, box_w, dx, dy, m, color, 1.0, 1.0, 0.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn place_scaled_glyph(
        &mut self,
        fonts: &mut Fonts,
        g: GlyphRef,
        left: f32,
        top: f32,
        box_w: f32,
        dx: f32,
        dy: f32,
        m: Metrics,
        color: [f32; 4],
        scale: f32,
        vertical_scale: f32,
        vertical_shift: f32,
    ) {
        let Some(a) = self.glyph(fonts, g) else { return };
        let (mut w, mut h) = (a.w as f32 * scale, a.h as f32 * vertical_scale);
        let mut x = (left + dx * scale + m.offset_x).round() + a.left as f32 * scale;
        let mut y = (top + (m.baseline as f32 - dy - a.top as f32) * vertical_scale + vertical_shift).round();
        if a.color {
            // Fit color glyphs (emoji bitmaps) into their cell box.
            let ch = m.cell_h as f32;
            let s = (box_w / w).min(ch / h).min(1.0);
            if s < 1.0 {
                w *= s;
                h *= s;
            }
            x = left + (box_w - w) / 2.0 + m.offset_x;
            y = y.clamp(top, top + ch - h);
        }
        self.fg.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv: a.uv,
            color,
            kind: if a.color { COLOR } else { MASK },
            slot: 0,
        });
    }
}

/// Consecutive cells of one row that are shaped together: same font style,
/// color and script, all single-width and covered by the primary face.
#[derive(Default)]
struct Run {
    /// Column of the first cell.
    start: usize,
    top: f32,
    /// Left edge of the grid.
    left: f32,
    style: u8,
    color: [f32; 4],
    /// First non-Common script in the run.
    script: Option<u32>,
    text: String,
    /// Byte offset in `text` where each cell's text starts.
    starts: Vec<u32>,
    positions: Vec<usize>,
    colors: Vec<[f32; 4]>,
    rtl: bool,
    ligatures: bool,
}

impl Run {
    fn continues(&self, x: usize, style: u8, color: [f32; 4], script: Option<u32>, rtl: bool) -> bool {
        self.text.is_empty()
            || (self.style == style
                && (rtl || self.color == color)
                && self.rtl == rtl
                && self.start + self.starts.len() == x
                && (script.is_none() || self.script.is_none() || script == self.script))
    }
}

static BLANK_CELL: proteus_core::cell::Cell = proteus_core::cell::Cell::blank(proteus_core::style::StyleId::DEFAULT);

/// A cell's full text: the base char plus combining/joined extras.
fn cell_text<'a>(cell: &'a proteus_core::cell::Cell, buf: &'a mut String) -> &'a str {
    buf.clear();
    buf.push(cell.ch());
    if let Some(extra) = cell.extra() {
        buf.push_str(extra);
    }
    buf
}

/// `12345` → `12,345`.
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn solid(x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) -> Instance {
    Instance { pos: [x, y], size: [w, h], uv: [0.0; 4], color: c, kind: SOLID, slot: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("shader.wgsl")).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .expect("WGSL validates");
        assert_eq!(std::mem::size_of::<Instance>(), 56);
    }

    #[test]
    fn digits() {
        assert_eq!(group_digits(7), "7");
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn shelf_packing() {
        let mut s = Shelves::new(ATLAS);
        assert_eq!(s.alloc(10, 20), Some((0, 0)));
        assert_eq!(s.alloc(10, 18), Some((11, 0)));
        // Much shorter glyphs open a new shelf.
        assert_eq!(s.alloc(10, 5), Some((0, 21)));
        assert_eq!(s.alloc(ATLAS + 1, 1), None);
        let mut full = Shelves::new(ATLAS);
        while full.alloc(1000, 1000).is_some() {}
        assert!(full.alloc(1000, 1000).is_none());
        // A grown atlas takes what did not fit.
        assert!(Shelves::new(MAX_ATLAS).alloc(ATLAS + 1, 1).is_some());
    }
}
