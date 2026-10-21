//! Inline graphics: the per-screen image store, placements anchored to grid
//! cells, quotas, and what the renderer needs to composite images.
//!
//! Protocol front ends (kitty graphics, Sixel, iTerm2 `File=`) live in
//! `term/graphics.rs` and feed this store. Placements are anchored to an
//! absolute grid line, so they scroll with the text, follow reflow and are
//! dropped together with the scrollback they belong to.

mod decode;
pub(crate) mod kitty;
pub(crate) mod media;
pub mod placeholder;
pub(crate) mod sixel;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

pub(crate) use decode::run_in_background;
pub use decode::{DecodeError, DecodedImage, decode_image, image_dimensions};
pub use image::ImageFormat;

use crate::grid::{Grid, LineMap};
use crate::style::StyleTable;

pub const MIB: usize = 1 << 20;
/// Cell size assumed for pixel geometry until the host reports the real one.
pub const FALLBACK_CELL_PX: (u32, u32) = (8, 16);
/// Maximum chain length of relative placements.
pub const MAX_RELATIVE_DEPTH: usize = 8;

/// Graphics policy and resource limits.
#[derive(Debug, Clone)]
pub struct GraphicsConfig {
    /// Accept image protocols at all.
    pub enabled: bool,
    /// Largest single decoded image (all frames).
    pub max_image_bytes: usize,
    /// Image memory per pane (both screens).
    pub pane_budget: usize,
    /// Shared image memory budget for all panes of a window.
    pub window_budget: Option<Arc<ImageBudget>>,
    /// Allow kitty file / temp-file / shared-memory transmission for local panes.
    pub allow_local_files: bool,
    /// This machine's hostname; an OSC 7 cwd on another host makes the pane remote.
    pub hostname: Option<String>,
    /// Decode PNG/JPEG/GIF/WebP on background threads and call this when a
    /// result is ready (then call [`crate::Terminal::advance`], with no bytes
    /// if need be). `None`: decode synchronously while parsing.
    pub waker: Option<DecodeWaker>,
}

/// Called from a decoder thread when a decoded image is ready.
#[derive(Clone)]
pub struct DecodeWaker(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for DecodeWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecodeWaker")
    }
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_bytes: 64 * MIB,
            pane_budget: 256 * MIB,
            window_budget: None,
            allow_local_files: true,
            hostname: None,
            waker: None,
        }
    }
}

/// A memory budget shared between image stores (e.g. all panes of a window).
#[derive(Debug)]
pub struct ImageBudget {
    limit: usize,
    used: AtomicUsize,
}

impl ImageBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { limit, used: AtomicUsize::new(0) })
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    fn try_reserve(&self, n: usize) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |u| u.checked_add(n).filter(|&t| t <= self.limit))
            .is_ok()
    }
    fn release(&self, n: usize) {
        let _ = self.used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |u| Some(u.saturating_sub(n)));
    }
}

/// Unique image keys across all stores (texture-cache keys for renderers).
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
/// Logical clock for least-recently-displayed eviction.
static CLOCK: AtomicU64 = AtomicU64::new(1);

fn now_tick() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// Which protocol created an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Kitty,
    Sixel,
    Iterm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationState {
    Stopped,
    /// Running, but waits at the last frame for more frames.
    Loading,
    Running,
}

#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub pixels: Vec<u8>,
    /// Milliseconds before the next frame; negative = gapless (skipped).
    pub gap: i32,
}

/// Default gap of frames added after the root frame.
pub(crate) const DEFAULT_GAP: i32 = 40;

/// A stored image (possibly animated).
#[derive(Debug)]
pub struct Image {
    pub(crate) key: u64,
    pub(crate) id: u32,
    pub(crate) number: u32,
    pub(crate) source: Source,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frames: Vec<Frame>,
    pub(crate) current: usize,
    pub(crate) state: AnimationState,
    /// Extra passes after the first (`None` = loop forever).
    pub(crate) loops: Option<u32>,
    pub(crate) loops_done: u32,
    pub(crate) shown_at: Option<Instant>,
    pub(crate) generation: u64,
    last_used: AtomicU64,
    pub(crate) evicted: bool,
    /// Pixels are still being decoded (bytes are reserved already).
    pub(crate) loading: bool,
}

impl Image {
    pub(crate) fn new(id: u32, number: u32, source: Source, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            id,
            number,
            source,
            width,
            height,
            frames: vec![Frame { pixels: rgba, gap: 0 }],
            current: 0,
            state: AnimationState::Stopped,
            loops: None,
            loops_done: 0,
            shown_at: None,
            generation: 1,
            last_used: AtomicU64::new(now_tick()),
            evicted: false,
            loading: false,
        }
    }

    /// An image whose single frame is still being decoded.
    pub(crate) fn loading(id: u32, number: u32, source: Source, width: u32, height: u32) -> Self {
        let mut img = Self::new(id, number, source, width, height, Vec::new());
        img.loading = true;
        img
    }

    /// Unique, never reused key (use it to cache textures).
    pub fn key(&self) -> u64 {
        self.key
    }
    /// Kitty image id (0 for anonymous images, including Sixel and iTerm2).
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn number(&self) -> u32 {
        self.number
    }
    pub fn source(&self) -> Source {
        self.source
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
    pub fn current_frame(&self) -> usize {
        self.current
    }
    pub fn animation_state(&self) -> AnimationState {
        self.state
    }
    /// Changes whenever the displayed pixels change.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Pixel data was dropped to stay within quota; placements show a placeholder.
    pub fn is_evicted(&self) -> bool {
        self.evicted
    }
    /// Still being decoded: nothing to show yet.
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    /// RGBA8 pixels of the current frame.
    pub fn pixels(&self) -> Option<&[u8]> {
        if self.evicted || self.loading { None } else { self.frames.get(self.current).map(|f| &f.pixels[..]) }
    }
    pub(crate) fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
    pub(crate) fn bytes(&self) -> usize {
        if self.evicted { 0 } else { self.frames.len() * self.frame_bytes() }
    }
    fn touch(&self) {
        self.last_used.store(now_tick(), Ordering::Relaxed);
    }
}

/// Rectangle in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// How a placement maps its source rectangle onto the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// One image pixel per screen pixel.
    Natural,
    /// Scaled to fit the cell box, aspect preserved, centered.
    Contain,
    /// Scaled to fill the cell box.
    Stretch,
    /// Exact size in screen pixels.
    Pixels(u32, u32),
}

/// One display of an image, anchored to a grid cell.
#[derive(Debug, Clone)]
pub struct Placement {
    pub(crate) image: u64,
    pub(crate) id: u32,
    /// Absolute grid line of the top-left cell (may be negative after
    /// scrolling inside a region on a screen without scrollback).
    pub(crate) line: i64,
    pub(crate) col: usize,
    /// Pixel offset within the anchor cell.
    pub(crate) offset: (u32, u32),
    pub(crate) src: PxRect,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    pub(crate) fit: Fit,
    pub(crate) z: i32,
    /// Kitty virtual placement (shown through Unicode placeholders only).
    pub(crate) is_virtual: bool,
    /// Relative placement: parent (image key, placement id) and cell offset.
    pub(crate) parent: Option<(u64, u32)>,
    pub(crate) rel: (i32, i32),
    /// Visible absolute line range after being scrolled partly out of a region.
    pub(crate) clip: Option<(i64, i64)>,
    pub(crate) seq: u64,
}

impl Placement {
    pub(crate) fn new(image: u64, line: i64, col: usize, src: PxRect, cols: u32, rows: u32, fit: Fit) -> Self {
        Self {
            image,
            id: 0,
            line,
            col,
            offset: (0, 0),
            src,
            cols: cols.max(1),
            rows: rows.max(1),
            fit,
            z: 0,
            is_virtual: false,
            parent: None,
            rel: (0, 0),
            clip: None,
            seq: 0,
        }
    }
    pub fn image_key(&self) -> u64 {
        self.image
    }
    pub fn id(&self) -> u32 {
        self.id
    }
    /// Anchor: absolute line and column (meaningless for relative placements).
    pub fn anchor(&self) -> (i64, usize) {
        (self.line, self.col)
    }
    /// Size of the placement in cells.
    pub fn cells(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }
    pub fn z(&self) -> i32 {
        self.z
    }
    pub fn is_virtual(&self) -> bool {
        self.is_virtual
    }
    pub fn is_relative(&self) -> bool {
        self.parent.is_some()
    }
}

/// Source rectangle in image pixels (fractional after clipping).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SrcRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Destination rectangle in viewport pixels; (0, 0) is the top-left corner of
/// viewport cell (0, 0). May extend beyond the viewport: scissor to the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestRect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// Something the renderer must draw: part of an image at a viewport position.
#[derive(Debug, Clone, Copy)]
pub struct ImageView<'a> {
    pub key: u64,
    pub id: u32,
    pub placement: u32,
    pub generation: u64,
    /// Full image size in pixels.
    pub width: u32,
    pub height: u32,
    /// Current frame (RGBA8). `None` if evicted (draw an "image evicted"
    /// placeholder) or still loading (draw nothing).
    pub pixels: Option<&'a [u8]>,
    pub loading: bool,
    pub src: SrcRect,
    pub dest: DestRect,
    /// Negative: below text; below `i32::MIN / 2`: below non-default cell backgrounds.
    pub z: i32,
    /// Shown through Unicode placeholder cells.
    pub from_placeholder: bool,
}

impl<'a> ImageView<'a> {
    /// Crop to the destination rectangle `[x0, x1) x [y0, y1)`, adjusting the
    /// source proportionally. `None` if nothing remains.
    pub fn clip(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> Option<ImageView<'a>> {
        let d = self.dest;
        if d.w <= 0 || d.h <= 0 {
            return None;
        }
        let (cx0, cy0) = (d.x.max(x0), d.y.max(y0));
        let (cx1, cy1) = ((d.x + d.w).min(x1), (d.y + d.h).min(y1));
        if cx0 >= cx1 || cy0 >= cy1 {
            return None;
        }
        let sx = self.src.w / d.w as f32;
        let sy = self.src.h / d.h as f32;
        let mut v = *self;
        v.src = SrcRect {
            x: self.src.x + (cx0 - d.x) as f32 * sx,
            y: self.src.y + (cy0 - d.y) as f32 * sy,
            w: (cx1 - cx0) as f32 * sx,
            h: (cy1 - cy0) as f32 * sy,
        };
        v.dest = DestRect { x: cx0, y: cy0, w: cx1 - cx0, h: cy1 - cy0 };
        Some(v)
    }
}

/// Fit `sw` x `sh` into `bw` x `bh` preserving aspect ratio, centered.
fn contain(sw: i64, sh: i64, bw: i64, bh: i64) -> (i64, i64, i64, i64) {
    if sw <= 0 || sh <= 0 {
        return (0, 0, 0, 0);
    }
    if sw * bh <= sh * bw {
        let w = (sw * bh / sh).max(1);
        ((bw - w) / 2, 0, w, bh)
    } else {
        let h = (sh * bw / sw).max(1);
        (0, (bh - h) / 2, bw, h)
    }
}

/// Pixel rectangle covered by a placement, relative to its anchor cell origin.
fn placement_rect(p: &Placement, cw: i64, ch: i64) -> (i64, i64, i64, i64) {
    let (ox, oy) = (p.offset.0 as i64, p.offset.1 as i64);
    let (sw, sh) = (p.src.w as i64, p.src.h as i64);
    let (bw, bh) = (p.cols as i64 * cw, p.rows as i64 * ch);
    match p.fit {
        Fit::Natural => (ox, oy, sw, sh),
        Fit::Pixels(w, h) => (ox, oy, w as i64, h as i64),
        Fit::Stretch => (ox, oy, bw, bh),
        Fit::Contain => {
            let (x, y, w, h) = contain(sw, sh, bw, bh);
            (ox + x, oy + y, w, h)
        }
    }
}

/// An in-progress chunked kitty transmission.
#[derive(Debug)]
pub(crate) struct Upload {
    pub cmd: kitty::Command,
    pub data: Vec<u8>,
}

/// Images and placements of one screen (primary or alternate).
#[derive(Debug)]
pub struct ImageStore {
    images: HashMap<u64, Image>,
    by_id: HashMap<u32, u64>,
    placements: Vec<Placement>,
    bytes: usize,
    budget: Option<Arc<ImageBudget>>,
    next_id: u32,
    seq: u64,
    pub(crate) upload: Option<Upload>,
}

impl Drop for ImageStore {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.release(self.bytes);
        }
    }
}

impl ImageStore {
    pub(crate) fn new(budget: Option<Arc<ImageBudget>>) -> Self {
        Self {
            images: HashMap::new(),
            by_id: HashMap::new(),
            placements: Vec::new(),
            bytes: 0,
            budget,
            next_id: 0x7000_0000,
            seq: 0,
            upload: None,
        }
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
    /// Decoded bytes held (excluding evicted images).
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn images(&self) -> impl Iterator<Item = &Image> {
        self.images.values()
    }
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }
    pub fn image(&self, key: u64) -> Option<&Image> {
        self.images.get(&key)
    }
    /// Image by kitty id.
    pub fn image_by_id(&self, id: u32) -> Option<&Image> {
        self.by_id.get(&id).and_then(|k| self.images.get(k))
    }

    pub(crate) fn image_mut(&mut self, key: u64) -> Option<&mut Image> {
        self.images.get_mut(&key)
    }
    pub(crate) fn images_mut(&mut self) -> impl Iterator<Item = &mut Image> {
        self.images.values_mut()
    }
    pub(crate) fn key_for_id(&self, id: u32) -> Option<u64> {
        if id == 0 { None } else { self.by_id.get(&id).copied() }
    }
    pub(crate) fn newest_with_number(&self, n: u32) -> Option<u64> {
        self.images.values().filter(|i| i.number == n && i.source == Source::Kitty).map(|i| i.key).max()
    }
    /// A free kitty image id (for `I=` transmissions).
    pub(crate) fn alloc_id(&mut self) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.by_id.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
    }

    fn has_placements(&self, key: u64) -> bool {
        self.placements.iter().any(|p| p.image == key)
    }

    /// Make room for `need` more bytes, given `other` bytes held by the pane's
    /// other screen. Unplaced images go first, then the least recently
    /// displayed images lose their pixels (placements stay, shown as evicted).
    /// On success the bytes are accounted.
    pub(crate) fn reserve(
        &mut self,
        need: usize,
        other: usize,
        cfg: &GraphicsConfig,
        protect: Option<u64>,
    ) -> Result<(), kitty::Error> {
        if need > cfg.max_image_bytes {
            return Err(kitty::err("EFBIG", "Image too large"));
        }
        loop {
            let pane_ok = self.bytes + other + need <= cfg.pane_budget;
            if pane_ok && self.budget.as_ref().is_none_or(|b| b.try_reserve(need)) {
                self.bytes += need;
                return Ok(());
            }
            let victim = self
                .images
                .values()
                .filter(|i| !i.evicted && Some(i.key) != protect && i.bytes() > 0)
                .min_by_key(|i| (self.has_placements(i.key), i.last_used.load(Ordering::Relaxed)))
                .map(|i| (i.key, self.has_placements(i.key)));
            match victim {
                Some((key, false)) => self.remove_image(key),
                Some((key, true)) => self.evict(key),
                None => return Err(kitty::err("ENOSPC", "Image storage quota exceeded")),
            }
        }
    }

    pub(crate) fn release(&mut self, n: usize) {
        self.bytes = self.bytes.saturating_sub(n);
        if let Some(b) = &self.budget {
            b.release(n);
        }
    }

    /// Insert an image whose bytes were already reserved.
    pub(crate) fn insert(&mut self, image: Image) -> u64 {
        let key = image.key;
        if image.id != 0
            && let Some(old) = self.by_id.insert(image.id, key)
            && old != key
        {
            self.remove_image(old);
            self.by_id.insert(image.id, key);
        }
        self.images.insert(key, image);
        key
    }

    pub(crate) fn remove_image(&mut self, key: u64) {
        if let Some(img) = self.images.remove(&key) {
            if img.id != 0 && self.by_id.get(&img.id) == Some(&key) {
                self.by_id.remove(&img.id);
            }
            self.release(img.bytes());
        }
        self.remove_placements(|p| p.image == key);
    }

    fn evict(&mut self, key: u64) {
        let Some(img) = self.images.get_mut(&key) else { return };
        let n = img.bytes();
        img.evicted = true;
        img.frames.iter_mut().for_each(|f| f.pixels = Vec::new());
        img.generation += 1;
        self.release(n);
    }

    /// Add a placement; an existing one with the same image and non-zero id is replaced.
    pub(crate) fn add_placement(&mut self, mut p: Placement) {
        self.seq += 1;
        p.seq = self.seq;
        if p.id != 0
            && let Some(old) = self.placements.iter_mut().find(|o| o.image == p.image && o.id == p.id)
        {
            *old = p;
            return;
        }
        self.placements.push(p);
    }

    /// Placement `(key, id)`; id 0 matches the first placement of the image.
    pub(crate) fn find_placement(&self, key: u64, id: u32) -> Option<&Placement> {
        self.placements.iter().find(|p| p.image == key && (id == 0 || p.id == id))
    }

    /// Remove matching placements and every relative placement depending on
    /// them. Returns the image keys that lost placements.
    pub(crate) fn remove_placements(&mut self, mut pred: impl FnMut(&Placement) -> bool) -> Vec<u64> {
        let mut affected = Vec::new();
        self.placements.retain(|p| {
            let rm = pred(p);
            if rm {
                affected.push(p.image);
            }
            !rm
        });
        // Cascade to orphaned relative placements.
        loop {
            let alive = |k: u64, id: u32, ps: &[Placement]| ps.iter().any(|q| q.image == k && q.id == id);
            let orphans: Vec<usize> = (0..self.placements.len())
                .filter(|&i| self.placements[i].parent.is_some_and(|(k, id)| !alive(k, id, &self.placements)))
                .collect();
            if orphans.is_empty() {
                break;
            }
            for &i in orphans.iter().rev() {
                affected.push(self.placements.remove(i).image);
            }
        }
        affected.sort_unstable();
        affected.dedup();
        affected
    }

    /// Delete the given images if nothing displays them any more.
    pub(crate) fn free_unplaced(&mut self, keys: &[u64]) {
        for &k in keys {
            if !self.has_placements(k) {
                self.remove_image(k);
            }
        }
    }

    /// Drop anonymous images (Sixel, iTerm2, kitty id 0) nothing displays:
    /// they can never be shown again.
    pub(crate) fn free_anonymous(&mut self) {
        let keys: Vec<u64> =
            self.images.values().filter(|i| i.id == 0 && !self.has_placements(i.key)).map(|i| i.key).collect();
        for k in keys {
            self.remove_image(k);
        }
    }

    pub(crate) fn clear(&mut self) {
        let n = self.bytes;
        self.images.clear();
        self.by_id.clear();
        self.placements.clear();
        self.upload = None;
        self.release(n);
    }

    /// Absolute (line, col) of a placement, following relative chains through
    /// non-virtual parents. `virt` resolves virtual parents.
    fn resolve(
        &self,
        p: &Placement,
        depth: usize,
        virt: &dyn Fn(u64, u32) -> Option<(i64, i64)>,
    ) -> Option<(i64, i64)> {
        let Some((pk, pid)) = p.parent else { return Some((p.line, p.col as i64)) };
        if depth >= MAX_RELATIVE_DEPTH {
            return None;
        }
        let parent = self.find_placement(pk, pid)?;
        let (l, c) = if parent.is_virtual { virt(pk, parent.id)? } else { self.resolve(parent, depth + 1, virt)? };
        Some((l + p.rel.1 as i64, c + p.rel.0 as i64))
    }

    /// Depth of the relative chain above `(key, id)`, or `None` if `target`
    /// appears in it (cycle).
    pub(crate) fn chain_depth(&self, key: u64, id: u32, target: (u64, u32)) -> Option<usize> {
        let mut cur = self.find_placement(key, id)?;
        let mut depth = 1;
        loop {
            if (cur.image, cur.id) == target {
                return None;
            }
            match cur.parent {
                Some((k, i)) => {
                    cur = self.find_placement(k, i)?;
                    depth += 1;
                    if depth > MAX_RELATIVE_DEPTH + 1 {
                        return Some(depth);
                    }
                }
                None => return Some(depth),
            }
        }
    }

    /// Placements (non-virtual) whose cell box contains the absolute cell.
    pub(crate) fn intersects(&self, p: &Placement, line: Option<i64>, col: Option<i64>) -> bool {
        if p.is_virtual {
            return false;
        }
        let Some((l, c)) = self.resolve(p, 0, &|_, _| None) else { return false };
        line.is_none_or(|y| y >= l && y < l + p.rows as i64) && col.is_none_or(|x| x >= c && x < c + p.cols as i64)
    }

    /// Placements intersecting absolute lines `[a, b)`.
    pub(crate) fn intersects_lines(&self, p: &Placement, a: i64, b: i64) -> bool {
        if p.is_virtual {
            return false;
        }
        let Some((l, _)) = self.resolve(p, 0, &|_, _| None) else { return false };
        l < b && l + p.rows as i64 > a
    }

    /// Text in the region `[top, bottom)` moved by `delta` lines. Placements
    /// entirely inside the region move with it and are clipped to it.
    pub(crate) fn scroll_region(&mut self, top: i64, bottom: i64, delta: i64) {
        let mut gone = false;
        for p in &mut self.placements {
            if p.is_virtual || p.parent.is_some() {
                continue;
            }
            let (l, r) = (p.line, p.rows as i64);
            let inside = l >= top && l + r <= bottom;
            if !inside && p.clip != Some((top, bottom)) {
                continue;
            }
            p.line = l + delta;
            if p.line + r <= top || p.line >= bottom {
                p.rows = 0;
                gone = true;
            } else if p.line < top || p.line + r > bottom {
                p.clip = Some((top, bottom));
            } else {
                p.clip = None;
            }
        }
        if gone {
            self.remove_placements(|p| p.rows == 0);
            self.free_anonymous();
        }
    }

    /// Move every anchored placement by `delta` lines (alternate screen resize).
    pub(crate) fn shift(&mut self, delta: i64) {
        for p in &mut self.placements {
            if !p.is_virtual && p.parent.is_none() {
                p.line += delta;
                p.clip = None;
            }
        }
    }

    /// Follow a reflow: anchors move with their line; columns are clamped.
    pub(crate) fn remap(&mut self, map: &LineMap, cols: usize) {
        let mut gone = false;
        for p in &mut self.placements {
            if p.is_virtual || p.parent.is_some() {
                continue;
            }
            p.clip = None;
            match u64::try_from(p.line).ok().and_then(|l| map.map(l)) {
                Some(l) => {
                    p.line = l as i64;
                    p.col = p.col.min(cols.saturating_sub(1));
                }
                None => {
                    p.rows = 0;
                    gone = true;
                }
            }
        }
        if gone {
            self.remove_placements(|p| p.rows == 0 && !p.is_virtual);
            self.free_anonymous();
        }
    }

    /// Drop placements entirely above absolute line `first` (fell out of scrollback).
    pub(crate) fn prune(&mut self, first: i64) {
        let gone = |p: &Placement| !p.is_virtual && p.parent.is_none() && p.line + p.rows as i64 <= first;
        if self.placements.iter().any(gone) {
            self.remove_placements(gone);
            self.free_anonymous();
        }
    }

    /// Remove anonymous (Sixel/iTerm2) placements fully covered by a new image
    /// so that redrawing at the same spot does not pile images up.
    pub(crate) fn remove_covered(&mut self, line: i64, col: usize, cols: u32, rows: u32) {
        let images = &self.images;
        let covered = |p: &Placement| {
            !p.is_virtual
                && p.parent.is_none()
                && images.get(&p.image).is_some_and(|i| i.source != Source::Kitty)
                && p.line >= line
                && p.line + p.rows as i64 <= line + rows as i64
                && p.col >= col
                && p.col + p.cols as usize <= col + cols as usize
        };
        let doomed: Vec<u64> = self.placements.iter().filter(|p| covered(p)).map(|p| p.seq).collect();
        if !doomed.is_empty() {
            self.remove_placements(|p| doomed.contains(&p.seq));
            self.free_anonymous();
        }
    }

    /// Everything to draw for the viewport of `grid`, bottom-most first.
    pub(crate) fn views<'a>(&'a self, grid: &Grid, styles: &StyleTable, cell: (u32, u32)) -> Vec<ImageView<'a>> {
        let (cw, ch) = (cell.0 as i64, cell.1 as i64);
        let lines = grid.lines() as i64;
        let top = grid.viewport_line(0) as i64;
        let mut out: Vec<(ImageView<'a>, u64)> = Vec::new();
        // Origin (absolute line, col) of each virtual placement shown on screen.
        let mut origins: HashMap<(u64, u32), (i64, i64)> = HashMap::new();

        for y in 0..grid.lines() {
            for run in placeholder::runs(grid.viewport_row(y), styles) {
                let Some(&key) = self.by_id.get(&run.image_id) else { continue };
                let Some(vp) = self
                    .placements
                    .iter()
                    .find(|p| p.image == key && p.is_virtual && (run.placement_id == 0 || p.id == run.placement_id))
                else {
                    continue;
                };
                let Some(img) = self.images.get(&key) else { continue };
                let origin = (top + y as i64 - run.row as i64, run.x as i64 - run.col as i64);
                origins
                    .entry((key, vp.id))
                    .and_modify(|o| *o = (o.0.min(origin.0), o.1.min(origin.1)))
                    .or_insert(origin);
                let (ix, iy, iw, ih) =
                    contain(vp.src.w as i64, vp.src.h as i64, vp.cols as i64 * cw, vp.rows as i64 * ch);
                let (rx0, ry0) = (run.col as i64 * cw, run.row as i64 * ch);
                let (rx1, ry1) = (rx0 + run.len as i64 * cw, ry0 + ch);
                let (ax0, ay0, ax1, ay1) = (rx0.max(ix), ry0.max(iy), rx1.min(ix + iw), ry1.min(iy + ih));
                if ax0 >= ax1 || ay0 >= ay1 {
                    continue;
                }
                let (sx, sy) = (vp.src.w as f32 / iw as f32, vp.src.h as f32 / ih as f32);
                let view = ImageView {
                    key,
                    id: img.id,
                    placement: vp.id,
                    generation: img.generation,
                    width: img.width,
                    height: img.height,
                    pixels: img.pixels(),
                    loading: img.loading,
                    src: SrcRect {
                        x: vp.src.x as f32 + (ax0 - ix) as f32 * sx,
                        y: vp.src.y as f32 + (ay0 - iy) as f32 * sy,
                        w: (ax1 - ax0) as f32 * sx,
                        h: (ay1 - ay0) as f32 * sy,
                    },
                    dest: DestRect {
                        x: run.x as i64 * cw + (ax0 - rx0),
                        y: y as i64 * ch + (ay0 - ry0),
                        w: ax1 - ax0,
                        h: ay1 - ay0,
                    },
                    z: vp.z,
                    from_placeholder: true,
                };
                out.push((view, vp.seq));
            }
        }

        let virt = |k: u64, id: u32| origins.get(&(k, id)).copied();
        for p in self.placements.iter().filter(|p| !p.is_virtual) {
            let Some(img) = self.images.get(&p.image) else { continue };
            let Some((line, col)) = self.resolve(p, 0, &virt) else { continue };
            let (rx, ry, rw, rh) = placement_rect(p, cw, ch);
            let mut view = ImageView {
                key: p.image,
                id: img.id,
                placement: p.id,
                generation: img.generation,
                width: img.width,
                height: img.height,
                pixels: img.pixels(),
                loading: img.loading,
                src: SrcRect { x: p.src.x as f32, y: p.src.y as f32, w: p.src.w as f32, h: p.src.h as f32 },
                dest: DestRect { x: col * cw + rx, y: (line - top) * ch + ry, w: rw, h: rh },
                z: p.z,
                from_placeholder: false,
            };
            if let Some((lo, hi)) = p.clip {
                match view.clip(i64::MIN / 4, (lo - top) * ch, i64::MAX / 4, (hi - top) * ch) {
                    Some(v) => view = v,
                    None => continue,
                }
            }
            let d = view.dest;
            if d.w <= 0 || d.h <= 0 || d.y + d.h <= 0 || d.y >= lines * ch || d.x + d.w <= 0 {
                continue;
            }
            out.push((view, p.seq));
        }

        out.sort_by_key(|(v, seq)| (v.z, v.id, *seq));
        for (v, _) in &out {
            if let Some(img) = self.images.get(&v.key) {
                img.touch();
            }
        }
        out.into_iter().map(|(v, _)| v).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(id: u32, w: u32, h: u32) -> Image {
        Image::new(id, 0, Source::Kitty, w, h, vec![0; (w * h * 4) as usize])
    }

    #[test]
    fn quota_prefers_unplaced_then_evicts() {
        let cfg = GraphicsConfig { pane_budget: 3 * 400, ..GraphicsConfig::default() };
        let mut s = ImageStore::new(None);
        let mut keys = Vec::new();
        for id in 1..=3 {
            s.reserve(400, 0, &cfg, None).unwrap();
            keys.push(s.insert(img(id, 10, 10)));
        }
        let full = PxRect { x: 0, y: 0, w: 10, h: 10 };
        s.add_placement(Placement::new(keys[0], 0, 0, full, 1, 1, Fit::Natural));
        s.add_placement(Placement::new(keys[2], 0, 0, full, 1, 1, Fit::Natural));
        // Image 2 has no placement: deleted first.
        s.reserve(400, 0, &cfg, None).unwrap();
        assert!(s.image(keys[1]).is_none());
        // Then the least recently used placed image loses its pixels.
        s.reserve(400, 0, &cfg, None).unwrap();
        assert!(s.image(keys[0]).unwrap().is_evicted());
        assert!(!s.image(keys[2]).unwrap().is_evicted());
        assert_eq!(s.bytes(), 3 * 400);
        assert!(s.reserve(cfg.max_image_bytes + 1, 0, &cfg, None).is_err());
    }

    #[test]
    fn window_budget_is_shared_and_released() {
        let budget = ImageBudget::new(1000);
        let cfg = GraphicsConfig { window_budget: Some(budget.clone()), ..GraphicsConfig::default() };
        let mut a = ImageStore::new(cfg.window_budget.clone());
        let mut b = ImageStore::new(cfg.window_budget.clone());
        a.reserve(800, 0, &cfg, None).unwrap();
        a.insert(img(1, 10, 20));
        assert!(b.reserve(400, 0, &cfg, None).is_err());
        drop(a);
        assert_eq!(budget.used(), 0);
        b.reserve(400, 0, &cfg, None).unwrap();
        assert_eq!(budget.used(), 400);
    }

    #[test]
    fn region_scroll_moves_and_clips() {
        let mut s = ImageStore::new(None);
        let key = s.insert(img(1, 10, 10));
        let r = PxRect { x: 0, y: 0, w: 10, h: 10 };
        s.add_placement(Placement::new(key, 2, 0, r, 1, 2, Fit::Natural));
        s.scroll_region(1, 5, -1);
        assert_eq!(s.placements()[0].line, 1);
        assert_eq!(s.placements()[0].clip, None);
        s.scroll_region(1, 5, -1);
        assert_eq!(s.placements()[0].clip, Some((1, 5)));
        s.scroll_region(1, 5, -1);
        assert!(s.placements().is_empty());
    }

    #[test]
    fn view_clip_scales_source() {
        let v = ImageView {
            key: 1,
            id: 0,
            placement: 0,
            generation: 1,
            width: 20,
            height: 20,
            pixels: None,
            loading: false,
            src: SrcRect { x: 0.0, y: 0.0, w: 20.0, h: 20.0 },
            dest: DestRect { x: -5, y: 0, w: 10, h: 10 },
            z: 0,
            from_placeholder: false,
        };
        let c = v.clip(0, 0, 100, 100).unwrap();
        assert_eq!(c.dest, DestRect { x: 0, y: 0, w: 5, h: 10 });
        assert_eq!(c.src, SrcRect { x: 10.0, y: 0.0, w: 10.0, h: 20.0 });
    }
}
