//! wgpu renderer: one instanced pipeline drawing solid rectangles, glyphs from
//! a coverage atlas and a color (emoji) atlas, and curly underlines.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use proteus_core::cell::CellFlags;
use proteus_core::color::Rgb;
use proteus_core::style::{Attrs, Underline};
use proteus_core::{CursorShape, Point, TermMode, Terminal};

use super::font::{self, BOLD, Fonts, GlyphRef, ITALIC, Metrics};
use super::ime::Preedit;

const ATLAS: u32 = 2048;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    kind: u32,
}

const SOLID: u32 = 0;
const MASK: u32 = 1;
const COLOR: u32 = 2;
const CURLY: u32 = 3;

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
}

impl Shelves {
    fn new() -> Self {
        Self { shelves: Vec::new(), next_y: 0 }
    }
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (w, h) = (w + 1, h + 1); // 1px gutter against bleeding
        if w > ATLAS || h > ATLAS {
            return None;
        }
        for s in &mut self.shelves {
            if h <= s.1 && h * 4 >= s.1 * 3 && s.2 + w <= ATLAS {
                let x = s.2;
                s.2 += w;
                return Some((x, s.0));
            }
        }
        if self.next_y + h > ATLAS {
            return None;
        }
        let y = self.next_y;
        self.next_y += h;
        self.shelves.push((y, h, w));
        Some((0, y))
    }
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

/// Everything about the frame the renderer needs besides the terminal.
pub struct FrameInfo {
    pub focused: bool,
    /// Padding around the grid in physical pixels.
    pub pad: (u32, u32),
    /// Preedit text could go here later (IME).
    pub cursor_blink_off: bool,
    /// Shape runs of text so fonts can apply ligatures.
    pub ligatures: bool,
    /// Input method composition, drawn at the cursor.
    pub preedit: Option<Preedit>,
    /// Prompt or notice drawn over the bottom rows.
    pub overlay: Option<Overlay>,
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
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
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
    glyphs: HashMap<GlyphRef, Option<AtlasGlyph>>,
    scratch: Vec<Instance>,
    /// Glyphs drawn after backgrounds.
    fg: Vec<Instance>,
    pub adapter_info: String,
    /// An atlas filled up mid-frame: draw again so every glyph is valid.
    pub redraw_needed: bool,
}

fn rgba(c: Rgb, a: f32) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, a]
}

impl Renderer {
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
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
        let tex = |label, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: ATLAS, height: ATLAS, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let mask = tex("mask atlas", wgpu::TextureFormat::R8Unorm);
        let color = tex("color atlas", wgpu::TextureFormat::Rgba8Unorm);
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
                entry(0, wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }),
                entry(1, texture_ty),
                entry(2, texture_ty),
                entry(3, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                entry(4, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let mask_view = mask.create_view(&Default::default());
        let color_view = color.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&mask_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&nearest) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&smooth) },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Uint32];
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
            mask_shelves: Shelves::new(),
            color_shelves: Shelves::new(),
            glyphs: HashMap::new(),
            scratch: Vec::new(),
            fg: Vec::new(),
            adapter_info,
            redraw_needed: false,
        })
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
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Forget all cached glyphs (font or size changed, or atlas full).
    pub fn clear_glyphs(&mut self) {
        self.glyphs.clear();
        self.mask_shelves = Shelves::new();
        self.color_shelves = Shelves::new();
    }

    fn glyph(&mut self, fonts: &mut Fonts, g: GlyphRef) -> Option<AtlasGlyph> {
        if let Some(&a) = self.glyphs.get(&g) {
            return a;
        }
        let entry = fonts.rasterize(g).and_then(|r| {
            let shelves = if r.color { &mut self.color_shelves } else { &mut self.mask_shelves };
            let (x, y) = match shelves.alloc(r.width, r.height) {
                Some(p) => p,
                None => {
                    // Atlas full: start over; glyphs re-rasterize as needed.
                    *shelves = Shelves::new();
                    self.redraw_needed = true;
                    self.glyphs.retain(|_, v| v.is_none_or(|a| a.color != r.color));
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
            let a = ATLAS as f32;
            Some(AtlasGlyph {
                color: r.color,
                uv: [x as f32 / a, y as f32 / a, (x + r.width) as f32 / a, (y + r.height) as f32 / a],
                left: r.left,
                top: r.top,
                w: r.width,
                h: r.height,
            })
        });
        self.glyphs.insert(g, entry);
        entry
    }

    /// Render one frame. Returns false if the surface was not available
    /// (the caller should retry on the next frame).
    pub fn draw(&mut self, term: &Terminal, fonts: &mut Fonts, info: &FrameInfo) -> bool {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame.texture.create_view(&Default::default());
        let enc = self.encode(term, fonts, info, &view);
        self.queue.submit([enc]);
        self.queue.present(frame);
        true
    }

    /// Render the current state offscreen and return it as RGBA8.
    pub fn snapshot(&mut self, term: &Terminal, fonts: &mut Fonts, info: &FrameInfo) -> (u32, u32, Vec<u8>) {
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
        let draw = self.encode(term, fonts, info, &view);
        let stride = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("snapshot readback"),
            size: (stride * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
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
            for px in row[..(w * 4) as usize].chunks_exact(4) {
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
    fn encode(&mut self, term: &Terminal, fonts: &mut Fonts, info: &FrameInfo, view: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let m = fonts.metrics();
        let bg_default = term.palette().background();
        self.scratch.clear();
        self.fg.clear();
        self.build(term, fonts, info, m);
        let mut all = std::mem::take(&mut self.scratch);
        all.append(&mut self.fg);
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
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: c[0], g: c[1], b: c[2], a: 1.0 }),
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
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..4, 0..all.len() as u32);
            }
        }
        all.clear();
        self.scratch = all;
        enc.finish()
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) {
        self.scratch.push(Instance { pos: [x, y], size: [w, h], uv: [0.0; 4], color: c, kind: SOLID });
    }

    /// Build all instances for the viewport.
    fn build(&mut self, t: &Terminal, fonts: &mut Fonts, info: &FrameInfo, m: Metrics) {
        let pal = t.palette();
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let (px, py) = (info.pad.0 as f32, info.pad.1 as f32);
        let default_bg = pal.background();
        let reverse = t.mode().contains(TermMode::REVERSE_VIDEO);
        let selection = t.selection_range();
        let cursor = t.cursor();
        let show_cursor = t.cursor_visible() && !info.cursor_blink_off && info.preedit.is_none();
        let grid = t.grid();
        for y in 0..t.lines() {
            let line = grid.viewport_line(y);
            let row = grid.viewport_row(y);
            let top = py + y as f32 * ch;
            let mut run = Run::default();
            let mut cell_buf = String::new();
            // Run-length merge of backgrounds on this row.
            let mut bg_run: Option<(usize, Rgb)> = None;
            let mut x = 0;
            while x < row.cells.len() {
                let cell = &row.cells[x];
                let width = if cell.flags.contains(CellFlags::WIDE) { 2 } else { 1 };
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    x += 1;
                    continue;
                }
                let style = t.style(cell.style);
                let selected = selection.is_some_and(|r| r.contains(Point::new(line, x)));
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
                let is_cursor = show_cursor && y == cursor.y && x == cursor.x && info.focused;
                let is_cursor_block = is_cursor && t.cursor_shape() == CursorShape::Block;
                if is_cursor_block {
                    let cc = pal.cursor().unwrap_or(fg);
                    fg = bg;
                    bg = cc;
                }
                let alpha = if style.attrs.contains(Attrs::DIM) { 0.6 } else { 1.0 };

                // Backgrounds.
                let cell_bg = if bg == default_bg { None } else { Some(bg) };
                match (bg_run, cell_bg) {
                    (Some((_, c)), Some(b)) if c == b => {}
                    _ => {
                        if let Some((start, c)) = bg_run.take() {
                            self.rect(px + start as f32 * cw, top, (x - start) as f32 * cw, ch, rgba(c, 1.0));
                        }
                        bg_run = cell_bg.map(|b| (x, b));
                    }
                }

                // Glyph. Single-cell clusters the primary face covers are
                // collected into runs and shaped together (ligatures, mark
                // positioning); other clusters (wide, emoji, fallback fonts)
                // are shaped on their own.
                let hidden = style.attrs.contains(Attrs::HIDDEN);
                let blank = cell.ch == ' ' || cell.flags.contains(CellFlags::LEADING_WIDE_SPACER);
                let color = rgba(fg, alpha);
                let st = (style.attrs.contains(Attrs::BOLD) as u8 * BOLD) | (style.attrs.contains(Attrs::ITALIC) as u8 * ITALIC);
                let drawable = !hidden && !blank && cell.ch != proteus_core::graphics::placeholder::PLACEHOLDER;
                let text = cell_text(cell, &mut cell_buf);
                let wide = width == 2;
                let emoji = drawable && font::emoji_presentation(text, wide);
                let shapable = drawable && !wide && !emoji && fonts.in_primary(text, st);
                let script = font::script_tag(text);
                if !(shapable && info.ligatures && run.continues(x, st, color, script)) {
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
                    }
                    run.script = run.script.or(script);
                    run.starts.push(run.text.len() as u32);
                    run.text.push_str(text);
                } else if drawable {
                    let left = px + x as f32 * cw;
                    let box_w = width as f32 * cw;
                    if cell.extra.is_none() && !emoji {
                        if let Some(g) = fonts.glyph(cell.ch, st) {
                            self.place_glyph(fonts, g, left, top, box_w, 0.0, 0.0, m, color);
                        }
                    } else {
                        let glyphs = fonts.shape_cluster(text, st, wide);
                        for g in glyphs.iter() {
                            self.place_glyph(fonts, g.glyph, left, top, box_w, g.x, g.y, m, color);
                        }
                    }
                }

                // Decorations.
                let w = width as f32 * cw;
                let left = px + x as f32 * cw;
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
                if show_cursor && y == cursor.y && x == cursor.x && !is_cursor_block {
                    let cc = pal.cursor().unwrap_or(pal.resolve(style.fg, true));
                    let c = rgba(cc, 1.0);
                    let thick = (cw / 8.0).round().max(1.0);
                    if !info.focused {
                        self.fg.push(solid(left, top, w, thick, c));
                        self.fg.push(solid(left, top + ch - thick, w, thick, c));
                        self.fg.push(solid(left, top, thick, ch, c));
                        self.fg.push(solid(left + w - thick, top, thick, ch, c));
                    } else if t.cursor_shape() == CursorShape::Bar {
                        self.fg.push(solid(left, top, thick, ch, c));
                    } else {
                        self.fg.push(solid(left, top + ch - thick, w, thick, c));
                    }
                }
                x += width;
            }
            self.flush_run(fonts, &mut run, m, cw);
            if let Some((start, c)) = bg_run {
                self.rect(px + start as f32 * cw, top, (row.cells.len() - start) as f32 * cw, ch, rgba(c, 1.0));
            }
        }
        if let Some(p) = &info.preedit {
            self.build_preedit(t, fonts, p, info, m);
        }
        if let Some(o) = &info.overlay {
            self.build_overlay(t, fonts, o, info, m);
        }
    }

    /// Message bar over the last rows, in inverted colors with a top rule.
    fn build_overlay(&mut self, t: &Terminal, fonts: &mut Fonts, o: &Overlay, info: &FrameInfo, m: Metrics) {
        let pal = t.palette();
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let (px, py) = (info.pad.0 as f32, info.pad.1 as f32);
        let (fg, bg) = (pal.background(), pal.foreground());
        let mut lines: Vec<(&str, f32)> = vec![(o.title.as_str(), 1.0)];
        if let Some(d) = &o.detail {
            lines.push((d.as_str(), 0.7));
        }
        lines.push((o.keys.as_str(), 0.85));
        let rows = lines.len().min(t.lines());
        let first = t.lines() - rows;
        let top = py + first as f32 * ch;
        // Cover the padding too so the bar reaches the window edges.
        let width = self.config.width as f32;
        let height = self.config.height as f32 - top;
        self.fg.push(solid(0.0, top, width, height, rgba(bg, 1.0)));
        self.fg.push(solid(0.0, top, width, (m.underline_h as f32).max(1.0), rgba(pal.indexed(4), 1.0)));
        for (i, (text, alpha)) in lines.into_iter().take(rows).enumerate() {
            let y = top + i as f32 * ch;
            let style = if i == 0 { BOLD } else { 0 };
            self.text_line(fonts, text, px, y, t.cols(), style, rgba(fg, alpha), m, cw);
        }
    }

    /// Lay out `text` grapheme by grapheme on the grid, truncated with `…`
    /// to `cols` columns.
    #[allow(clippy::too_many_arguments)]
    fn text_line(&mut self, fonts: &mut Fonts, text: &str, left: f32, top: f32, cols: usize, style: u8, color: [f32; 4], m: Metrics, cw: f32) {
        use unicode_segmentation::UnicodeSegmentation;
        use unicode_width::UnicodeWidthStr;
        let mut col = 0;
        let graphemes: Vec<&str> = text.graphemes(true).collect();
        let total: usize = graphemes.iter().map(|g| g.width().clamp(1, 2)).sum();
        let limit = if total > cols { cols.saturating_sub(1) } else { cols };
        let mut full = false;
        for g in graphemes.iter().copied().chain((total > cols).then_some("…")) {
            let w = g.width().clamp(1, 2);
            if g != "…" && (full || col + w > limit) {
                full = true;
                continue;
            }
            if g != " " {
                let x = left + col as f32 * cw;
                for sg in fonts.shape_cluster(g, style, w == 2).iter() {
                    self.place_glyph(fonts, sg.glyph, x, top, w as f32 * cw, sg.x, sg.y, m, color);
                }
            }
            col += w;
        }
    }

    /// Draw the input method's composition over the grid at the cursor:
    /// opaque background, underline, thicker underline under the selected
    /// segment, and a bar cursor.
    fn build_preedit(&mut self, t: &Terminal, fonts: &mut Fonts, p: &Preedit, info: &FrameInfo, m: Metrics) {
        let pal = t.palette();
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let (px, py) = (info.pad.0 as f32, info.pad.1 as f32);
        let cursor = t.cursor();
        let row = cursor.y + t.grid().display_offset();
        if row >= t.lines() {
            return;
        }
        let start = preedit_start(cursor.x, p.width(), t.cols());
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
        for c in &cells {
            if start + c.col + c.width > t.cols() {
                break;
            }
            let x = left + c.col as f32 * cw;
            let box_w = c.width as f32 * cw;
            let glyphs = fonts.shape_cluster(c.text, 0, c.width == 2);
            for g in glyphs.iter() {
                self.place_glyph(fonts, g.glyph, x, top, box_w, g.x, g.y, m, fg);
            }
            if let Some((b, e)) = p.cursor
                && b < e
                && c.bytes.0 < e
                && c.bytes.1 > b
            {
                // Segment being converted.
                let thick = (th * 2.0).max(2.0);
                self.fg.push(solid(x, (uy + th - thick).max(top), box_w, thick, fg));
            }
        }
        if let Some(col) = p.cursor_col().filter(|_| p.cursor.is_some_and(|(b, e)| b == e)) {
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
        let glyphs = fonts.shape(&run.text, run.style);
        for g in glyphs.iter() {
            // Cell whose text contains the glyph's cluster start.
            let cell = run.starts.partition_point(|&s| s <= g.src).saturating_sub(1);
            let left = run.left + (run.start + cell) as f32 * cw;
            self.place_glyph(fonts, g.glyph, left, run.top, cw, g.x, g.y, m, run.color);
        }
        run.text.clear();
        run.starts.clear();
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
        let Some(a) = self.glyph(fonts, g) else { return };
        let (mut w, mut h) = (a.w as f32, a.h as f32);
        let mut x = (left + dx).round() + a.left as f32;
        let mut y = (top + m.baseline as f32 - dy).round() - a.top as f32;
        if a.color {
            // Fit color glyphs (emoji bitmaps) into their cell box.
            let ch = m.cell_h as f32;
            let s = (box_w / w).min(ch / h).min(1.0);
            if s < 1.0 {
                w *= s;
                h *= s;
            }
            x = left + (box_w - w) / 2.0;
            y = y.clamp(top, top + ch - h);
        }
        self.fg.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv: a.uv,
            color,
            kind: if a.color { COLOR } else { MASK },
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
}

impl Run {
    fn continues(&self, x: usize, style: u8, color: [f32; 4], script: Option<u32>) -> bool {
        self.text.is_empty()
            || (self.style == style
                && self.color == color
                && self.start + self.starts.len() == x
                && (script.is_none() || self.script.is_none() || script == self.script))
    }
}

/// A cell's full text: the base char plus combining/joined extras.
fn cell_text<'a>(cell: &'a proteus_core::cell::Cell, buf: &'a mut String) -> &'a str {
    match &cell.extra {
        None => {
            buf.clear();
            buf.push(cell.ch);
            buf
        }
        Some(extra) => {
            buf.clear();
            buf.push(cell.ch);
            buf.push_str(extra);
            buf
        }
    }
}

fn solid(x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) -> Instance {
    Instance { pos: [x, y], size: [w, h], uv: [0.0; 4], color: c, kind: SOLID }
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
        assert_eq!(std::mem::size_of::<Instance>(), 52);
    }

    #[test]
    fn shelf_packing() {
        let mut s = Shelves::new();
        assert_eq!(s.alloc(10, 20), Some((0, 0)));
        assert_eq!(s.alloc(10, 18), Some((11, 0)));
        // Much shorter glyphs open a new shelf.
        assert_eq!(s.alloc(10, 5), Some((0, 21)));
        assert_eq!(s.alloc(ATLAS + 1, 1), None);
        let mut full = Shelves::new();
        while full.alloc(1000, 1000).is_some() {}
        assert!(full.alloc(1000, 1000).is_none());
    }
}
