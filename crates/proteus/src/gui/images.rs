//! Inline images: where each image of a pane goes on screen, and the GPU
//! textures holding them.
//!
//! The core hands out [`ImageView`]s in viewport pixels; [`place`] fits them
//! to what the pane really shows (folded blocks, rows hidden under the
//! bottom bar) and clips them to the grid, so splits, floats and zoom need
//! nothing special. [`TextureCache`] uploads pixels when an image (or its
//! animation frame) changes and drops the least recently drawn textures
//! when over budget; the core keeps the pixels, so they can be uploaded again.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use proteus_core::ImageView;
use proteus_core::Line;

use super::rows::{Row, Rows};

/// Textures not drawn for this long are freed even under budget.
const IDLE: Duration = Duration::from_secs(60);

/// Stacking of an image relative to the text of its pane (kitty semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Under cells with a non-default background (`z < i32::MIN / 2`).
    UnderBackground,
    /// Over backgrounds, under text (`z < 0`).
    UnderText,
    /// Over text.
    OverText,
}

impl Layer {
    pub fn of(z: i32) -> Layer {
        if z < i32::MIN / 2 {
            Layer::UnderBackground
        } else if z < 0 {
            Layer::UnderText
        } else {
            Layer::OverText
        }
    }
}

/// Fit the views of a pane to the rows it shows. Input destinations are in
/// viewport pixels for the terminal's cell size `tcell`; output ones are in
/// pixels for the renderer's cell size `cell`, relative to viewport row 0,
/// clipped to the grid below the `skip` hidden rows. With folds, an image
/// moves with the row of its first visible line, and disappears when that
/// line is folded away.
pub fn place<'a>(
    views: Vec<ImageView<'a>>,
    rows: &Rows,
    top: Line,
    cols: usize,
    cell: (u32, u32),
    tcell: (u32, u32),
    skip: usize,
) -> Vec<ImageView<'a>> {
    let lines = rows.0.len();
    let (cw, ch) = (cell.0 as i64, cell.1 as i64);
    let (tcw, tch) = (tcell.0.max(1) as i64, tcell.1.max(1) as i64);
    let identity = rows.0.iter().enumerate().all(|(y, r)| *r == Row::Line(top + y as Line));
    let (x0, y0, x1, y1) = (0, skip as i64 * ch, cols as i64 * cw, lines as i64 * ch);
    let mut out = Vec::with_capacity(views.len());
    for mut v in views {
        if (cw, ch) != (tcw, tch) {
            let d = v.dest;
            let (l, t) = (d.x * cw / tcw, d.y * ch / tch);
            let (r, b) = ((d.x + d.w) * cw / tcw, (d.y + d.h) * ch / tch);
            v.dest = proteus_core::graphics::DestRect { x: l, y: t, w: r - l, h: b - t };
        }
        if !identity {
            // Viewport row of the first visible line of the image.
            let first = v.dest.y.div_euclid(ch).max(0);
            if first >= lines as i64 {
                continue;
            }
            let line = top + first as Line;
            match rows.row_of(line).map(|r| (r, rows.0[r])) {
                Some((r, Row::Line(_))) => v.dest.y += (r as i64 - first) * ch,
                _ => continue,
            }
        }
        if let Some(c) = v.clip(x0, y0, x1, y1) {
            out.push(c);
        }
    }
    out
}

/// Texture coordinates of a view's source rectangle.
pub fn uv(v: &ImageView) -> [f32; 4] {
    let (w, h) = (v.width.max(1) as f32, v.height.max(1) as f32);
    [v.src.x / w, v.src.y / h, (v.src.x + v.src.w) / w, (v.src.y + v.src.h) / h]
}

struct Gpu {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    generation: u64,
    /// Texture size (smaller than the image if it exceeded the GPU limit).
    size: (u32, u32),
    bytes: usize,
    used: Instant,
}

/// GPU textures of images, keyed by the core's never-reused image key.
pub struct TextureCache {
    layout: wgpu::BindGroupLayout,
    /// Bound for draws that sample no image.
    pub empty: wgpu::BindGroup,
    textures: HashMap<u64, Gpu>,
    /// Image keys used in the frame being built; slot `n` is `frame[n - 1]`.
    frame: Vec<u64>,
    bytes: usize,
    pub budget: usize,
    max_dim: u32,
}

impl TextureCache {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, budget: usize) -> TextureCache {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        // The bind group keeps the texture alive.
        let (tex, empty) = Self::texture(device, &layout, (1, 1));
        Self::write(queue, &tex, (1, 1), &[0; 4]);
        TextureCache {
            layout,
            empty,
            textures: HashMap::new(),
            frame: Vec::new(),
            bytes: 0,
            budget,
            max_dim: device.limits().max_texture_dimension_2d.max(1),
        }
    }

    pub fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    fn texture(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        (w, h): (u32, u32),
    ) -> (wgpu::Texture, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
        });
        (texture, bind)
    }

    fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, (w, h): (u32, u32), pixels: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Start building a frame.
    pub fn begin(&mut self) {
        self.frame.clear();
    }

    /// Slot (≥ 1) of an image with up-to-date pixels, uploading them if needed.
    /// `None` if the image has no pixels (evicted, or malformed).
    pub fn slot(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: u64,
        generation: u64,
        (w, h): (u32, u32),
        pixels: Option<&[u8]>,
    ) -> Option<u32> {
        if let Some(i) = self.frame.iter().position(|&k| k == key) {
            return Some(i as u32 + 1);
        }
        let fresh = self.textures.get(&key).is_some_and(|t| t.generation == generation);
        if !fresh {
            let pixels = pixels.filter(|p| w > 0 && h > 0 && p.len() == w as usize * h as usize * 4)?;
            let scaled;
            let (size, data) = if w > self.max_dim || h > self.max_dim {
                // Larger than the GPU allows: upload a scaled-down copy.
                let s = self.max_dim as f64 / w.max(h) as f64;
                let size = (((w as f64 * s) as u32).max(1), ((h as f64 * s) as u32).max(1));
                let img = image::RgbaImage::from_raw(w, h, pixels.to_vec())?;
                scaled = image::imageops::resize(&img, size.0, size.1, image::imageops::FilterType::Triangle);
                (size, scaled.as_raw().as_slice())
            } else {
                ((w, h), pixels)
            };
            match self.textures.get_mut(&key) {
                Some(t) if t.size == size => {
                    Self::write(queue, &t.texture, size, data);
                    t.generation = generation;
                }
                _ => {
                    let (texture, bind) = Self::texture(device, &self.layout, size);
                    Self::write(queue, &texture, size, data);
                    let bytes = size.0 as usize * size.1 as usize * 4;
                    self.bytes += bytes;
                    if let Some(old) =
                        self.textures.insert(key, Gpu { texture, bind, generation, size, bytes, used: Instant::now() })
                    {
                        self.bytes -= old.bytes;
                    }
                }
            }
        }
        if let Some(t) = self.textures.get_mut(&key) {
            t.used = Instant::now();
        }
        self.frame.push(key);
        Some(self.frame.len() as u32)
    }

    /// Bind group of a slot.
    pub fn bind(&self, slot: u32) -> &wgpu::BindGroup {
        slot.checked_sub(1)
            .and_then(|i| self.frame.get(i as usize))
            .and_then(|k| self.textures.get(k))
            .map_or(&self.empty, |t| &t.bind)
    }

    /// After a frame: free idle textures, and the least recently drawn ones
    /// not in this frame while over budget.
    pub fn trim(&mut self) {
        let now = Instant::now();
        let mut idle: Vec<(Instant, u64)> =
            self.textures.iter().filter(|(k, _)| !self.frame.contains(k)).map(|(k, t)| (t.used, *k)).collect();
        idle.sort();
        for (used, key) in idle {
            if self.bytes <= self.budget && now.duration_since(used) < IDLE {
                continue;
            }
            if let Some(t) = self.textures.remove(&key) {
                self.bytes -= t.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proteus_core::Terminal;

    /// A terminal showing a 16x32 kitty image (2x2 cells at 8x16) at row 1, col 1.
    fn term() -> Terminal {
        let mut t = Terminal::new(10, 5, proteus_core::Config::default());
        t.set_cell_pixel_size(8, 16);
        let rgba = vec![255u8; 16 * 32 * 4];
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(&rgba);
        t.advance(b"\r\n x");
        t.advance(b"\x1b[2;2H");
        t.advance(format!("\x1b_Ga=T,f=32,s=16,v=32,i=1,q=2;{data}\x1b\\").as_bytes());
        t
    }

    #[test]
    fn layers() {
        assert_eq!(Layer::of(0), Layer::OverText);
        assert_eq!(Layer::of(-1), Layer::UnderText);
        assert_eq!(Layer::of(i32::MIN / 2 - 1), Layer::UnderBackground);
        assert_eq!(Layer::of(i32::MIN / 2), Layer::UnderText);
    }

    #[test]
    fn placed_in_viewport_pixels() {
        let t = term();
        let rows = Rows::of(&t);
        let top = t.grid().viewport_line(0);
        let v = place(t.visible_images(), &rows, top, t.cols(), (8, 16), (8, 16), 0);
        assert_eq!(v.len(), 1);
        let d = v[0].dest;
        assert_eq!((d.x, d.y, d.w, d.h), (8, 16, 16, 32));
        assert_eq!(uv(&v[0]), [0.0, 0.0, 1.0, 1.0]);
        // Another cell size scales the destination.
        let v = place(t.visible_images(), &rows, top, t.cols(), (16, 32), (8, 16), 0);
        let d = v[0].dest;
        assert_eq!((d.x, d.y, d.w, d.h), (16, 32, 32, 64));
    }

    #[test]
    fn clipped_to_grid_and_skipped_rows() {
        let t = term();
        let rows = Rows::of(&t);
        let top = t.grid().viewport_line(0);
        // Grid 2 columns wide: the image's right half is cut off.
        let v = place(t.visible_images(), &rows, top, 2, (8, 16), (8, 16), 0);
        let d = v[0].dest;
        assert_eq!((d.x, d.w), (8, 8));
        assert_eq!(uv(&v[0])[2], 0.5);
        // Two rows hidden at the top: the image's first row is hidden.
        let v = place(t.visible_images(), &rows, top, 10, (8, 16), (8, 16), 2);
        let d = v[0].dest;
        assert_eq!((d.y, d.h), (32, 16));
        assert_eq!(uv(&v[0])[1], 0.5);
    }

    #[test]
    fn follows_folds() {
        let t = term();
        let top = t.grid().viewport_line(0);
        // Fake fold of line 0: the image's row moves up by one.
        let mut rows = Rows::of(&t);
        rows.0.remove(0);
        rows.0.push(Row::Empty);
        let v = place(t.visible_images(), &rows, top, 10, (8, 16), (8, 16), 0);
        assert_eq!(v[0].dest.y, 0);
        // Its line folded away: gone.
        let mut rows = Rows::of(&t);
        rows.0[1] = Row::Fold { block: Default::default(), start: top + 1, end: top + 2 };
        assert!(place(t.visible_images(), &rows, top, 10, (8, 16), (8, 16), 0).is_empty());
    }
}
